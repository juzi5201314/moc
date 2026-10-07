//! 运行时配置加载、合并与解析。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use moc_llm::LlmConfig;
use toml::Value;

use crate::CoreConfig;
use crate::error::CoreError;
use model::{ModelRoles, ResolvedModel, all_models, resolve_catalog, resolve_one, validate_roles};
use schema::{ConfigDocument, validate_compaction, validate_skills};

pub mod catalog;
pub mod discovery;
pub mod editor;
pub mod model;
mod schema;

pub use model::{ModelCapabilities, ThinkingLevel};
pub use schema::{
    BashConfig, CompactionConfig, CompactionMethod, CompactionThreshold, ContextFilesConfig,
    EditConfig, FeatureConfig, FindConfig, GrepConfig, JobConfig, JobOverflow, LimitsConfig,
    OutputConfig, ReadConfig, RemoteCompactionConfig, RetentionConfig, ShakeConfig, SkillsConfig,
    SourcesConfig, SystemPromptConfig, ToolsConfig, WorkspaceConfig, WriteConfig,
};

/// 会话文件与保留策略；跨进程锁始终启用。
#[derive(Debug, Clone)]
pub struct SessionSettings {
    pub dir: PathBuf,
    pub retention: RetentionConfig,
}

/// 一次完成路径合并、密钥解析与 provider/model 校验的运行时配置。
#[derive(Clone)]
pub struct ResolvedConfig {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub roles: ModelRoles,
    pub sources: SourcesConfig,
    pub tools: ToolsConfig,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub system_prompt: SystemPromptConfig,
    pub context_files: ContextFilesConfig,
    pub workspace: WorkspaceConfig,
    pub openviking: moc_openviking::OpenVikingConfig,
    pub session: SessionSettings,
    pub mcp: moc_mcp::McpDocument,
    pub skills: SkillsConfig,
    providers: BTreeMap<String, model::ResolvedProvider>,
    base_raw: Value,
    overlays: Vec<PathBuf>,
}

impl ResolvedConfig {
    /// 读取内置默认、全局配置与命令行覆盖层，不加载项目配置。
    pub fn load(config: &CoreConfig) -> Result<Self, CoreError> {
        let home = expand_home_path(&config.home);
        let mut base_raw: Value = toml::from_str(include_str!("defaults.toml"))
            .map_err(|error| CoreError::Config(format!("内置默认配置无效: {error}")))?;
        merge_optional_file(&mut base_raw, &home.join("config.toml"))?;
        let mut raw = base_raw.clone();
        for overlay in &config.overlays {
            merge_required_file(&mut raw, overlay)?;
        }
        Self::from_merged(
            raw,
            base_raw,
            home,
            expand_home_path(&config.cwd),
            config.overlays.clone(),
        )
    }

    /// 以项目配置覆盖全局配置，再按原顺序合并 CLI 覆盖层。
    pub fn for_project(&self, cwd: impl AsRef<Path>) -> Result<Self, CoreError> {
        let cwd = expand_home_path(cwd.as_ref());
        let mut raw = self.base_raw.clone();
        if self.sources.project_config {
            merge_optional_file(&mut raw, &cwd.join(".moc").join("config.toml"))?;
        }
        for overlay in &self.overlays {
            merge_required_file(&mut raw, overlay)?;
        }
        Self::from_merged(
            raw,
            self.base_raw.clone(),
            self.home.clone(),
            cwd,
            self.overlays.clone(),
        )
    }

    /// 按 provider/model[:level] 或模型角色解析可调用模型。
    pub fn resolve_model(&self, selector: &str) -> Result<ResolvedModel, CoreError> {
        let selector = match selector {
            "default" => self.roles.default.as_str(),
            "smol" => self.roles.smol.as_str(),
            "slow" => self.roles.slow.as_str(),
            "plan" => self.roles.plan.as_str(),
            other => other,
        };
        let selector = if selector == "inherit" {
            self.roles.default.as_str()
        } else {
            selector
        };
        if selector.is_empty() {
            return Err(CoreError::Config(
                "尚未配置主模型，请打开 /config 添加供应商并选择至少一个模型".into(),
            ));
        }
        resolve_one(&self.providers, selector, None)
    }

    /// 返回模型角色对应的已解析模型。
    pub fn resolve_role(&self, role: &str) -> Result<ResolvedModel, CoreError> {
        self.resolve_model(role)
    }

