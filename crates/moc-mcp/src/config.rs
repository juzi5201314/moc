//! MCP 服务器配置的规范表示、社区 `.mcp.json` 解析与多来源合并。
//!
//! 来源优先级（同名 server 取最高优先者，冲突告警）：
//! 1. 项目级 `.mcp.json` / `mcp.json`（cwd 向上到 git root，越近越优先）；
//! 2. moc 原生 TOML `[mcp.servers]`（用户级 + 项目级已由 core 的三层合并产出）；
//! 3. 用户级 `~/.moc/mcp.json`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::error::McpError;

/// 项目级与用户级社区配置文件名；同目录内 `.mcp.json` 优先于 `mcp.json`。
pub const PROJECT_CONFIG_FILES: [&str; 2] = [".mcp.json", "mcp.json"];
pub const USER_CONFIG_FILE: &str = "mcp.json";

/// server 名称约束：进入 `mcp__<server>__<tool>` 命名空间的前提。
///
/// 不允许出现 `__`（保证限定名可无歧义反解）、不含空白与控制字符。
pub fn validate_server_name(name: &str) -> Result<(), McpError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !name.contains("__")
        && !name.starts_with('-')
        && !name.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(McpError::Config(format!(
            "server name `{name}` is invalid: use 1-64 ASCII letters/digits with `-`/`_`, excluding `__` and leading/trailing `-`"
        )))
    }
}

/// 单个 server 的原始描述；`.mcp.json` 与 TOML `[mcp.servers]` 共用。
///
/// 刻意**不**拒绝未知字段：社区配置常携带各家扩展键（如 `startup_timeout_ms`），
/// 严格拒绝会让合法的社区文件无法加载。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawServerSpec {
    /// 社区写法中的 `type`：`stdio`/`local` 或 `http`/`sse`/`remote`。
    /// 通常可省略——有 `command` 即 stdio，有 `url` 即 HTTP。
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 覆盖全局 `call_timeout_secs` 的单 server 调用时限。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub tools: Option<ToolFilter>,
}

/// 工具名过滤（glob 语法，匹配 server 内的原始工具名）；`deny` 永远优先。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolFilter {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl ToolFilter {
    /// 编译为可匹配的过滤器；allow/deny 均为空时返回 `None`（不过滤）。
    pub fn compile(&self) -> Result<Option<CompiledToolFilter>, McpError> {
        if self.allow.is_empty() && self.deny.is_empty() {
            return Ok(None);
        }
        let mut allow = GlobSetBuilder::new();
        for pattern in &self.allow {
            allow.add(glob(pattern)?);
        }
        let mut deny = GlobSetBuilder::new();
        for pattern in &self.deny {
            deny.add(glob(pattern)?);
        }
        Ok(Some(CompiledToolFilter {
            allow: allow.build().map_err(|error| {
                McpError::Config(format!("tools.allow glob compilation failed: {error}"))
            })?,
            deny: deny.build().map_err(|error| {
                McpError::Config(format!("tools.deny glob compilation failed: {error}"))
            })?,
            allow_empty: self.allow.is_empty(),
        }))
    }
}

fn glob(pattern: &str) -> Result<globset::Glob, McpError> {
    // 工具名匹配按整串进行，`*` 需要能跨 `__` 等任意字符。
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|error| McpError::Config(format!("invalid glob `{pattern}`: {error}")))
}

/// 编译后的工具过滤器。
#[derive(Debug, Clone)]
pub struct CompiledToolFilter {
    allow: GlobSet,
    deny: GlobSet,
    allow_empty: bool,
}

impl CompiledToolFilter {
    /// `deny` 命中即拒绝；`allow` 非空时仅放行命中者。
    pub fn admits(&self, tool: &str) -> bool {
        if self.deny.is_match(tool) {
            return false;
        }
        self.allow_empty || self.allow.is_match(tool)
    }
}

