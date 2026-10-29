//! 运行时配置加载、合并与解析。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use moc_llm::LlmConfig;
use moc_protocol::SessionPermission;
use toml::Value;

use crate::CoreConfig;
use crate::error::CoreError;
use model::{ModelRoles, ResolvedModel, all_models, resolve_catalog, resolve_one, validate_roles};
use schema::{
    ConfigDocument, SandboxConfig, validate_compaction, validate_plugins, validate_skills,
};

pub mod catalog;
pub mod discovery;
pub mod editor;
pub mod model;
mod schema;

pub use model::{InstructionRole, ModelCapabilities, ROLE_INHERIT, ROLE_NONE, ThinkingLevel};
pub use schema::{
    BashConfig, CompactionConfig, CompactionMethod, CompactionThreshold, ContextFilesConfig,
    EditConfig, FindConfig, GrepConfig, HOST_PLUGIN_KEYS, JobConfig, JobOverflow, LimitsConfig,
    OnDenied, OutputConfig, PluginsConfig, PtcConfig, ReadConfig, RemoteCompactionConfig,
    RetentionConfig, ShakeConfig, SkillsConfig, SoftMethod, SystemPromptConfig, TodoConfig,
    ToolsConfig, WebFetchConfig, WebSearchConfig, WorkspaceConfig, WriteConfig, is_plugin_key,
};

/// 会话文件与保留策略；跨进程锁始终启用。
#[derive(Debug, Clone)]
pub struct SessionSettings {
    pub dir: PathBuf,
    pub compress: bool,
    pub retention: RetentionConfig,
}

/// web search 供应商的运行时配置；api_key 已按 env:/!command 语法解析。
///
/// 与 `model::ResolvedProvider` 一致不派生 `Debug`：结构持有已解析密钥，`{:?}` 会把密钥带进日志。
#[derive(Clone)]
pub struct ResolvedWebSearch {
    pub enabled: bool,
    /// 校验阶段已归一化为小写。
    pub provider: String,
    /// 原样保存；空串 = Tavily 官方地址（规范化在 websearch 模块做）。
    pub base_url: String,
    pub timeout_secs: u64,
    pub api_key: Option<String>,
}

impl ResolvedWebSearch {
    /// `web_search` 是否对模型可见：启用且已解析出密钥。
    ///
    /// 工具定义过滤、执行门槛与系统提示词共用同一判定，保持单点。
    pub fn available(&self) -> bool {
        self.enabled && self.api_key.is_some()
    }
}

/// web_fetch 本地抓取的运行时配置。
#[derive(Debug, Clone)]
pub struct ResolvedWebFetch {
    pub enabled: bool,
    pub timeout_secs: u64,
    pub max_bytes: usize,
    pub max_chars: usize,
}

/// 已展开、去重并校验过的沙盒配置。
#[derive(Debug, Clone)]
pub struct ResolvedSandboxConfig {
    pub default_mode: SessionPermission,
    pub network: bool,
    pub on_denied: OnDenied,
    pub allow_unsandboxed: bool,
    pub overlay_home: bool,
    pub extra_read_paths: Vec<PathBuf>,
    pub extra_write_paths: Vec<PathBuf>,
    pub deny_read_paths: Vec<PathBuf>,
    pub deny_write_paths: Vec<PathBuf>,
}

/// 一次完成路径合并、密钥解析与 provider/model 校验的运行时配置。
#[derive(Clone)]
pub struct ResolvedConfig {
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub roles: ModelRoles,
    pub tools: ToolsConfig,
    pub sandbox: ResolvedSandboxConfig,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub system_prompt: SystemPromptConfig,
    pub context_files: ContextFilesConfig,
    pub workspace: WorkspaceConfig,
    pub session: SessionSettings,
    pub mcp: moc_mcp::McpDocument,
    pub skills: SkillsConfig,
    pub plugins: PluginsConfig,
    pub web_search: ResolvedWebSearch,
    pub web_fetch: ResolvedWebFetch,
    pub agents: schema::AgentsConfig,
    providers: BTreeMap<String, model::ResolvedProvider>,
    base_raw: Value,
    overlays: Vec<PathBuf>,
}

