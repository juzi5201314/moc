//! 三层 TOML 配置合并与解析后的运行时设置。

pub mod model;
mod schema;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use moc_llm::LlmConfig;
use toml::Value;

use crate::CoreConfig;
use crate::error::CoreError;
use model::{ModelRoles, ResolvedModel, all_models, resolve_catalog, resolve_one, validate_roles};
use schema::{ConfigDocument, validate_compaction};

pub use model::{ModelCapabilities, ThinkingLevel};
pub use schema::{
    ApprovalMode, BashConfig, CompactionConfig, CompactionMethod, ContextFilesConfig, EditConfig,
    FeatureConfig, FindConfig, GrepConfig, LimitsConfig, ReadConfig, RemoteCompactionConfig,
    ShakeConfig, SystemPromptConfig, ToolApproval, ToolsConfig, WorkspaceConfig, WriteConfig,
};

/// 会话文件与跨进程锁设置。
#[derive(Debug, Clone)]
pub struct SessionSettings {
    pub dir: PathBuf,
    pub locking: bool,
}

/// 一次完成路径合并、密钥解析与 provider/model 校验的运行时配置。
#[derive(Clone)]
pub struct ResolvedConfig {
    /// core 配置与数据根目录。
    pub home: PathBuf,
    /// 当前配置所对应的工作目录。
    pub cwd: PathBuf,
    /// 模型角色 selector。
    pub roles: ModelRoles,
    /// 工具策略与限制。
    pub tools: ToolsConfig,
    /// 上下文压缩设置。
    pub compaction: CompactionConfig,
    /// 轮次资源上限。
    pub limits: LimitsConfig,
    /// 系统提示词覆盖。
    pub system_prompt: SystemPromptConfig,
    /// 上下文文件发现设置。
    pub context_files: ContextFilesConfig,
    /// 工作区监视设置。
    pub workspace: WorkspaceConfig,
    /// 会话数据设置。
    pub session: SessionSettings,
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
        merge_optional_file(&mut raw, &cwd.join(".moc").join("config.toml"))?;
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
        let role_selector = match selector {
            "default" => Some(self.roles.default.as_str()),
            "smol" => Some(self.roles.smol.as_str()),
            "slow" => Some(self.roles.slow.as_str()),
            "plan" => Some(
                self.roles
                    .plan
                    .as_deref()
                    .filter(|selector| !selector.is_empty())
                    .unwrap_or(&self.roles.slow),
            ),
            _ => None,
        };
        let selector = role_selector.unwrap_or(selector);
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

    /// 将模型最大输出限制到 core 全局上限。
    pub fn llm_config(&self, model: &ResolvedModel) -> LlmConfig {
        model.llm.clone()
    }

    fn from_merged(
        raw: Value,
        base_raw: Value,
        home: PathBuf,
        cwd: PathBuf,
        overlays: Vec<PathBuf>,
    ) -> Result<Self, CoreError> {
        let document: ConfigDocument = raw
            .clone()
            .try_into()
            .map_err(|error| CoreError::Config(format!("配置字段无效: {error}")))?;
        let providers = resolve_catalog(&document.providers)?;
        validate_roles(&document.models.roles, &providers)?;
        validate_compaction(&document.compaction)?;
        let session_dir = expand_config_path(&document.session.dir, &home)?;
        Ok(Self {
            home,
            cwd,
            roles: document.models.roles,
            tools: document.tools,
            compaction: document.compaction,
            limits: document.limits,
            system_prompt: document.system_prompt,
            context_files: document.context_files,
            workspace: document.workspace,
            session: SessionSettings {
                dir: session_dir,
                locking: document.session.locking,
            },
            providers,
            base_raw,
            overlays,
        })
    }
}

/// 解析配置中的所有 provider/model，供 CLI 列表与配置检查使用。
pub fn resolve_models(config: &CoreConfig) -> anyhow::Result<Vec<ResolvedModel>> {
    Ok(ResolvedConfig::load(config)?.available_models())
}

fn expand_home_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_owned()
}

fn expand_config_path(value: &str, moc_home: &Path) -> Result<PathBuf, CoreError> {
    if value == "~/.moc" {
        return Ok(moc_home.to_owned());
    }
    if let Some(rest) = value.strip_prefix("~/.moc/") {
        return Ok(moc_home.join(rest));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| CoreError::Config("无法展开配置路径：HOME 未设置".into()))?;
        return Ok(PathBuf::from(home).join(rest));
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
                "{}\n[tools.read]\ndefault_limit=400\nmax_bytes=40000\n",
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

        assert_eq!(config.tools.read.default_limit, 700);
        assert_eq!(config.tools.read.max_bytes, 8192);
        assert_eq!(config.tools.read.line_max_bytes, 12345);
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
        let home_secret = std::env::var("HOME").expect("测试环境提供 HOME");
        for (secret, expected) in [
            ("literal-token", "literal-token".to_owned()),
            ("env:HOME", home_secret),
            ("!printf command-token", "command-token".to_owned()),
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
}