/// 传输方式。stdio 由本进程拉起子进程；HTTP 指向 Streamable HTTP 端点。
#[derive(Debug, Clone)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// 解析完成的单个 server 配置。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub enabled: bool,
    pub tools: Option<CompiledToolFilter>,
    /// 握手 + `tools/list` 的时限（秒）。
    pub connect_timeout_secs: u64,
    /// 单次 `tools/call` 的时限（秒）。
    pub call_timeout_secs: u64,
}

/// TOML `[mcp]` 段的结构；也承载全局默认值。
#[derive(Debug, Clone, Deserialize)]
pub struct McpDocument {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub user_config: bool,
    #[serde(default = "default_true")]
    pub project_config: bool,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_call_timeout")]
    pub call_timeout_secs: u64,
    #[serde(default)]
    pub servers: BTreeMap<String, RawServerSpec>,
}

impl Default for McpDocument {
    fn default() -> Self {
        Self {
            enabled: true,
            user_config: true,
            project_config: true,
            connect_timeout_secs: default_connect_timeout(),
            call_timeout_secs: default_call_timeout(),
            servers: BTreeMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_connect_timeout() -> u64 {
    15
}
fn default_call_timeout() -> u64 {
    120
}

/// 展开字符串中的 `${VAR}` 与 `${VAR:-default}`；未设置且无默认值时展开为空并告警。
pub fn expand_env(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$'
            && bytes.get(index + 1) == Some(&b'{')
            && let Some(close) = input[index + 2..].find('}')
        {
            let expr = &input[index + 2..index + 2 + close];
            let (name, default) = match expr.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (expr, None),
            };
            if name.is_empty() || name.contains('$') {
                // 非变量引用形态（如 `${}`），按字面量保留。
                out.push_str(&input[index..index + 2 + close + 1]);
            } else {
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => {
                        if let Some(default) = default {
                            out.push_str(default);
                        } else {
                            tracing::warn!("环境变量 {name} 未设置，MCP 配置中展开为空");
                        }
                    }
                }
            }
            index += 2 + close + 1;
            continue;
        }
        let step = input[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        out.push_str(&input[index..index + step]);
        index += step;
    }
    out
}

fn expand_all_env(spec: &mut RawServerSpec) {
    if let Some(command) = &mut spec.command {
        *command = expand_env(command);
    }
    if let Some(args) = &mut spec.args {
        for arg in args.iter_mut() {
            *arg = expand_env(arg);
        }
    }
    if let Some(env) = &mut spec.env {
        for value in env.values_mut() {
            *value = expand_env(value);
        }
    }
    if let Some(cwd) = &mut spec.cwd {
        *cwd = expand_env(cwd);
    }
    if let Some(url) = &mut spec.url {
        *url = expand_env(url);
    }
    if let Some(headers) = &mut spec.headers {
        for value in headers.values_mut() {
            *value = expand_env(value);
        }
    }
}

/// 解析社区 `mcp.json` 文本：接受顶层 `mcpServers`（Claude Desktop 格式）、
/// `servers`（VS Code 变体）或裸的名称→描述映射。
pub fn parse_mcp_json(text: &str) -> Result<BTreeMap<String, RawServerSpec>, McpError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| McpError::Config(format!("invalid JSON: {error}")))?;
    let servers = match &value {
        serde_json::Value::Object(map) => map
            .get("mcpServers")
            .or_else(|| map.get("servers"))
            .cloned()
            .unwrap_or(value),
        _ => return Err(McpError::Config("JSON top level must be an object".into())),
    };
    serde_json::from_value(servers)
        .map_err(|error| McpError::Config(format!("invalid server description: {error}")))
}

/// 从 cwd 向上（含 git root 本身）收集项目级 `.mcp.json` / `mcp.json`，越近越靠前。
pub fn discover_project_json(cwd: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in cwd.ancestors() {
        for file in PROJECT_CONFIG_FILES {
            let candidate = dir.join(file);
            if candidate.is_file() {
                found.push(candidate);
                break;
            }
        }
        if Some(dir) == git_root {
            break;
        }
    }
    found
}

