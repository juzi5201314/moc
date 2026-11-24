//! 运行时配置加载、合并与解析。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use moc_llm::LlmConfig;
use moc_protocol::SessionPermission;
use moc_protocol::features::{FeatureFlag, FeatureStates};
use moc_i18n::t;
use toml::Value;

use crate::CoreConfig;
use crate::error::CoreError;
use model::{
    FusionDefaults, ModelRoles, ResolvedModel, all_image_models, all_models, resolve_catalog,
    resolve_image_one, resolve_selector, validate_models,
};
use schema::{
    ConfigDocument, SandboxConfig, validate_compaction, validate_plugins, validate_skills,
};

pub mod catalog;
pub mod discovery;
pub mod editor;
pub mod model;
pub(crate) mod parameters;
mod schema;
pub mod selection;
pub mod web;

pub use model::{ModelCapabilities, ROLE_INHERIT, ThinkingLevel};
pub use schema::{
    BashConfig, CompactionConfig, CompactionMethod, CompactionThreshold, ContextFilesConfig,
    EditConfig, FeaturesConfig, FindConfig, GitConfig, GrepConfig, HOST_PLUGIN_KEYS, JobConfig,
    JobOverflow, LimitsConfig, OnDenied, OutputConfig, PluginsConfig, PtcConfig, ReadConfig,
    RetentionConfig, ShakeConfig, SkillsConfig, SystemPromptConfig, TodoConfig, ToolsConfig,
    WebFetchConfig, WorkflowConfig, WorkspaceConfig, WriteConfig, is_plugin_key,
};

/// 会话文件与保留策略；跨进程锁始终启用。
#[derive(Debug, Clone)]
pub struct SessionSettings {
    pub dir: PathBuf,
    pub compress: bool,
    pub retention: RetentionConfig,
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
    /// `.agents` 用户级约定（`~/.agents/mcp.json`、`~/.agents/skills`）的挂靠
    /// 目录；来自 [`CoreConfig::user_home`]，未配置时取真实用户主目录。
    pub user_home: Option<PathBuf>,
    pub roles: ModelRoles,
    /// `[models.fusion]` 的默认组合；两侧都为空表示未配置。
    pub fusion: FusionDefaults,
    /// `[models] recent`：最近使用模型最多保留几条；0 表示不记录。
    pub recent_limit: usize,
    pub tools: ToolsConfig,
    pub sandbox: ResolvedSandboxConfig,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub system_prompt: SystemPromptConfig,
    pub git: GitConfig,
    pub context_files: ContextFilesConfig,
    pub workspace: WorkspaceConfig,
    pub session: SessionSettings,
    pub mcp: moc_mcp::McpDocument,
    pub skills: SkillsConfig,
    pub plugins: PluginsConfig,
    pub web_fetch: ResolvedWebFetch,
    pub agents: schema::AgentsConfig,
    pub workflow: schema::WorkflowConfig,
    /// 用户界面语言（`auto`/`zh`/`en`）；`auto` 由前端按系统环境解析。
    pub language: String,
    /// 特性开关状态快照：`[features]` 覆盖与各段 `enabled` 合成，加载时定死。
    pub features: FeatureStates,
    providers: BTreeMap<String, model::ResolvedProvider>,
    base_raw: Value,
    overlays: Vec<PathBuf>,
}