    /// 列出 provider 配置中的全部模型，顺序按 provider 与模型 ID 稳定排序。
    pub fn available_models(&self) -> Vec<ResolvedModel> {
        all_models(&self.providers)
    }

    /// 返回指定模型的协议配置；输出上限由模型自身配置决定。
    pub fn llm_config(&self, model: &ResolvedModel) -> LlmConfig {
        model.llm.clone()
    }

    fn from_merged(
        mut raw: Value,
        base_raw: Value,
        home: PathBuf,
        cwd: PathBuf,
        overlays: Vec<PathBuf>,
    ) -> Result<Self, CoreError> {
        apply_model_defaults(&mut raw)?;
        let document: ConfigDocument = raw
            .try_into()
            .map_err(|error| CoreError::Config(format!("配置字段无效: {error}")))?;
        let providers = resolve_catalog(&document.providers)?;
        validate_roles(&document.models.roles, &providers)?;
        validate_compaction(&document.compaction)?;
        validate_skills(&document.skills)?;
        schema::validate_ptc(&document.tools.ptc)?;
        if document.tools.job.max_running == 0 {
            return Err(CoreError::Config("后台进程上限必须大于 0".into()));
        }
        let session_dir = expand_config_path(&document.session.dir, &home)?;
        Ok(Self {
            home,
            cwd,
            roles: document.models.roles,
            sources: document.sources,
            tools: document.tools,
            compaction: document.compaction,
            limits: document.limits,
            system_prompt: document.system_prompt,
            context_files: document.context_files,
            workspace: document.workspace,
            openviking: document.openviking,

            session: SessionSettings {
                dir: session_dir,
                retention: document.session.retention,
            },
            mcp: document.mcp,
            skills: document.skills,
            providers,
            base_raw,
            overlays,
        })
    }

    /// 汇总该项目的全部 MCP server 配置：TOML `[mcp.servers]` 与社区
    /// `.mcp.json`（cwd 向上到 git root）、`~/.moc/mcp.json` 合并。
    pub fn mcp_servers(&self) -> Result<Vec<moc_mcp::ServerConfig>, CoreError> {
        if !self.mcp.enabled {
            return Ok(Vec::new());
        }
        let project_files = if self.mcp.project_config {
            moc_mcp::discover_project_json(
                &self.cwd,
                crate::system_prompt::context_files::git_root(&self.cwd).as_deref(),
            )
        } else {
            Vec::new()
        }
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).unwrap_or_default();
            let servers = moc_mcp::parse_mcp_json(&text)
                .map_err(|error| CoreError::Config(format!("解析 {}: {error}", path.display())));
            servers.map(|servers| (path, servers))
        })
        .collect::<Result<Vec<_>, CoreError>>()?;
        let user_file = {
            let path = moc_mcp::user_json_path(&self.home);
            if self.mcp.user_config && path.is_file() {
                let text = fs::read_to_string(&path).map_err(|error| {
                    CoreError::Config(format!("读取 {}: {error}", path.display()))
                })?;
                let servers = moc_mcp::parse_mcp_json(&text).map_err(|error| {
                    CoreError::Config(format!("解析 {}: {error}", path.display()))
                })?;
                Some((path, servers))
            } else {
                None
            }
        };
        let source = moc_mcp::JsonSource {
            project_files,
            user_file,
        };
        moc_mcp::resolve_all(&self.mcp, &source)
            .map_err(|error| CoreError::Config(error.to_string()))
    }
}

/// 解析配置中的所有 provider/model，供 CLI 列表与配置检查使用。
pub fn resolve_models(config: &CoreConfig) -> anyhow::Result<Vec<ResolvedModel>> {
    Ok(ResolvedConfig::load(config)?.available_models())
}

/// 展开 `~` 前缀的用户路径（`[skills] dirs` 等扩展配置使用）。
pub fn expand_user_path(value: &str) -> PathBuf {
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        && let Some(home) = crate::paths::home_dir()
    {
        return home.join(rest);
    }
    if value == "~"
        && let Some(home) = crate::paths::home_dir()
    {
        return home;
    }
    PathBuf::from(value)
}

fn expand_home_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return crate::paths::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\"))
        && let Some(home) = crate::paths::home_dir()
    {
        return home.join(rest);
    }
    path.to_owned()
}