/// 用户级配置文件路径：`<home>/mcp.json`（`home` 即 `~/.moc`）。
pub fn user_json_path(home: &Path) -> PathBuf {
    home.join(USER_CONFIG_FILE)
}

/// 把原始描述归一化为可启动配置；做环境变量展开、传输判定与过滤编译。
pub fn resolve_server(
    name: &str,
    raw: &RawServerSpec,
    global_connect_timeout_secs: u64,
    global_call_timeout_secs: u64,
) -> Result<ServerConfig, McpError> {
    validate_server_name(name)?;
    let mut spec = raw.clone();
    expand_all_env(&mut spec);

    let kind = spec.kind.as_deref().map(str::to_ascii_lowercase);
    let transport = match (&spec.command, &spec.url, kind.as_deref()) {
        (Some(_), _, Some("http" | "sse" | "remote")) => {
            return Err(McpError::Config(format!(
                "server `{name}` declares both command and HTTP transport types"
            )));
        }
        (Some(command), _, _) => {
            let args = spec.args.clone().unwrap_or_default();
            let env = spec.env.clone().unwrap_or_default();
            let cwd = spec
                .cwd
                .as_deref()
                .map(expand_home_in_path)
                .map(PathBuf::from);
            Transport::Stdio {
                command: command.clone(),
                args,
                env,
                cwd,
            }
        }
        (None, Some(_), Some("stdio" | "local")) => {
            return Err(McpError::Config(format!(
                "server `{name}` declares stdio transport without a command"
            )));
        }
        (None, Some(url), _) => {
            if let Some("sse") = kind.as_deref() {
                tracing::warn!("server `{name}` 使用已废弃的 sse 类型，按 streamable HTTP 处理");
            }
            Transport::Http {
                url: url.clone(),
                headers: spec.headers.clone().unwrap_or_default(),
            }
        }
        (None, None, _) => {
            return Err(McpError::Config(format!(
                "server `{name}` declares neither a stdio command nor an HTTP URL"
            )));
        }
    };
    Ok(ServerConfig {
        name: name.to_owned(),
        transport,
        enabled: spec.enabled.unwrap_or(true),
        tools: match &spec.tools {
            Some(filter) => filter.compile()?,
            None => None,
        },
        connect_timeout_secs: global_connect_timeout_secs,
        call_timeout_secs: spec.timeout_secs.unwrap_or(global_call_timeout_secs),
    })
}

fn expand_home_in_path(value: &str) -> String {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        && let Some(home) = home
    {
        return Path::new(&home).join(rest).to_string_lossy().into_owned();
    }
    value.to_owned()
}

/// 一个已解析的社区配置来源；`project` 内部按发现顺序（近者优先）排列。
#[derive(Debug, Clone)]
pub struct JsonSource {
    pub project_files: Vec<(PathBuf, BTreeMap<String, RawServerSpec>)>,
    pub user_file: Option<(PathBuf, BTreeMap<String, RawServerSpec>)>,
}