impl ResolvedConfig {
    /// 读取内置默认、全局配置与命令行覆盖层。
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
    ///
    /// 项目级 `.moc/config.toml` 始终参与合并；项目配置中的 `!命令` 仍按统一配置
    /// 解析规则执行，使用项目配置前应确认仓库可信。
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
        let selector = match selector {
            "default" => self.roles.default.as_str(),
            "explore" => self.roles.explore.as_str(),
            "plan" => self.roles.plan.as_str(),
            other => other,
        };
        let selector = if selector == ROLE_INHERIT {
            self.roles.default.as_str()
        } else {
            selector
        };
        if selector == ROLE_NONE {
            return Err(CoreError::Config(
                "none 只用于计划模型，表示进入计划模式时不切换模型".into(),
            ));
        }
        if selector.is_empty() {
            return Err(CoreError::Config(
                "尚未配置主模型，请打开 /config 添加供应商并选择至少一个模型".into(),
            ));
        }
        resolve_one(&self.providers, selector, None)
    }

    /// 进入计划模式时应用的模型；`plan = "none"` 表示沿用会话当前模型。
    pub fn resolve_plan_mode_model(&self) -> Result<Option<ResolvedModel>, CoreError> {
        if self.roles.plan == ROLE_NONE {
            return Ok(None);
        }
        self.resolve_model("plan").map(Some)
    }

    /// 返回模型角色对应的已解析模型。
    ///
    /// 计划模型可以配置为 `none`（进入计划模式不切换模型），此时返回配置错误；
    /// 需要区分这种情况时改用 [`Self::resolve_plan_mode_model`]。
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
        remove_deprecated_retention_keys(&mut raw);
        apply_model_defaults(&mut raw)?;
        let document: ConfigDocument = raw
            .try_into()
            .map_err(|error| CoreError::Config(format!("配置字段无效: {error}")))?;
        let providers = resolve_catalog(&document.providers)?;
        validate_roles(&document.models.roles, &providers)?;
        validate_compaction(&document.compaction)?;
        schema::validate_tools(&document.tools)?;
        validate_skills(&document.skills)?;
        validate_plugins(&document.plugins)?;
        let web_search_provider = schema::validate_web_search(&document.web_search)?;
        let web_search_key =
            model::resolve_secret("web_search.api_key", &document.web_search.api_key)?;
        schema::validate_web_fetch(&document.web_fetch)?;
        schema::validate_ptc(&document.tools.ptc)?;
        schema::validate_agents(&document.agents)?;
        schema::validate_limits(&document.limits)?;
        if document.tools.job.max_running == 0 {
            return Err(CoreError::Config("后台进程上限必须大于 0".into()));
        }
        let session_dir = expand_config_path(&document.session.dir, &home)?;
        let sandbox = resolve_sandbox(&document.sandbox)?;
        validate_sandbox_boundaries(&sandbox, &cwd)?;
        Ok(Self {
            home,
            cwd,
            roles: document.models.roles,
            tools: document.tools,
            sandbox,
            compaction: document.compaction,
            limits: document.limits,
            system_prompt: document.system_prompt,
            context_files: document.context_files,
            workspace: document.workspace,
            session: SessionSettings {
                dir: session_dir,
                compress: document.session.compress,
                retention: document.session.retention,
            },
            mcp: document.mcp,
            skills: document.skills,
            plugins: document.plugins,
            web_search: ResolvedWebSearch {
                enabled: document.web_search.enabled,
                provider: web_search_provider,
                base_url: document.web_search.base_url,
                timeout_secs: document.web_search.timeout_secs,
                api_key: web_search_key,
            },
            web_fetch: ResolvedWebFetch {
                enabled: document.web_fetch.enabled,
                timeout_secs: document.web_fetch.timeout_secs,
                max_bytes: document.web_fetch.max_bytes,
                max_chars: document.web_fetch.max_chars,
            },
            agents: document.agents,
            providers,
            base_raw,
            overlays,
        })
    }

    /// 汇总该项目的全部 MCP server 配置：TOML `[mcp.servers]`、用户
    /// `~/.moc/mcp.json`，以及需显式开启 `[mcp] project_config` 才加载的社区
    /// `.mcp.json`（cwd 向上到 git root）。
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
        Ok(moc_mcp::resolve_all(&self.mcp, &source))
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