fn expand_config_path(value: &str, moc_home: &Path) -> Result<PathBuf, CoreError> {
    if matches!(value, "~/.moc" | "~\\.moc") {
        return Ok(moc_home.to_owned());
    }
    if let Some(rest) = value
        .strip_prefix("~/.moc/")
        .or_else(|| value.strip_prefix("~\\.moc\\"))
    {
        return Ok(moc_home.join(rest));
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        let home = crate::paths::home_dir()
            .ok_or_else(|| CoreError::Config("无法展开配置路径：用户主目录未设置".into()))?;
        return Ok(home.join(rest));
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        moc_home.join(path)
    })
}

fn merge_optional_file(target: &mut Value, path: &Path) -> Result<(), CoreError> {
    if path.exists() {
        merge_required_file(target, path)?;
    }
    Ok(())
}

fn merge_required_file(target: &mut Value, path: &Path) -> Result<(), CoreError> {
    let text = fs::read_to_string(path)
        .map_err(|error| CoreError::Config(format!("读取配置 {} 失败: {error}", path.display())))?;
    let overlay: Value = toml::from_str(&text)
        .map_err(|error| CoreError::Config(format!("解析配置 {} 失败: {error}", path.display())))?;
    merge_value(target, overlay);
    Ok(())
}

fn merge_value(target: &mut Value, overlay: Value) {
    match (target, overlay) {
        (Value::Table(target), Value::Table(overlay)) => {
            for (key, value) in overlay {
                match target.get_mut(&key) {
                    Some(current) => merge_value(current, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, overlay) => *target = overlay,
    }
}

fn apply_model_defaults(raw: &mut Value) -> Result<(), CoreError> {
    let Some(defaults) = raw.get("defaults") else {
        return Ok(());
    };
    let defaults_models = defaults
        .get("models")
        .and_then(Value::as_table)
        .cloned()
        .ok_or_else(|| CoreError::Config("defaults.models 必须是表".into()))?;
    for pattern in defaults_models.keys() {
        GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|error| {
                CoreError::Config(format!(
                    "defaults.models.{pattern} 不是合法模型匹配模式：{error}"
                ))
            })?;
    }

    let providers = raw
        .get_mut("providers")
        .and_then(Value::as_table_mut)
        .ok_or_else(|| CoreError::Config("providers 必须是表".into()))?;
    for (_, provider) in providers.iter_mut() {
        let Some(models) = provider.get_mut("models").and_then(Value::as_table_mut) else {
            continue;
        };
        for (model_id, explicit) in models.iter_mut() {
            let mut matching = defaults_models
                .iter()
                .filter_map(|(pattern, value)| {
                    let glob = GlobBuilder::new(pattern)
                        .literal_separator(false)
                        .build()
                        .expect("defaults.models 匹配模式已在上方校验");
                    glob.compile_matcher().is_match(model_id).then_some((
                        !pattern.contains(['*', '?', '[', '{']),
                        pattern.len(),
                        pattern.as_str(),
                        value,
                    ))
                })
                .collect::<Vec<_>>();
            matching.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then(left.1.cmp(&right.1))
                    .then(left.2.cmp(right.2))
            });
            let mut merged = Value::Table(Default::default());
            for (_, _, _, value) in matching {
                merge_value(&mut merged, value.clone());
            }
            merge_value(&mut merged, explicit.clone());
            *explicit = merged;
        }
    }
    raw.as_table_mut()
        .expect("配置根节点必须是表")
        .remove("defaults");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn core_config(home: &Path, cwd: &Path, overlays: Vec<PathBuf>) -> CoreConfig {
        CoreConfig {
            home: home.to_owned(),
            cwd: cwd.to_owned(),
            overlays,
            session_dir: None,
            client: moc_llm::Client::new(),
        }
    }

    fn provider_config(api_key: &str) -> String {
        format!(
            "[models.roles]\ndefault='p/model:high'\nsmol='p/model:low'\nslow='p/model:max'\nplan='p/model:xhigh'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\napi_key='{api_key}'\n[providers.p.models.model]\nreasoning=true\ninput=['text','image']\n"
        )
    }

    #[test]
    fn merges_global_project_and_cli_tables_in_precedence_order() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        let overlay = dir.path().join("cli.toml");
        fs::create_dir_all(&home).expect("创建 home");
        fs::create_dir_all(project.join(".moc")).expect("创建项目配置目录");
        fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[tools.read]\ndefault_limit=400\nmax_bytes=40000\n[tools.bash]\nallow_patterns=['^python3\\s+build\\.py$']\n",
                provider_config("literal")
            ),
        )
        .expect("写全局配置");
        fs::write(
            project.join(".moc/config.toml"),
            "[tools.read]\ndefault_limit=600\nline_max_bytes=12345\n",
        )
        .expect("写项目配置");
        fs::write(
            &overlay,
            "[tools.read]\ndefault_limit=700\nmax_bytes=8192\n",
        )
        .expect("写 CLI 覆盖层");

        let config = ResolvedConfig::load(&core_config(&home, &project, vec![overlay]))
            .expect("全局与 CLI 配置可解析")
            .for_project(&project)
            .expect("项目配置可合并");

        assert_eq!(
            config.tools.bash.allow_patterns,
            vec![r"^python3\s+build\.py$".to_owned()]
        );
        assert_eq!(
            config
                .resolve_role("default")
                .expect("default 模型")
                .thinking_level,
            ThinkingLevel::High
        );
        assert_eq!(
            config
                .resolve_role("slow")
                .expect("slow 模型")
                .thinking_level,
            ThinkingLevel::Max
        );
    }

    #[test]
    fn resolves_literal_environment_and_command_secrets_without_leaking_them() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        let (home_variable, home_secret) = std::env::var("HOME")
            .map(|value| ("HOME", value))
            .or_else(|_| std::env::var("USERPROFILE").map(|value| ("USERPROFILE", value)))
            .expect("测试环境提供用户主目录变量");
        let environment_secret = format!("env:{home_variable}");
        for (secret, expected) in [
            ("literal-token", "literal-token".to_owned()),
            (environment_secret.as_str(), home_secret),
            ("!echo command-token", "command-token".to_owned()),
        ] {
            fs::write(home.join("config.toml"), provider_config(secret)).expect("写配置");
            let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
                .expect("密钥可解析");
            let model = config.resolve_role("default").expect("模型可解析");
            assert_eq!(model.llm.auth, moc_llm::Auth::Bearer(expected.clone()));
            assert!(!format!("{:?}", model.llm.auth).contains(&expected));
        }
    }

    #[test]
    fn rejects_unknown_levels_missing_environment_and_invalid_toml() {
        assert!("ultra".parse::<ThinkingLevel>().is_err());
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            provider_config("env:MOC_TEST_MISSING_KEY_7261"),
        )
        .expect("写配置");
        assert!(ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err());
        fs::write(home.join("config.toml"), "[providers.p\ninvalid").expect("写非法 TOML");
        assert!(ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err());
    }

    #[test]
    fn wildcard_model_defaults_merge_before_provider_overrides() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            r#"