impl ResolvedConfig {
    /// 读取内置默认、全局配置与命令行覆盖层。
    pub fn load(config: &CoreConfig) -> Result<Self, CoreError> {
        let home = expand_home_path(&config.home);
        let mut base_raw: Value =
            toml::from_str(include_str!("defaults.toml")).map_err(|error| {
                CoreError::Config(t!("core.err.defaults_invalid", error = error).into_owned())
            })?;
        merge_optional_file(&mut base_raw, &home.join("config.toml"))?;
        let legacy_model = base_raw
            .get("models")
            .and_then(|models| models.get("roles"))
            .and_then(|roles| roles.get("default"))
            .and_then(Value::as_str)
            .filter(|selector| !selector.is_empty())
            .map(str::to_owned);
        let mut raw = base_raw.clone();
        for overlay in &config.overlays {
            merge_required_file(&mut raw, overlay)?;
        }
        let resolved = Self::from_merged(
            raw,
            base_raw,
            home,
            expand_home_path(&config.cwd),
            config.user_home.clone().or_else(crate::paths::home_dir),
            config.overlays.clone(),
        )?;
        if let Some(selector) = legacy_model
            && let Ok(model) = resolved.resolve_model(&selector)
        {
            selection::migrate(&resolved.home, &model.selector)?;
        }
        Ok(resolved)
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
            self.user_home.clone(),
            self.overlays.clone(),
        )
    }

    /// 按 provider/model[:level] 或 Fusion selector 解析可调用模型。
    pub fn resolve_model(&self, selector: &str) -> Result<ResolvedModel, CoreError> {
        resolve_selector(&self.providers, &self.fusion, selector)
    }

    /// 每次从共享文件读取，保证不同前端进程能看到最新选择。
    pub fn last_model(&self) -> Result<Option<ResolvedModel>, CoreError> {
        Ok(selection::load(&self.home)?.and_then(|selector| self.resolve_model(&selector).ok()))
    }

    /// 记一次用户主动选择：更新新会话默认模型，并按 `recent_limit` 维护最近使用列表。
    pub fn remember_model(&self, model: &ResolvedModel) -> Result<(), CoreError> {
        selection::save(
            &self.home,
            &model.selector,
            self.recent_limit,
            |selector| match self.resolve_model(selector) {
                Ok(resolved) if resolved.fusion.is_none() => resolved.id,
                _ => selector.to_owned(),
            },
        )
    }

    /// 最近使用且在当前配置下仍可解析的模型，新到旧，至多 `recent_limit` 条。
    pub fn recent_models(&self) -> Result<Vec<ResolvedModel>, CoreError> {
        Ok(selection::load_recent(&self.home)?
            .iter()
            .filter_map(|selector| self.resolve_model(selector).ok())
            .take(self.recent_limit)
            .collect())
    }

    pub fn require_last_model(&self) -> Result<ResolvedModel, CoreError> {
        self.last_model()?
            .ok_or_else(|| CoreError::Config(t!("core.err.select_model_first").into_owned()))
    }

    /// 列出 provider 配置中的全部模型（不含图片生成模型），顺序按 provider 与模型 ID 稳定排序。
    pub fn available_models(&self) -> Vec<ResolvedModel> {
        all_models(&self.providers)
    }

    /// 列出 provider 配置中的全部图片生成模型（`image_gen = true`），供 Image Playground 使用。
    pub fn available_image_models(&self) -> Vec<ResolvedModel> {
        all_image_models(&self.providers)
    }

    /// 解析 Image Playground 的图片生成模型（格式 `provider/model`）。
    pub fn resolve_image_model(&self, selector: &str) -> Result<ResolvedModel, CoreError> {
        resolve_image_one(&self.providers, selector)
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
        user_home: Option<PathBuf>,
        overlays: Vec<PathBuf>,
    ) -> Result<Self, CoreError> {
        selection::remove_legacy_roles(&mut raw);
        remove_deprecated_retention_keys(&mut raw);
        apply_model_defaults(&mut raw)?;
        let document: ConfigDocument = raw.try_into().map_err(|error| {
            CoreError::Config(t!("core.err.field_invalid", error = error).into_owned())
        })?;
        let providers = resolve_catalog(&document.providers)?;
        validate_models(&document.models, &providers)?;
        if !matches!(document.language.as_str(), "" | "auto" | "zh" | "en") {
            return Err(CoreError::Config(t!("core.config.invalid_language").into()));
        }
        validate_compaction(&document.compaction)?;
        schema::validate_tools(&document.tools)?;
        validate_skills(&document.skills)?;
        validate_plugins(&document.plugins)?;
        schema::validate_web_fetch(&document.web_fetch)?;
        schema::validate_ptc(&document.tools.ptc)?;
        schema::validate_agents(&document.agents)?;
        schema::validate_limits(&document.limits)?;
        let features = resolve_features(&document)?;
        if document.tools.job.max_running == 0 || document.tools.job.max_wait_secs == 0 {
            return Err(CoreError::Config(
                t!("core.err.tools_job_positive").into_owned(),
            ));
        }
        let session_dir = expand_config_path(&document.session.dir, &home)?;
        let sandbox = resolve_sandbox(&document.sandbox)?;
        validate_sandbox_boundaries(&sandbox, &cwd)?;
        Ok(Self {
            home,
            cwd,
            user_home,
            roles: document.models.roles,
            fusion: document.models.fusion,
            recent_limit: document.models.recent,
            tools: document.tools,
            sandbox,
            compaction: document.compaction,
            limits: document.limits,
            system_prompt: document.system_prompt,
            git: document.git,
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
            web_fetch: ResolvedWebFetch {
                enabled: document.web_fetch.enabled,
                timeout_secs: document.web_fetch.timeout_secs,
                max_bytes: document.web_fetch.max_bytes,
                max_chars: document.web_fetch.max_chars,
            },
            agents: document.agents,
            workflow: document.workflow,
            language: document.language,
            features,
            providers,
            base_raw,
            overlays,
        })
    }

    /// 特性 flag 是否启用（注册表快照查询）；引擎/工作流等门控共用。
    pub fn feature_enabled(&self, flag: FeatureFlag) -> bool {
        self.features.enabled(flag)
    }

    /// 汇总该项目的全部 MCP server 配置：TOML `[mcp.servers]`、用户
    /// `~/.moc/mcp.json` 与 `~/.agents/mcp.json`，以及需显式开启
    /// `[mcp] project_config` 才加载的项目级 `.mcp.json` / `mcp.json` /
    /// `.agents/mcp.json`（cwd 向上到 git root）。
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
            let servers = moc_mcp::parse_mcp_json(&text).map_err(|error| {
                CoreError::Config(
                    t!(
                        "core.err.parse_path",
                        path = path.display().to_string(),
                        error = error
                    )
                    .into_owned(),
                )
            });
            servers.map(|servers| (path, servers))
        })
        .collect::<Result<Vec<_>, CoreError>>()?;
        let user_files = if self.mcp.user_config {
            let mut candidates = vec![moc_mcp::user_json_path(&self.home)];
            if let Some(user_home) = &self.user_home {
                candidates.push(moc_mcp::agents_user_json_path(user_home));
            }
            candidates
                .into_iter()
                .filter(|path| path.is_file())
                .map(|path| {
                    let text = fs::read_to_string(&path).map_err(|error| {
                        CoreError::Config(
                            t!(
                                "core.err.read_path",
                                path = path.display().to_string(),
                                error = error
                            )
                            .into_owned(),
                        )
                    })?;
                    let servers = moc_mcp::parse_mcp_json(&text).map_err(|error| {
                        CoreError::Config(
                            t!(
                                "core.err.parse_path",
                                path = path.display().to_string(),
                                error = error
                            )
                            .into_owned(),
                        )
                    })?;
                    Ok((path, servers))
                })
                .collect::<Result<Vec<_>, CoreError>>()?
        } else {
            Vec::new()
        };
        let source = moc_mcp::JsonSource {
            project_files,
            user_files,
        };
        Ok(moc_mcp::resolve_all(&self.mcp, &source))
    }
}