/// 展开沙盒路径列表：支持 `~`，展开后必须是绝对路径；允许路径必须存在。
fn resolve_sandbox(config: &SandboxConfig) -> Result<ResolvedSandboxConfig, CoreError> {
    Ok(ResolvedSandboxConfig {
        default_mode: config.default_mode,
        network: config.network,
        on_denied: config.on_denied,
        allow_unsandboxed: config.allow_unsandboxed,
        overlay_home: config.overlay_home,
        extra_read_paths: resolve_sandbox_paths(
            "sandbox.extra_read_paths",
            &config.extra_read_paths,
            true,
        )?,
        extra_write_paths: resolve_sandbox_paths(
            "sandbox.extra_write_paths",
            &config.extra_write_paths,
            true,
        )?,
        deny_read_paths: resolve_sandbox_paths(
            "sandbox.deny_read_paths",
            &config.deny_read_paths,
            false,
        )?,
        deny_write_paths: resolve_sandbox_paths(
            "sandbox.deny_write_paths",
            &config.deny_write_paths,
            false,
        )?,
    })
}

fn validate_sandbox_boundaries(
    sandbox: &ResolvedSandboxConfig,
    cwd: &Path,
) -> Result<(), CoreError> {
    let workspace = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for (field, paths) in [
        ("sandbox.deny_read_paths", &sandbox.deny_read_paths),
        ("sandbox.deny_write_paths", &sandbox.deny_write_paths),
    ] {
        for path in paths {
            if path == &workspace || crate::sandbox::is_within(&workspace, path) {
                return Err(CoreError::Config(format!(
                    "{field}: 不能等于或包含会话工作区 {}（{path}）",
                    workspace.display(),
                    path = path.display()
                )));
            }
        }
    }
    Ok(())
}