[models.roles]
default = "p/ag/gpt-5.6-sol:high"
smol = "p/ag/gpt-5.6-sol:high"
slow = "p/ag/gpt-5.6-sol:max"
plan = "p/ag/gpt-5.6-sol:max"

[defaults.models."*gpt-5.6-sol"]
context_window = 500000
max_output_tokens = 384000
reasoning = true
thinking_levels = ["high", "max"]
input = ["text", "image"]

[defaults.models."*gpt-5.6-sol".compat]
developer_role = true

[providers.p]
api = "openai_responses"
base_url = "https://example.invalid"

[providers.p.models."ag/gpt-5.6-sol"]
max_output_tokens = 128000
"#,
        )
        .expect("写配置");

        let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
            .expect("通配模型默认值可解析");
        let model = config.resolve_role("default").expect("解析模型");
        assert_eq!(model.context_window, 500_000);
        assert_eq!(model.max_output_tokens, 128_000);
        assert!(model.capabilities.reasoning);
        assert!(model.capabilities.image);
        assert_eq!(
            model.thinking_levels,
            vec![ThinkingLevel::High, ThinkingLevel::Max]
        );
        assert!(model.llm.wire.developer_role);
        assert!(model.remote_compaction);
    }

    #[test]
    fn reasoning_defaults_to_standard_effort_levels() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            "[models.roles]\ndefault='p/m:max'\nsmol='p/m:low'\nslow='p/m:high'\nplan='p/m:max'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\nreasoning=true\n",
        )
        .expect("写配置");
        let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
            .expect("默认思考等级可解析");
        assert_eq!(
            config
                .resolve_role("default")
                .expect("解析模型")
                .thinking_levels,
            vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
                ThinkingLevel::Max,
            ]
        );
    }
}