/// 按优先级合并全部来源：项目 `.mcp.json`（近者优先）> TOML `[mcp.servers]` > 用户 `mcp.json`。
/// 返回按名称排序的配置列表；同名冲突告警并保留最高优先者。
pub fn resolve_all(toml: &McpDocument, json: &JsonSource) -> Result<Vec<ServerConfig>, McpError> {
    let mut merged: BTreeMap<String, (RawServerSpec, String)> = BTreeMap::new();
    // 项目级按发现顺序（近 → 远）遍历：近者先占位，远处同名告警后忽略。
    for (path, servers) in &json.project_files {
        let displayed = path.display().to_string();
        for (name, spec) in servers {
            match merged.get(name) {
                Some((_, nearer)) => tracing::warn!(
                    "MCP server `{name}` 同时出现在 {nearer}（更近）与 {displayed}，采用前者"
                ),
                None => {
                    merged.insert(name.clone(), (spec.clone(), displayed.clone()));
                }
            }
        }
    }
    for (name, spec) in &toml.servers {
        match merged.get(name) {
            Some((_, source)) => tracing::warn!(
                "MCP server `{name}` 同时出现在 {source}（项目级 .mcp.json）与 [mcp.servers]，采用前者"
            ),
            None => {
                merged.insert(name.clone(), (spec.clone(), "[mcp.servers]".to_owned()));
            }
        }
    }
    if let Some((path, servers)) = &json.user_file {
        let displayed = path.display().to_string();
        for (name, spec) in servers {
            match merged.get(name) {
                Some((_, source)) => tracing::warn!(
                    "MCP server `{name}` 同时出现在 {source}（更高优先级）与 {displayed}，采用前者"
                ),
                None => {
                    merged.insert(name.clone(), (spec.clone(), displayed.clone()));
                }
            }
        }
    }

    let mut configs = Vec::with_capacity(merged.len());
    for (name, (spec, _)) in merged {
        configs.push(resolve_server(
            &name,
            &spec,
            toml.connect_timeout_secs,
            toml.call_timeout_secs,
        )?);
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_mcp_servers_servers_and_bare_forms() {
        let claude = r#"{"mcpServers":{"github":{"command":"npx","args":["-y","x"]}}}"#;
        let parsed = parse_mcp_json(claude).unwrap();
        assert!(parsed.contains_key("github"));

        let vscode = r#"{"servers":{"a":{"url":"http://x/mcp"}}}"#;
        let parsed = parse_mcp_json(vscode).unwrap();
        assert!(parsed.contains_key("a"));

        let bare = r#"{"b":{"command":"uvx","env":{"K":"V"}}}"#;
        let parsed = parse_mcp_json(bare).unwrap();
        assert!(parsed.contains_key("b"));

        // 未知社区字段被忽略，不阻断已知字段解析。
        let community =
            r#"{"mcpServers":{"x":{"command":"c","startup_timeout_ms":3000,"version":"1"}}}"#;
        let parsed = parse_mcp_json(community).unwrap();
        assert_eq!(parsed["x"].command.as_deref(), Some("c"));

        assert!(parse_mcp_json("[]").is_err());
        assert!(parse_mcp_json("not json").is_err());
    }

    #[test]
    fn env_expansion_supports_default_and_missing() {
        // SAFETY：单线程测试中设置进程级环境变量；名称唯一避免并行测试互扰。
        unsafe { std::env::set_var("MOC_MCP_TEST_VAR", "present") };
        assert_eq!(expand_env("${MOC_MCP_TEST_VAR}"), "present");
        assert_eq!(expand_env("${MOC_MCP_TEST_MISSING:-fallback}"), "fallback");
        assert_eq!(expand_env("${MOC_MCP_TEST_MISSING}"), "");
        assert_eq!(expand_env("a${MOC_MCP_TEST_VAR}b"), "apresentb");
        // 非闭合与空表达式按字面量保留。
        assert_eq!(expand_env("${unclosed"), "${unclosed");
        assert_eq!(expand_env("${}"), "${}");
        unsafe { std::env::remove_var("MOC_MCP_TEST_VAR") };
    }

    #[test]
    fn transport_resolution_maps_command_and_url() {
        let stdio = resolve_server(
            "fs",
            &serde_json::from_value(json!({
                "command": "${MOC_MCP_TEST_UNSET_VAR:-uvx}",
                "args": ["mcp-server-fs"],
                "env": {"A": "1"},
                "cwd": "/tmp"
            }))
            .unwrap(),
            15,
            60,
        )
        .unwrap();
        match &stdio.transport {
            Transport::Stdio {
                command, args, cwd, ..
            } => {
                assert_eq!(command, "uvx");
                assert_eq!(args, &["mcp-server-fs".to_owned()]);
                assert_eq!(cwd.as_deref(), Some(Path::new("/tmp")));
            }
            other => panic!("应为 stdio：{other:?}"),
        }

        let http = resolve_server(
            "api",
            &serde_json::from_value(json!({
                "type": "sse",
                "url": "http://127.0.0.1:3927/mcp",
                "headers": {"Authorization": "Bearer t"}
            }))
            .unwrap(),
            15,
            60,
        )
        .unwrap();
        match &http.transport {
            Transport::Http { url, headers } => {
                assert_eq!(url, "http://127.0.0.1:3927/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer t")
                );
            }
            other => panic!("应为 http：{other:?}"),
        }

        // 缺失传输字段必须报错，而不是默默启动。
        assert!(resolve_server("bad", &RawServerSpec::default(), 15, 60).is_err());
        assert!(
            resolve_server(
                "__bad__",
                &serde_json::from_value(json!({"url": "http://x"})).unwrap(),
                15,
                60
            )
            .is_err()
        );
    }

    #[test]
    fn tool_filter_deny_beats_allow_and_supports_globs() {
        let filter = ToolFilter {
            allow: vec!["github__*".into()],
            deny: vec!["github__delete_*".into()],
        };
        let compiled = filter.compile().unwrap().unwrap();
        assert!(compiled.admits("github__list_issues"));
        assert!(!compiled.admits("github__delete_issue"));
        assert!(!compiled.admits("gitlab__list_issues"));

        let empty = ToolFilter::default().compile().unwrap();
        assert!(empty.is_none());
    }

    #[test]
    fn server_name_validation_rejects_double_underscore_and_empty() {
        assert!(validate_server_name("github").is_ok());
        assert!(validate_server_name("a-b_c1").is_ok());
        assert!(validate_server_name("a__b").is_err());
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("-lead").is_err());
        assert!(validate_server_name("空格 name").is_err());
        assert!(validate_server_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn resolve_all_applies_project_over_toml_over_user() {
        let toml = McpDocument {
            servers: BTreeMap::from([
                (
                    "shared".to_owned(),
                    RawServerSpec {
                        command: Some("toml-command".into()),
                        ..Default::default()
                    },
                ),
                (
                    "only-toml".to_owned(),
                    RawServerSpec {
                        command: Some("t".into()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let json = JsonSource {
            project_files: vec![(
                PathBuf::from("/repo/.mcp.json"),
                BTreeMap::from([(
                    "shared".to_owned(),
                    RawServerSpec {
                        command: Some("project-command".into()),
                        ..Default::default()
                    },
                )]),
            )],
            user_file: Some((
                PathBuf::from("/home/mcp.json"),
                BTreeMap::from([(
                    "shared".to_owned(),
                    RawServerSpec {
                        command: Some("user-command".into()),
                        ..Default::default()
                    },
                )]),
            )),
        };
        let resolved = resolve_all(&toml, &json).unwrap();
        let by_name: BTreeMap<&str, &ServerConfig> =
            resolved.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(by_name.len(), 2);
        match &by_name["shared"].transport {
            Transport::Stdio { command, .. } => assert_eq!(command, "project-command"),
            other => panic!("{other:?}"),
        }
        match &by_name["only-toml"].transport {
            Transport::Stdio { command, .. } => assert_eq!(command, "t"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn discover_walks_up_to_git_root_and_prefers_dot_file() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join(".mcp.json"), "{}").unwrap();
        std::fs::write(nested.join("mcp.json"), "{}").unwrap();
        let found = discover_project_json(&nested, Some(dir.path()));
        assert_eq!(
            found,
            vec![nested.join("mcp.json"), dir.path().join(".mcp.json")]
        );
    }

    #[test]
    fn disabled_server_is_resolved_but_flagged() {
        let config = resolve_server(
            "x",
            &serde_json::from_value(json!({"command": "c", "enabled": false, "timeout_secs": 30}))
                .unwrap(),
            15,
            60,
        )
        .unwrap();
        assert!(!config.enabled);
        assert_eq!(config.call_timeout_secs, 30);
    }
}