fn resolve_sandbox_paths(
    field: &str,
    values: &[String],
    must_exist: bool,
) -> Result<Vec<PathBuf>, CoreError> {
    let mut paths = Vec::with_capacity(values.len());
    for value in values {
        let expanded = expand_home_path(Path::new(value));
        if !expanded.is_absolute() {
            return Err(CoreError::Config(format!(
                "{field}: 路径必须是绝对路径或 ~ 开头：{value}"
            )));
        }
        if must_exist && !expanded.exists() {
            return Err(CoreError::Config(format!(
                "{field}: 路径不存在：{}",
                expanded.display()
            )));
        }
        let canonical = std::fs::canonicalize(&expanded).unwrap_or(expanded);
        if !paths.contains(&canonical) {
            paths.push(canonical);
        }
    }
    paths.sort();
    Ok(paths)
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

/// 清理已移除的配置键；其余未知字段仍由 schema 拒绝。
fn remove_deprecated_retention_keys(raw: &mut Value) {
    if let Some(retention) = raw
        .get_mut("session")
        .and_then(|session| session.get_mut("retention"))
        .and_then(Value::as_table_mut)
    {
        for key in [
            "archive_after_days",
            "retain_newest_global",
            "retain_newest_per_workspace",
        ] {
            retention.remove(key);
        }
    }
}

fn apply_model_defaults(raw: &mut Value) -> Result<(), CoreError> {
    // `defaults` 是给模型补缺省字段的指令，不属于类型化配置：无论能否用上，
    // 都要在解析前摘除，否则 deny_unknown_fields 会把整个 [defaults] 段判成非法字段。
    let Some(defaults) = raw
        .as_table_mut()
        .and_then(|table| table.remove("defaults"))
    else {
        return Ok(());
    };
    // 空的 [defaults] 表没有任何默认值可用，与「没有 defaults 段」等价。
    let Some(models) = defaults.get("models") else {
        return Ok(());
    };
    let defaults_models = models
        .as_table()
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
            ptc_worker: None,
        }
    }

    fn provider_config(api_key: &str) -> String {
        format!(
            "[models.roles]\ndefault='p/model:high'\nexplore='p/model:low'\nplan='p/model:max'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\napi_key='{api_key}'\n[providers.p.models.model]\nreasoning=true\ninput=['text','image']\n"
        )
    }

    #[test]
    fn session_compression_defaults_off_and_can_be_enabled() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        fs::create_dir_all(&home).expect("创建 home");
        fs::create_dir_all(&cwd).expect("创建 cwd");
        let config_path = home.join("config.toml");
        fs::write(&config_path, provider_config("literal")).expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("默认配置可解析");
        assert!(!config.session.compress, "会话压缩默认关闭");

        fs::write(
            &config_path,
            format!("{}\n[session]\ncompress=true\n", provider_config("literal")),
        )
        .expect("写压缩配置");
        let config =
            ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("压缩配置可解析");
        assert!(config.session.compress);
    }

    #[test]
    fn sandbox_paths_are_expanded_and_allow_paths_must_exist() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        let extra = dir.path().join("extra");
        fs::create_dir_all(&home).expect("创建 home");
        fs::create_dir_all(&cwd).expect("创建 cwd");
        fs::create_dir_all(&extra).expect("创建 extra");
        let config_path = home.join("config.toml");
        fs::write(
            &config_path,
            format!(
                "{}\n[sandbox]\ndefault_mode='workspace_only'\nnetwork=false\non_denied='deny'\nextra_read_paths=[{:?}]\ndeny_read_paths=['/definitely-not-existing']\n",
                provider_config("literal"),
                extra.display().to_string()
            ),
        )
        .expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, &cwd, vec![])).expect("沙盒配置可解析");
        assert_eq!(
            config.sandbox.default_mode,
            SessionPermission::WorkspaceOnly
        );
        assert!(!config.sandbox.network);
        assert_eq!(config.sandbox.on_denied, OnDenied::Deny);
        assert_eq!(
            config.sandbox.extra_read_paths,
            vec![extra.canonicalize().unwrap()]
        );
        assert_eq!(
            config.sandbox.deny_read_paths,
            vec![PathBuf::from("/definitely-not-existing")]
        );

        fs::write(
            &config_path,
            format!(
                "{}\n[sandbox]\nextra_read_paths=['relative/path']\n",
                provider_config("literal")
            ),
        )
        .expect("写相对路径配置");
        assert!(ResolvedConfig::load(&core_config(&home, &cwd, vec![])).is_err());

        fs::write(
            &config_path,
            format!(
                "{}\n[sandbox]\nextra_read_paths=['/definitely/not/existing']\n",
                provider_config("literal")
            ),
        )
        .expect("写不存在路径配置");
        assert!(ResolvedConfig::load(&core_config(&home, &cwd, vec![])).is_err());
    }

    #[test]
    fn deny_paths_cannot_cover_the_workspace() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        fs::create_dir_all(&home).expect("创建 home");
        fs::create_dir_all(&cwd).expect("创建 cwd");
        let config_path = home.join("config.toml");
        for denied in [cwd.clone(), dir.path().to_path_buf()] {
            fs::write(
                &config_path,
                format!(
                    "{}\n[sandbox]\ndeny_write_paths=[{:?}]\n",
                    provider_config("literal"),
                    denied.display().to_string()
                ),
            )
            .expect("写配置");
            assert!(
                ResolvedConfig::load(&core_config(&home, &cwd, vec![])).is_err(),
                "deny 路径 {denied:?} 覆盖工作区时必须报错"
            );
        }
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
                "{}\n[sandbox]\nnetwork=false\non_denied=\"deny\"\n[tools.read]\ndefault_limit=400\nmax_bytes=40000\n",
                provider_config("literal")
            ),
        )
        .expect("写全局配置");
        fs::write(
            project.join(".moc/config.toml"),
            "[tools.read]\ndefault_limit=600\nline_max_bytes=12345\n\n[sandbox]\nnetwork=true\n",
        )
        .expect("写项目配置");
        fs::write(
            &overlay,
            "[tools.read]\ndefault_limit=700\nmax_bytes=8192\n\n[sandbox]\nallow_unsandboxed=false\n",
        )
        .expect("写 CLI 覆盖层");

        let config = ResolvedConfig::load(&core_config(&home, &project, vec![overlay]))
            .expect("全局与 CLI 配置可解析")
            .for_project(&project)
            .expect("项目配置可合并");

        assert!(config.sandbox.network);
        assert_eq!(config.sandbox.on_denied, OnDenied::Deny);
        assert!(!config.sandbox.allow_unsandboxed);
        // 项目配置覆盖全局、CLI 覆盖层再覆盖项目：三层的值都能看出来。
        assert_eq!(config.tools.read.default_limit, 700);
        assert_eq!(config.tools.read.line_max_bytes, 12345);
        assert_eq!(config.tools.read.max_bytes, 8192);
        assert_eq!(
            config
                .resolve_role("default")
                .expect("default 模型")
                .thinking_level,
            ThinkingLevel::High
        );
        assert_eq!(
            config
                .resolve_role("plan")
                .expect("plan 模型")
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

    /// 插件配置的 0 值必须拒绝：它会让「永远超时」「永远加载失败」变成静默行为。
    #[test]
    fn rejects_zero_plugin_limits_and_unknown_plugin_keys() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        let base = provider_config("k");
        fs::write(
            home.join("config.toml"),
            format!("{base}\n[plugins]\ncall_timeout_ms=0\n"),
        )
        .expect("写配置");
        let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
            Ok(_) => panic!("0 超时必须报错"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("call_timeout_ms"), "{error}");

        fs::write(
            home.join("config.toml"),
            format!("{base}\n[plugins]\nmax_plugns=3\n"),
        )
        .expect("写配置");
        assert!(
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err(),
            "未知字段必须报错"
        );

        // 默认值：全开、64 MiB、5 秒、4k 提示词预算。
        fs::write(home.join("config.toml"), &base).expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("默认可用");
        assert!(config.plugins.enabled && config.plugins.project && config.plugins.user);
        assert_eq!(config.plugins.memory_limit_bytes, 64 * 1024 * 1024);
        assert_eq!(config.plugins.call_timeout_ms, 5000);
        assert_eq!(config.plugins.max_prompt_chars, 4000);
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
explore = "p/ag/gpt-5.6-sol:high"
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
            "[models.roles]\ndefault='p/m:max'\nexplore='p/m:low'\nplan='p/m:max'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\nreasoning=true\n",
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

    #[test]
    fn validates_standard_tool_concurrency_limit() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[tools]\nmax_concurrent_calls = 0\n",
                provider_config("literal")
            ),
        )
        .expect("写配置");
        assert!(ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err());
    }

    #[test]
    fn plan_role_accepts_none_and_other_roles_reject_it() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        let provider = "[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n";
        fs::write(
            home.join("config.toml"),
            format!("[models.roles]\ndefault='p/m'\nplan='none'\n{provider}"),
        )
        .expect("写配置");
        let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
            .expect("计划模型可以是 none");
        assert!(
            config
                .resolve_plan_mode_model()
                .expect("解析计划模型")
                .is_none()
        );
        assert!(config.resolve_model("plan").is_err());
        for roles in ["default='none'", "default='p/m'\nexplore='none'"] {
            fs::write(
                home.join("config.toml"),
                format!("[models.roles]\n{roles}\n{provider}"),
            )
            .expect("写配置");
            assert!(
                ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).is_err(),
                "{roles} 不能是 none"
            );
        }
    }

    #[test]
    fn web_tools_follow_their_own_switches() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(home.join("config.toml"), provider_config("literal")).expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
        assert!(config.web_search.enabled);
        assert_eq!(config.web_search.provider, "tavily");
        assert_eq!(config.web_search.base_url, "");
        assert_eq!(config.web_search.timeout_secs, 30);
        assert!(config.web_search.api_key.is_none());
        assert!(config.web_fetch.enabled);
        assert_eq!(config.web_fetch.timeout_secs, 30);
        assert_eq!(config.web_fetch.max_bytes, 50 * 1024 * 1024);
        assert_eq!(config.web_fetch.max_chars, 500_000);
        let registry = crate::tools::ToolRegistry::new();
        let visible = |config: &ResolvedConfig, name: &str| {
            registry
                .definitions(
                    moc_protocol::methods::SessionEngine::Standard,
                    false,
                    config,
                    crate::tools::ToolScope::top(),
                    &[],
                )
                .iter()
                .any(|definition| definition.name == name)
        };
        assert!(!visible(&config, "web_search"), "无 key 时搜索工具隐藏");
        assert!(visible(&config, "web_fetch"), "抓取不依赖密钥");

        fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[web_search]\napi_key='tvly-test'\n",
                provider_config("literal")
            ),
        )
        .expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
        assert_eq!(config.web_search.api_key.as_deref(), Some("tvly-test"));
        assert!(visible(&config, "web_search"));
        assert!(visible(&config, "web_fetch"));

        fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[web_search]\napi_key='tvly-test'\n[web_fetch]\nenabled=false\n",
                provider_config("literal")
            ),
        )
        .expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
        assert!(visible(&config, "web_search"));
        assert!(!visible(&config, "web_fetch"), "独立开关只隐藏抓取");
    }

    #[test]
    fn limits_reject_removed_knobs_and_invalid_deadlines() {
        // 已移除的轮数／字节／token／调用数上限必须被拒绝，避免旧配置静默失效。
        for (snippet, expected) in [
            ("max_turns_per_prompt=1\n", "max_turns_per_prompt"),
            ("max_response_bytes=1\n", "max_response_bytes"),
            (
                "max_response_duration_secs=1\n",
                "max_response_duration_secs",
            ),
            ("max_prompt_bytes=1\n", "max_prompt_bytes"),
            ("max_prompt_output_tokens=1\n", "max_prompt_output_tokens"),
            ("max_tool_calls_per_prompt=1\n", "max_tool_calls_per_prompt"),
            ("max_prompt_duration_secs=1\n", "max_prompt_duration_secs"),
            ("connect_deadline_secs=0\n", "必须大于 0"),
            ("request_warn_secs=0\n", "必须大于 0"),
            ("connect_deadline_secs=604801\n", "不得超过 7 天"),
        ] {
            let dir = tempfile::tempdir().expect("临时目录");
            let home = dir.path().join("home");
            fs::create_dir_all(&home).expect("创建 home");
            fs::write(
                home.join("config.toml"),
                format!("{}\n[limits]\n{snippet}", provider_config("literal")),
            )
            .expect("写配置");
            let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
                Ok(_) => panic!("{snippet} 应被拒绝"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {snippet}: {error}"
            );
        }
    }

    #[test]
    fn limits_defaults_cover_loop_detection_and_deadlines() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(home.join("config.toml"), provider_config("literal")).expect("写配置");
        let config =
            ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())).expect("配置可解析");
        assert_eq!(config.limits.connect_deadline_secs, 900);
        assert_eq!(config.limits.request_warn_secs, 900);
        assert!(config.limits.loop_detection);
        assert_eq!(config.limits.max_length_continues, 2);
    }

    #[test]
    fn rejects_invalid_web_search_settings() {
        for (snippet, expected) in [
            ("provider='google'\n", "web_search.provider"),
            (
                "base_url='ftp://proxy.example.com'\n",
                "web_search.base_url",
            ),
            ("timeout_secs=0\n", "web_search.timeout_secs"),
        ] {
            let dir = tempfile::tempdir().expect("临时目录");
            let home = dir.path().join("home");
            fs::create_dir_all(&home).expect("创建 home");
            fs::write(
                home.join("config.toml"),
                format!("{}\n[web_search]\n{snippet}", provider_config("literal")),
            )
            .expect("写配置");
            let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
                Ok(_) => panic!("{snippet} 应被拒绝"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {snippet}: {error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_web_fetch_settings() {
        for (snippet, expected) in [
            ("timeout_secs=0\n", "web_fetch.timeout_secs"),
            ("max_bytes=0\n", "web_fetch.max_bytes"),
            ("max_chars=0\n", "web_fetch.max_chars"),
        ] {
            let dir = tempfile::tempdir().expect("临时目录");
            let home = dir.path().join("home");
            fs::create_dir_all(&home).expect("创建 home");
            fs::write(
                home.join("config.toml"),
                format!("{}\n[web_fetch]\n{snippet}", provider_config("literal")),
            )
            .expect("写配置");
            let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
                Ok(_) => panic!("{snippet} 应被拒绝"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains(expected),
                "unexpected error for {snippet}: {error}"
            );
        }
    }

    #[test]
    fn normalizes_web_search_provider_case() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[web_search]\nprovider=' Tavily '\n",
                provider_config("literal")
            ),
        )
        .expect("写配置");
        let config = ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new()))
            .expect("大小写与空白被归一化");
        assert_eq!(config.web_search.provider, "tavily");
    }

    #[test]
    fn web_search_env_secret_reports_missing_variable() {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("创建 home");
        fs::write(
            home.join("config.toml"),
            format!(
                "{}\n[web_search]\napi_key='env:MOC_TEST_MISSING_WEB_KEY_7261'\n",
                provider_config("literal")
            ),
        )
        .expect("写配置");
        let error = match ResolvedConfig::load(&core_config(&home, dir.path(), Vec::new())) {
            Ok(_) => panic!("缺失环境变量应报错"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("web_search.api_key"));
    }
}