/// 把 `[features]` 覆盖与各段 `enabled` 合成注册表快照。
///
/// `Existing` 开关读原配置段（保持单点来源）；`Features` 自有开关读 `[features]`
/// 覆盖值、缺省用注册表默认。
fn resolve_features(document: &ConfigDocument) -> Result<FeatureStates, CoreError> {
    schema::validate_features(&document.features)?;
    Ok(FeatureStates::resolve(|flag| match flag {
        FeatureFlag::Clm | FeatureFlag::Ptc => document
            .features
            .get(flag.spec().key)
            .copied()
            .unwrap_or(flag.spec().default_enabled),
        FeatureFlag::Workflow => document.workflow.enabled,
        FeatureFlag::Plugins => document.plugins.enabled,
        FeatureFlag::Mcp => document.mcp.enabled,
        FeatureFlag::Skills => document.skills.enabled,
        FeatureFlag::WebFetch => document.web_fetch.enabled,
    }))
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
            .ok_or_else(|| CoreError::Config(t!("core.err.expand_home").into_owned()))?;
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
                return Err(CoreError::Config(
                    t!(
                        "core.err.sandbox_deny_workspace",
                        field = field,
                        workspace = workspace.display().to_string(),
                        path = path.display().to_string()
                    )
                    .into_owned(),
                ));
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
            return Err(CoreError::Config(
                t!("core.err.sandbox_path_abs", field = field, value = value).into_owned(),
            ));
        }
        if must_exist && !expanded.exists() {
            return Err(CoreError::Config(
                t!(
                    "core.err.sandbox_path_missing",
                    field = field,
                    path = expanded.display().to_string()
                )
                .into_owned(),
            ));
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
    let text = fs::read_to_string(path).map_err(|error| {
        CoreError::Config(
            t!(
                "core.err.read_config",
                path = path.display().to_string(),
                error = error
            )
            .into_owned(),
        )
    })?;
    let overlay: Value = toml::from_str(&text).map_err(|error| {
        CoreError::Config(
            t!(
                "core.err.parse_config",
                path = path.display().to_string(),
                error = error
            )
            .into_owned(),
        )
    })?;
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
        .ok_or_else(|| CoreError::Config(t!("core.err.defaults_models_table").into_owned()))?;
    for pattern in defaults_models.keys() {
        GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|error| {
                CoreError::Config(
                    t!(
                        "core.err.defaults_models_glob",
                        pattern = pattern,
                        error = error
                    )
                    .into_owned(),
                )
            })?;
    }

    let providers = raw
        .get_mut("providers")
        .and_then(Value::as_table_mut)
        .ok_or_else(|| CoreError::Config(t!("core.err.providers_table").into_owned()))?;
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
mod tests;
