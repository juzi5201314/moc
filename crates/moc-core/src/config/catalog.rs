//! /config 的字段目录：字段说明与类型来自此处，值仍由配置 schema 校验。

use moc_llm::Protocol;
use moc_protocol::{ChoiceOption, ConfigField, FieldKind, SessionPermission};
use serde_json::{Value, json};

pub fn provider_template() -> Value {
    json!({"api":"openai_chat", "base_url":"", "api_key":"", "headers":{}, "extra_body":{},
        "timeout_secs":null, "connect_timeout_secs":20.0, "max_concurrency":0, "cache":"auto",
        "retry":{"max_attempts":10,"initial_backoff_secs":0.5,"max_backoff_secs":300.0,"jitter":true},
        "endpoint":"Default", "models":{}})
}

pub fn model_template() -> Value {
    json!({"context_window":200000,"max_output_tokens":32000,"max_request_bytes":null,"reasoning":false,
        "thinking_levels":null,"input":["text"],"pricing":null,"fast_mode":false,
        "compat":{"max_output_tokens_field":"max_tokens","supports_stop":true,
            "supports_prompt_cache_key":true,"reasoning_field":"reasoning_content",
            "reasoning_field_aliases":["reasoning_content","reasoning"],"tool_choice_dialect":"standard",
            "parallel_tool_calls":true,"developer_role":false,"response_format_dialect":"standard",
            "echo_reasoning":"protocol","requires_reasoning_content_for_tool_calls":false,
            "requires_reasoning_content_for_all_assistant_turns":false,"requires_assistant_content_for_tool_calls":false,
            "thinking":{"toggle_field":null,"enabled_value":"enabled","disabled_value":"disabled",
                "effort_field":"reasoning_effort","effort_map":{},"toggle_extra":{}},"extra_body":{},"headers":{}}})
}

/// 补齐可编辑的可选字段，仅用于页面展示，绝不回填进配置文件。
pub fn complete_view(value: &mut Value) {
    use super::editor::merge_json;
    let mut system = json!({"override":null,"append":null});
    merge_json(
        &mut system,
        value
            .get("system_prompt")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    value["system_prompt"] = system;
    if let Some(providers) = value.get_mut("providers").and_then(Value::as_object_mut) {
        for provider in providers.values_mut() {
            let mut defaults = provider_template();
            merge_json(&mut defaults, std::mem::take(provider));
            let provider_body = defaults["extra_body"].clone();
            let provider_headers = defaults["headers"].clone();
            if let Some(models) = defaults.get_mut("models").and_then(Value::as_object_mut) {
                for model in models.values_mut() {
                    let mut defaults = model_template();
                    merge_json(&mut defaults, std::mem::take(model));
                    let mut body = provider_body.clone();
                    merge_json(&mut body, defaults["compat"]["extra_body"].clone());
                    defaults["compat"]["extra_body"] = body;
                    let mut headers = provider_headers.clone();
                    merge_json(&mut headers, defaults["compat"]["headers"].clone());
                    defaults["compat"]["headers"] = headers;
                    *model = defaults;
                }
            }
            *provider = defaults;
        }
    }
    if let Some(servers) = value
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(Value::as_object_mut)
    {
        for server in servers.values_mut() {
            let mut defaults = json!({"type":null,"command":null,"args":[],"env":{},"cwd":null,"url":null,"headers":{},"enabled":true,"timeout_secs":null,"tools":{"allow":[],"deny":[]}});
            merge_json(&mut defaults, std::mem::take(server));
            *server = defaults;
        }
    }
    if let Some(patterns) = value
        .get_mut("defaults")
        .and_then(|defaults| defaults.get_mut("models"))
        .and_then(Value::as_object_mut)
    {
        for pattern in patterns.values_mut() {
            let mut defaults = model_template();
            merge_json(&mut defaults, std::mem::take(pattern));
            *pattern = defaults;
        }
    }
}

pub fn fields(effective: &Value) -> Vec<ConfigField> {
    let mut output = Vec::new();
    walk(effective, &mut Vec::new(), None, &mut output);
    super::parameters::append_fields(effective, &mut output);
    output
}

/// 同一分组内的展示顺序：越靠前的越常用、越关键；同档内保持字母序。
fn rank(key: &str) -> usize {
    match key {
        // 沙盒：先模式与网络，再越界策略与路径
        "default_mode" => 0,
        "network" => 1,
        "on_denied" => 2,
        "allow_unsandboxed" => 3,
        "overlay_home" => 4,
        "extra_read_paths" => 5,
        "extra_write_paths" => 6,
        "deny_read_paths" => 7,
        "deny_write_paths" => 8,
        // 供应商：先地址与密钥，再协议与缓存
        "base_url" => 0,
        "api_key" => 1,
        "api" => 2,
        "cache" => 3,
        "max_concurrent_calls" => 4,
        "timeout_secs" => 5,
        // 重试
        "max_attempts" => 0,
        "initial_backoff_secs" => 1,
        "max_backoff_secs" => 2,
        "jitter" => 3,
        // 模型：先规模，再能力，最后价格与兼容
        "context_window" => 0,
        "max_output_tokens" => 1,
        "max_request_bytes" => 2,
        "max_bytes" => 2,
        "max_chars" => 3,
        "reasoning" => 3,
        "thinking_levels" => 4,
        "input" => 5,
        // 会话、工作区与实验特性
        "clm" => 0,
        "ptc" => 1,
        "max_concurrency_ceiling" => 1,
        "world_run_timeout_ms" => 2,
        "file_read_bytes" => 3,
        "log_bytes" => 4,
        "ask_max_repairs" => 5,
        "max_scan_entries" => 1,
        "compress" => 0,
        "dir" => 1,
        "retention" => 2,
        "artifacts_days" => 1,
        "logs_days" => 2,
        // 子代理与角色
        "explore" => 0,
        "max_depth" => 1,
        // 上下文指令
        "user" => 1,
        "project" => 2,
        "max_tokens" => 3,
        "import_depth" => 4,
        "override" => 0,
        "append" => 1,
        // MCP 与插件
        "user_config" => 1,
        "project_config" => 2,
        "connect_timeout_secs" => 3,
        "call_timeout_secs" => 4,
        "max_plugins" => 10,
        "load_timeout_ms" => 11,
        "max_prompt_chars" => 12,
        // 网络搜索与抓取
        "provider" => 1,
        // PTC 沙盒
        "default_timeout_secs" => 0,
        "max_pending_calls" => 5,
        "max_total_calls" => 6,
        "max_concurrent_runs" => 7,
        "host_idle_timeout_secs" => 8,
        "max_state_bytes" => 9,
        "memory_limit_bytes" => 10,
        "max_result_bytes" => 11,
        // Todo 工具
        "reminders" => 1,
        "reminders_after" => 2,
        "reminders_max" => 3,
        // Bash 资源与工具限制
        "max_length_continues" => 0,
        "loop_detection" => 1,
        "connect_deadline_secs" => 2,
        "request_warn_secs" => 3,
        // 技能来源
        "moc_user" => 1,
        "moc_project" => 2,
        "agents_user" => 3,
        "agents_project" => 4,
        "dirs" => 5,
        "disable" => 6,
        "catalog_budget_percent" => 7,
        "catalog_budget_min_tokens" => 8,
        "description_max_chars" => 9,
        // 上下文压缩
        "auto" => 1,
        "threshold" => 2,
        "mid_prompt" => 3,
        "auto_continue" => 4,
        "method" => 5,
        "keep_recent_tokens" => 7,
        "protect_recent_tool_tokens" => 8,
        "shake" => 10,
        "foreground_after_secs" => 0,
        "monitored_timeout_secs" => 1,
        "stall_after_secs" => 2,
        "max_timeout_secs" => 3,
        "max_output_bytes" => 4,
        "persistent_log_bytes" => 5,
        "prefer_builtin" => 6,
        // bash 资源限制：开关在前，CPU → 软限 → 硬顶 → swap
        "cpu_cores" => 1,
        "memory_high_bytes" => 2,
        "memory_max_bytes" => 3,
        "memory_swap_bytes" => 4,
        "max_files" => 0,
        "max_matches_per_file" => 1,
        "max_matches_total" => 2,
        "line_max_chars" => 3,
        "context_before" => 4,
        "context_after" => 5,
        "timeout_ms" => 6,
        "max_file_bytes" => 7,
        "max_candidate_files" => 8,
        "cross_filesystems" => 9,
        "output_bytes" => 10,
        "head_bytes" => 0,
        "tail_bytes" => 1,
        // 通用
        "enabled" => 0,
        "url" => 0,
        "command" => 0,
        "args" => 1,
        "env" => 2,
        "cwd" => 3,
        "type" => 4,
        "headers" => 5,
        _ => 100,
    }
}

/// 只限制编码器不使用的协议兼容项；历史回传等通用行为仍保留。
fn compat_field_applies(api: Option<Protocol>, key: &str) -> bool {
    match key {
        "max_output_tokens_field"
        | "reasoning_field"
        | "reasoning_field_aliases"
        | "thinking"
        | "developer_role" => api == Some(Protocol::OpenAiChat),
        "supports_stop" => matches!(
            api,
            Some(Protocol::OpenAiChat | Protocol::AnthropicMessages)
        ),
        "supports_prompt_cache_key" => {
            matches!(api, Some(Protocol::OpenAiChat | Protocol::OpenAiResponses))
        }
        _ => true,
    }
}

fn walk(
    value: &Value,
    path: &mut Vec<String>,
    api: Option<Protocol>,
    output: &mut Vec<ConfigField>,
) {
    // 动态插件配置由插件声明字段后补入；目录扫描阶段不猜测 api_key/provider 等语义。
    if path.first().is_some_and(|part| part == "plugins")
        && path.get(1).is_some_and(|name| super::is_plugin_key(name))
    {
        return;
    }
    let api = if path.len() == 2 && path[0] == "providers" {
        value.get("api").and_then(super::parameters::protocol)
    } else {
        api
    };
    // 在兼容项的根部过滤整棵子树，不删除已存配置；无所属 API 的模型默认规则不受限。
    if path.len() == 6
        && path[0] == "providers"
        && path[2] == "models"
        && path[4] == "compat"
        && !compat_field_applies(api, &path[5])
    {
        return;
    }
    let object_editor = path.last().is_some_and(|key| {
        matches!(
            key.as_str(),
            "headers"
                | "env"
                | "extra_body"
                | "effort_map"
                | "toggle_extra"
                | "pricing"
                | "endpoint"
        )
    });
    if let Value::Object(values) = value
        && !object_editor
    {
        // 对象键按配置顺序无关紧要，这里按展示顺序重排，同档保持原有字母序。
        let mut keys: Vec<&String> = values.keys().collect();
        keys.sort_by_key(|key| rank(key));
        for key in keys {
            path.push(key.clone());
            walk(&values[key], path, api, output);
            path.pop();
        }
        return;
    }
    let key = path.last().map(String::as_str).unwrap_or_default();
    let joined = path.join(".");
    let (label, description) = describe(&joined, key);
    let advanced = path.iter().any(|part| part == "compat")
        || (path.first().is_some_and(|part| part == "tools")
            && path.get(1).is_some_and(|part| part == "ptc")
            && !matches!(key, "default_timeout_secs" | "max_timeout_secs"))
        || (path.first().is_some_and(|part| part == "workflow") && key != "enabled")
        || (path.first().is_some_and(|part| part == "plugins")
            && matches!(
                key,
                "max_plugins"
                    | "memory_limit_bytes"
                    | "load_timeout_ms"
                    | "call_timeout_ms"
                    | "max_prompt_chars"
            ))
        || matches!(
            key,
            "noop_loop_threshold"
                | "hash_echo_min_hits"
                | "max_candidate_files"
                | "max_scan_entries"
                | "catalog_budget_min_tokens"
                | "description_max_chars"
                | "line_max_bytes"
                | "max_request_bytes"
                | "output_bytes"
                | "connect_deadline_secs"
                | "request_warn_secs"
                | "overlay_home"
                | "deny_read_paths"
                | "deny_write_paths"
        );
    let tab = if path.first().is_some_and(|part| part == "providers") {
        if path.get(2).is_some_and(|part| part == "models") {
            "模型"
        } else {
            "供应商"
        }
    } else if path.first().is_some_and(|part| part == "models") {
        "模型"
    } else {
        match path.first().map_or("", String::as_str) {
            "tools" | "web_search" | "web_fetch" => "工具",
            "sandbox" => "沙盒",
            "compaction" | "limits" => "上下文",
            "context_files" | "system_prompt" => "指令",
            "defaults" | "agents" => "模型",
            "skills" => "技能",
            "mcp" => "MCP",
            "plugins" => "插件",
            _ => "会话",
        }
    };
    let options: &[(&str, &str)] = match key {
        "api" => &[
            ("openai_chat", "OpenAI Chat"),
            ("openai_responses", "OpenAI Responses"),
            ("anthropic_messages", "Anthropic Messages"),
        ],
        "cache" => &[
            ("auto", "自动"),
            ("short", "短缓存"),
            ("long", "长缓存"),
            ("off", "不缓存"),
        ],
        "overflow" => &[("error", "直接报错"), ("wait", "等待空位")],
        "max_output_tokens_field" => &[
            ("max_tokens", "max_tokens"),
            ("max_completion_tokens", "max_completion_tokens"),
        ],
        "tool_choice_dialect" if api.is_some_and(|api| api != Protocol::OpenAiChat) => {
            &[("standard", "标准"), ("unsupported", "不发送")]
        }
        "tool_choice_dialect" => &[
            ("standard", "标准"),
            ("any_for_required", "required 改写为 any"),
            ("unsupported", "不发送"),
        ],
        "response_format_dialect" => &[("standard", "标准"), ("unsupported", "不发送")],
        "echo_reasoning" => &[
            ("protocol", "按协议默认"),
            ("always", "总是"),
            ("never", "从不"),
        ],
        "provider" if path.first().is_some_and(|part| part == "web_search") => {
            &[("tavily", "Tavily")]
        }
        "mode" if path.iter().any(|part| part == "threshold") => {
            &[("percent", "按上下文比例"), ("tokens", "按固定长度")]
        }
        "method" if path.first().is_some_and(|part| part == "compaction") => &[
            ("moc", "Moc（本地修剪 + 摘要）"),
            ("grok_build", "Grok Build"),
            ("eocc", "EoCC（实验性交接状态）"),
        ],
        "default_mode" if path.first().is_some_and(|part| part == "sandbox") => {
            let support = crate::sandbox::detect();
            if support.supports(SessionPermission::Workspace)
                && support.supports(SessionPermission::WorkspaceOnly)
            {
                &[
                    ("full", "full（无沙盒）"),
                    ("workspace", "workspace（工作区可写，外部只读）"),
                    ("workspace_only", "workspace-only（仅工作区可写）"),
                ]
            } else if support.supports(SessionPermission::Workspace) {
                &[
                    ("full", "full（无沙盒）"),
                    ("workspace", "workspace（工作区可写，外部只读）"),
                ]
            } else {
                &[("full", "full（当前平台无沙盒 runner）")]
            }
        }
        "on_denied" if path.first().is_some_and(|part| part == "sandbox") => {
            &[("ask", "向用户申请授权"), ("deny", "直接拒绝")]
        }
        _ => &[],
    };
    let kind = if !options.is_empty() {
        FieldKind::Choice(
            options
                .iter()
                .map(|(value, label)| ChoiceOption::new(value, label))
                .collect(),
        )
    } else if key == "input" && path.iter().any(|part| part == "models") {
        FieldKind::Choice(vec![
            ChoiceOption::new("text", "仅文本 (text)"),
            ChoiceOption::new("text, image", "文本与图片 / 多模态 (text, image)"),
        ])
    } else if matches!(key, "api_key") {
        FieldKind::Secret
    } else if object_editor {
        FieldKind::Object
    } else if matches!(key, "override" | "append") {
        FieldKind::Multiline
    } else if matches!(key, "thinking_levels" | "input" | "args" | "allow" | "deny") {
        FieldKind::Strings
    } else if matches!(
        key,
        "timeout_secs" | "connect_timeout_secs" | "max_request_bytes"
    ) {
        FieldKind::Number
    } else {
        match value {
            Value::Bool(_) => FieldKind::Bool,
            Value::Number(n) if n.is_f64() => FieldKind::Number,
            Value::Number(_) => FieldKind::Integer,
            Value::Array(_) => FieldKind::Strings,
            Value::Object(_) => FieldKind::Object,
            _ => FieldKind::Text,
        }
    };
    output.push(ConfigField {
        path: path.clone(),
        tab: tab.into(),
        label: label.into(),
        description: if path.first().is_some_and(|part| part == "providers") {
            let start = if path.get(2).is_some_and(|part| part == "models") {
                4
            } else {
                2
            };
            format!("`{}`：{description}", path[start..].join("."))
        } else {
            description.into()
        },
        kind,
        advanced,
    });
}

fn describe<'a>(path: &'a str, key: &'a str) -> (&'a str, &'a str) {
    match path {
        "features.clm" => ("CLM 引擎", "启用实验性 CLM 会话引擎。"),
        "features.ptc" => (
            "PTC 代码沙盒",
            "启用 PTC（Programmatic Tool Calling）模式：模型通过 execute 沙盒编排工具调用。",
        ),
        "agents.max_depth" => (
            "最大嵌套深度",
            "允许子代理嵌套委托的最大层数；1 表示仅主会话可派发子代理。",
        ),
        "models.roles.explore" => (
            "探索",
            "Explore 角色使用的模型；inherit 沿用主模型，none 禁用。",
        ),
        "models.fusion.lead" => (
            "Fusion Lead 模型",
            "负责规划、调查与评审的主导模型（格式：供应商/模型[:等级]）。",
        ),
        "models.fusion.sidekick" => (
            "Fusion Sidekick 模型",
            "负责具体实现与测试的执行模型（格式：供应商/模型[:等级]）。",
        ),
        "context_files.enabled" => ("启用 AGENTS.md", "关闭后不再自动加载 AGENTS.md。"),
        "context_files.user" => (
            "全局 AGENTS.md",
            "加载用户全局目录（~/.moc/AGENTS.md 等）中的上下文文件。",
        ),
        "context_files.project" => (
            "项目 AGENTS.md",
            "加载工作区中的 AGENTS.md 及其 @引用的文件。",
        ),
        "context_files.max_tokens" => (
            "AGENTS.md token 上限",
            "所有 AGENTS.md 合计的最大 token 数，超出部分省略。",
        ),
        "context_files.import_depth" => (
            "@引用展开层数",
            "AGENTS.md 中 @文件引用的最大递归展开层数。",
        ),
        "system_prompt.override" => ("覆盖 System Prompt", "完全替换默认的 System Prompt。"),
        "system_prompt.append" => (
            "追加 System Prompt",
            "在默认 System Prompt 末尾追加自定义指令。",
        ),
        "skills.enabled" => ("启用 Skills", "关闭后不再加载任何 Skill。"),
        "skills.moc_user" => ("全局 moc Skills", "加载 ~/.moc/skills 中的 Skill。"),
        "skills.moc_project" => (
            "项目 moc Skills",
            "加载工作区 .moc/skills 中的 Skill，同名优先。",
        ),
        "skills.agents_user" => ("全局共享 Skills", "加载 ~/.agents/skills 中的共享 Skill。"),
        "skills.agents_project" => (
            "项目共享 Skills",
            "加载工作区 .agents/skills 中的共享 Skill。",
        ),
        "skills.dirs" => ("额外 Skills 目录", "逐行填写额外扫描的目录路径，支持 ~。"),
        "skills.disable" => ("禁用的 Skills", "逐行填写要禁用的 Skill 名称。"),
        "skills.catalog_budget_percent" => (
            "目录预算比例 (%)",
            "Skills 目录最多占用上下文窗口的百分比；0 表示仅使用下限。",
        ),
        "skills.catalog_budget_min_tokens" => (
            "目录预算下限 (token)",
            "按比例计算的预算低于此值时使用此下限。",
        ),
        "skills.description_max_chars" => (
            "单个描述字符上限",
            "每条 Skill 描述在目录中最多显示的字符数。",
        ),
        "mcp.enabled" => ("启用 MCP", "关闭后不再连接任何 MCP 服务。"),
        "mcp.user_config" => ("全局 MCP 配置", "加载 ~/.moc/mcp.json 等用户级 MCP 配置。"),
        "mcp.project_config" => (
            "项目 MCP 配置",
            "加载工作区 .mcp.json、mcp.json 等项目级 MCP 配置；启用前请确认仓库可信。",
        ),
        "mcp.connect_timeout_secs" => (
            "连接超时（秒）",
            "启动 MCP 服务并拉取工具列表的最长等待时间。",
        ),
        "mcp.call_timeout_secs" => ("调用超时（秒）", "单次 MCP 工具调用的最长等待时间。"),
        "mcp.max_concurrent_calls" => (
            "单服务并发上限",
            "同一 MCP server 同时执行工具调用的上限；1 表示串行。",
        ),
        "workflow.enabled" => ("启用 Workflow", "关闭后 /workflow 与相关调用不可用。"),
        "workflow.max_concurrency_ceiling" => {
            ("并发执行上限", "Workflow 同时运行的并发子任务数上限。")
        }
        "workflow.world_run_timeout_ms" => (
            "脚本默认超时（毫秒）",
            "单次 Workflow 脚本运行的默认超时时间。",
        ),
        "workflow.file_read_bytes" => (
            "读取文件上限（字节）",
            "Workflow 内单次读取文件的最大字节数。",
        ),
        "workflow.log_bytes" => (
            "单条日志上限（字节）",
            "Workflow 输出单条日志的最大字节数。",
        ),
        "workflow.ask_max_repairs" => (
            "Schema 修复重试上限",
            "子任务结构化交付校验失败时的最大自动修复次数。",
        ),
        "plugins.enabled" => ("启用插件系统", "关闭后不再扫描与加载本地插件。"),
        "plugins.user" => ("全局插件目录", "扫描 ~/.moc/plugins 中的本地插件。"),
        "plugins.project" => (
            "项目插件目录",
            "扫描 .moc/plugins 中的项目插件（仅在可信仓库开启）。",
        ),
        "plugins.dirs" => ("额外插件目录", "逐行填写额外扫描的插件目录，支持 ~。"),
        "plugins.disable" => ("禁用插件列表", "逐行填写要禁用的插件名称。"),
        "plugins.max_plugins" => ("最大插件数", "允许同时加载的插件数量上限。"),
        "plugins.memory_limit_bytes" => (
            "插件内存上限（字节）",
            "QuickJS 宿主为插件分配的最大堆内存。",
        ),
        "plugins.load_timeout_ms" => ("加载超时（毫秒）", "初始化单个插件模块的最长等待时间。"),
        "plugins.call_timeout_ms" => (
            "调用超时（毫秒）",
            "插件钩子或工具单次调用的超时时间；超时后本进程内禁用该插件。",
        ),
        "plugins.max_prompt_chars" => (
            "提示词贡献上限（字符）",
            "所有插件向 System Prompt 注入文本的总字符上限。",
        ),
        "compaction.enabled" => ("启用自动压缩", "关闭后不再自动压缩对话历史。"),
        "sandbox.default_mode" => (
            "默认沙盒模式",
            "新会话初始模式：full 无隔离；workspace 工作区可写、外部只读；workspace-only 仅工作区可读写。",
        ),
        "sandbox.network" => (
            "允许沙盒网络",
            "仅约束沙盒内 bash；web_fetch、web_search 与 MCP 不受影响。",
        ),
        "sandbox.on_denied" => (
            "越界处理策略",
            "沙盒拦截时：ask 弹窗申请授权；deny 直接拒绝。",
        ),
        "sandbox.allow_unsandboxed" => (
            "允许沙盒外重试",
            "允许模型在失败后申请以 full 模式在沙盒外重试一次。",
        ),
        "sandbox.overlay_home" => (
            "HOME Copy-on-Write",
            "Linux 专属：将 $HOME 挂为会话级写时复制层，缓存可写但不污染真实主目录。",
        ),
        "sandbox.extra_read_paths" => (
            "额外只读路径",
            "逐行填写允许读取的目录或文件；支持 ~，必须存在。",
        ),
        "sandbox.extra_write_paths" => (
            "额外可写路径",
            "逐行填写允许写入的目录或文件；支持 ~，必须存在，写入直接落盘。",
        ),
        "sandbox.deny_read_paths" => (
            "强制拒绝读取",
            "逐行填写路径；优先级高于允许列表，工作区内同样生效。",
        ),
        "sandbox.deny_write_paths" => (
            "强制拒绝写入",
            "逐行填写路径；优先级高于允许列表，工作区内同样生效。",
        ),
        "tools.ptc.default_timeout_secs" => {
            ("默认脚本超时（秒）", "单次 execute 脚本的默认超时时间。")
        }
        "tools.ptc.max_timeout_secs" => (
            "最大脚本超时（秒）",
            "单次 execute 脚本允许设置的最大超时秒数。",
        ),
        "tools.ptc.memory_limit_bytes" => {
            ("沙盒内存上限（字节）", "QuickJS 实例的最大堆内存限制。")
        }
        "tools.ptc.max_output_bytes" => (
            "脚本输出字节上限",
            "单次 execute 的日志与返回值预算，也限制脚本源码和挂起请求字节数。",
        ),
        "tools.ptc.max_result_bytes" => (
            "子调用结果字节上限",
            "单个宿主调用交给 execute 脚本读取的结果上限，超出时提供归档引用。",
        ),
        "tools.ptc.max_pending_calls" => (
            "挂起调用上限",
            "单个脚本同时处于等待状态的宿主工具调用数上限。",
        ),
        "tools.ptc.max_total_calls" => (
            "累计调用上限",
            "单个 execute 脚本允许发起的宿主工具调用总次数。",
        ),
        "tools.ptc.max_concurrent_runs" => (
            "并行脚本上限",
            "同一 Core 同时运行的 execute 脚本数量上限。",
        ),
        "tools.ptc.host_idle_timeout_secs" => (
            "Host 空闲超时（秒）",
            "PTC Host 无活跃脚本后自动回收的等待时间。",
        ),
        "tools.ptc.max_state_bytes" => (
            "持久化状态上限（字节）",
            "PTC 会话跨脚本持久化状态的最大字节数。",
        ),
        "tools.bash.limits.enabled" => (
            "限制 Bash 资源",
            "为模型发起的每条 bash 命令单独限制 CPU 与内存；手动 !命令不受影响。",
        ),
        "tools.bash.limits.cpu_cores" => (
            "CPU 核数上限",
            "0 表示不限；Linux 通过 cgroup CPUQuota 限核，Windows 通过 Job Object 限速。",
        ),
        "tools.bash.limits.memory_high_bytes" => (
            "内存软限（字节）",
            "超过后节流减速，0 表示不节流（仅 Linux，无 swap 时自动忽略）。",
        ),
        "tools.bash.limits.memory_max_bytes" => (
            "内存硬顶（字节）",
            "超过按 OOM 终止，0 表示不设，非 0 时须 ≥ 软限（仅 Linux）。",
        ),
        "tools.bash.limits.memory_swap_bytes" => (
            "Swap 预算（字节）",
            "单条命令最多可占用的 swap，0 表示不限（仅 Linux）。",
        ),
        "tools.read.default_limit" => ("默认读取行数", "未指定 limit 时每次读取文件的默认行数。"),
        "tools.read.max_bytes" => ("单次读取字节上限", "read 单次返回的最大字节数。"),
        "tools.read.line_max_bytes" => ("单行字节上限", "单行超过此字节数时截断显示。"),
        "tools.todo.enabled" => ("启用 Todo 工具", "允许模型维护任务清单与进度追踪。"),
        "tools.todo.reminders" => ("闲置提醒", "连续多轮未更新 Todo 时自动提醒模型。"),
        "tools.todo.reminders_after" => {
            ("触发提醒轮数", "连续多少次调用未更新清单后触发首次提醒。")
        }
        "tools.todo.reminders_max" => ("最多提醒次数", "同一轮任务内最多触发提醒的次数。"),
        "session.retention.enabled" => {
            ("自动清理过期数据", "定期清理超过保留天数的临时产物与日志。")
        }
        "compaction.threshold.mode" => {
            ("触发方式", "按上下文占用百分比，或按固定 token 阈值触发。")
        }
        "compaction.threshold.value" => ("触发阈值", "百分比模式填 1–100，固定模式填 token 数。"),
        "web_search.enabled" => ("启用", "关闭后 web_search 工具不再向模型开放。"),
        "web_search.provider" => ("供应商", "网页搜索服务商。"),
        "web_search.api_key" => (
            "API Key",
            "Tavily API Key，仅用于 web_search；支持 env:变量名 或 !命令。",
        ),
        "web_search.base_url" => (
            "Base URL",
            "自定义搜索 API 地址；留空使用官方 https://api.tavily.com。",
        ),
        "web_search.timeout_secs" => ("请求超时（秒）", "单次搜索请求的最长等待时间。"),
        "web_fetch.enabled" => ("启用", "关闭后 web_fetch 工具不再向模型开放。"),
        "web_fetch.timeout_secs" => ("请求超时（秒）", "单次页面抓取的最长等待时间。"),
        "web_fetch.max_bytes" => ("响应字节上限", "单页原始响应的最大读取字节数；超出截断。"),
        "web_fetch.max_chars" => (
            "单页预览字符上限",
            "只限制网页预览，不裁切已保存正文；多页共享 16 KiB 展示预算。",
        ),
        _ if path.starts_with("mcp.servers.") && key == "timeout_secs" => (
            "调用超时（秒）",
            "该服务单次工具调用的最长等待时间；留空用全局设置。",
        ),
        _ if path.starts_with("mcp.servers.") && key == "max_concurrent_calls" => (
            "单服务并发上限",
            "该 MCP server 同时执行工具调用的上限；留空用全局设置。",
        ),
        _ => match key {
            "enabled" => ("启用", "关闭后不再使用该功能。"),
            "api" => ("API 协议", "按供应商支持的协议格式选择。"),
            "base_url" => (
                "Base URL",
                "服务端 API 地址；OpenAI 兼容接口通常自动补全 /v1。",
            ),
            "api_key" => ("API Key", "直接填写密钥，或写 env:变量名 / !命令读取。"),
            "headers" => ("附加请求头", "JSON 格式的自定义 HTTP 请求头。"),
            "extra_body" => (
                "扩展请求体（JSON）",
                "编辑完整请求参数或供应商扩展；已列出的参数也可在下方单独编辑。模型逐层覆盖供应商默认值，对象合并、数组替换。",
            ),
            "timeout_secs" => ("请求超时（秒）", "单次请求的最长等待时间；留空不限。"),
            "connect_timeout_secs" => (
                "连接超时（秒）",
                "建立网络连接的最长等待时间；留空用默认值。",
            ),
            "max_concurrency" => ("并发请求上限", "同时向该供应商发起的请求数；0 表示不限。"),
            "cache" => (
                "提示缓存策略",
                "auto 自动缓存，short 短缓存，long 长缓存；off 不发送缓存键、缓存选项或显式断点，上游仍可能自动缓存。",
            ),
            "max_attempts" => ("最大尝试次数", "单次请求总尝试次数（含首次）。"),
            "initial_backoff_secs" => ("初始退避（秒）", "首次重试前的等待时间。"),
            "max_backoff_secs" => ("最大退避（秒）", "退避等待时间的上限。"),
            "jitter" => ("退避抖动", "为重试等待加入随机抖动，避免并发雪崩。"),
            "endpoint" => ("服务端点", "一般保持默认；Azure 需填写 api-version。"),
            "context_window" => (
                "Context Window",
                "模型最大上下文 token 数，用于估算压缩时机。",
            ),
            "max_output_tokens" => (
                "回复 token 上限",
                "单次回复的输出上限；Chat 对应 `max_tokens` 或 `max_completion_tokens`，Responses 对应 `max_output_tokens`，Anthropic 对应 `max_tokens`（含思考预算）。",
            ),
            "max_request_bytes" => (
                "请求体字节上限",
                "HTTP 请求体硬上限（默认 50 MiB，图片 Base64 合计另限 8 MiB）。",
            ),
            "reasoning" => ("思考推理", "模型是否支持思维链推理。"),
            "thinking_levels" => (
                "可选思考等级",
                "支持预设或自定义等级；未配置 API 默认强度时使用第一项，off 始终可用，会话显式选择优先。",
            ),
            "input" => ("输入模态", "模型支持接收 text 或 text + image 多模态输入。"),
            "fast_mode" => (
                "Fast Mode",
                "开启后模型选择器显示 Fast Mode 开关（请求携带 service_tier = \"fast\"）。",
            ),
            "pricing" => (
                "Pricing（每百万 token）",
                "JSON：input/output 必填，cache_read/cache_write 可选，单位 USD。",
            ),
            "prefer_builtin" => (
                "内置工具优先",
                "拒绝可由 read/grep/find/edit/write 等价完成的 bash 调用，过滤输出等正常命令不受影响。",
            ),
            "deny_patterns" => (
                "高危命令正则",
                "逐行填写正则表达式；命中的命令须先经用户确认方可执行。",
            ),
            "head_bytes" => ("超长输出保留开头", "输出截断时保留开头的字节预算。"),
            "tail_bytes" => ("超长输出保留结尾", "输出截断时保留结尾的字节数。"),
            "max_file_bytes" => ("单文件大小上限", "工具处理单个文件的最大字节数。"),
            "foreground_after_secs" => (
                "前台转后台阈值（秒）",
                "前台命令超过该时间后自动转为后台；tools.job.enabled 关闭时保持前台。",
            ),
            "monitored_timeout_secs" => (
                "命令默认超时（秒）",
                "后台受控命令的默认超时上限（不可超过 max_timeout_secs）。",
            ),
            "stall_after_secs" => (
                "无输出停滞提醒（秒）",
                "连续无新输出超过该时间后提醒模型检查状态；每次停滞仅提醒一次。",
            ),
            "max_timeout_secs" => ("命令最大超时（秒）", "单条受控命令允许设置的最大超时秒数。"),
            "max_output_bytes" => (
                "受控命令输出上限",
                "命令日志保留字节上限，超出时保留首尾各半并丢弃中段。",
            ),
            "persistent_log_bytes" => (
                "常驻任务日志上限",
                "persistent 进程日志保留字节上限，超出时保留首尾各半。",
            ),
            "cross_filesystems" => ("跨文件系统扫描", "grep/find 是否进入扫描根下的挂载点。"),
            "line_max_bytes" => ("单行字节上限", "单行超过该长度时截断并禁止整行编辑。"),
            "max_edits" => (
                "单次编辑处数上限",
                "单次 edit 工具调用允许包含的修改处数上限。",
            ),
            "noop_loop_threshold" => (
                "无效编辑提醒阈值",
                "同一处编辑连续无变更达到此次数时提醒模型重新读取文件。",
            ),
            "hash_echo_min_hits" => (
                "行号前缀误写保护",
                "检测到写回内容疑似误带 read 行号前缀达到此次数时拒绝写入。",
            ),
            "max_files" => ("结果文件数上限", "单次 grep 最多返回的文件数。"),
            "max_matches_per_file" => (
                "多文件搜索的每文件匹配上限",
                "搜索目录时，每个文件最多返回的匹配处数；单独搜索一个文件采用总匹配上限。",
            ),
            "max_matches_total" => (
                "总匹配上限",
                "单次 grep 最多返回的匹配总数，也是单文件搜索的匹配上限。",
            ),
            "line_max_chars" => ("匹配行字符上限", "搜索结果单行最多显示的字符数。"),
            "context_before" => ("上文行数", "每处匹配上方附带的上下文行数。"),
            "context_after" => ("下文行数", "每处匹配下方附带的上下文行数。"),
            "output_bytes" => (
                "输出字节上限",
                "单次搜索最多返回的字节数，超出后通过 skip 翻页。",
            ),
            "timeout_ms" => ("搜索超时（毫秒）", "单次搜索允许运行的最长时间。"),
            "max_concurrent_calls" => {
                ("工具并发上限", "单次工具批次中同时执行的只读工具数量上限。")
            }
            "max_candidate_files" => ("扫描文件数上限", "搜索时最多遍历的候选文件总数。"),
            "limit" => ("默认返回条数", "find 未指定 limit 时的默认最大结果数。"),
            "max_running" => ("后台任务上限", "每个会话同时运行的后台任务数上限。"),
            "max_wait_secs" => ("job wait 超时（秒）", "单次 job wait 最长阻塞等待秒数。"),
            "overflow" => ("超出并发策略", "超出后台任务上限时直接报错或排队等待。"),
            "method" => (
                "压缩算法",
                "moc：本地修剪 + 摘要；grok_build：9 段摘要；eocc：交接状态压缩（后两者忽略保留 token 与本地修剪参数）。",
            ),
            "keep_recent_tokens" => ("保留最近对话 (token)", "压缩后保留的最近对话预算。"),
            "protect_recent_tool_tokens" => (
                "保护最近工具结果 (token)",
                "压缩时不清理最近该预算内的工具结果。",
            ),
            "mid_prompt" => ("回合中途压缩", "长工具链执行期间也检查并触发压缩。"),
            "auto" => (
                "自动压缩",
                "上下文接近上限时自动触发压缩；关闭后仅手动触发。",
            ),
            "auto_continue" => ("压缩后自动继续", "自动压缩完成后继续当前回合执行。"),
            "soft_trim_chars" => (
                "软修剪字符阈值",
                "超过该长度的旧工具结果将被保留首尾、折叠中间内容。",
            ),
            "head_chars" => ("软修剪保留开头", "软修剪时保留开头的字符数。"),
            "tail_chars" => ("软修剪保留结尾", "软修剪时保留结尾的字符数。"),
            "hard_clear_prompts" => (
                "硬清理轮数阈值",
                "距今超过该用户轮次的旧工具结果将被完全清空内容。",
            ),
            "keep_last_prompts" => ("保护最近轮数", "最近这些用户轮次内的工具结果不会被硬清理。"),
            "supersede_reads" => (
                "去重旧文件读取",
                "同一文件被重新读取后，自动清理更早的读取结果。",
            ),
            "max_length_continues" => (
                "截断自动续写次数",
                "回复因输出上限截断后自动续写的最大次数。",
            ),
            "loop_detection" => ("死循环检测", "检测思考、正文与工具参数中出现的重复死循环。"),
            "connect_deadline_secs" => (
                "连接与重试硬时限（秒）",
                "流式请求收到响应头前或非流式全流程的硬超时（流式生成阶段不限时）。",
            ),
            "request_warn_secs" => (
                "请求慢响应提醒（秒）",
                "单次请求耗时超过该值时发出提醒通知，不中断请求。",
            ),
            "max_scan_entries" => ("文件查找扫描上限", "find 工具单次查找最多扫描的条目数。"),
            "url" => ("服务 URL", "远程 MCP 服务的 HTTP 地址。"),
            "compress" => (
                "压缩会话日志",
                "新会话以 zstd 格式保存（.jsonl.zst）；读取自动兼容未压缩历史。",
            ),
            "dir" => ("会话存储目录", "会话历史与运行记录存储路径（重启生效）。"),
            "artifacts_days" => ("临时产物保留天数", "超过该天数且无引用的产物可被清理。"),
            "logs_days" => ("任务日志保留天数", "超过该天数且无引用的日志可被清理。"),
            "command" => ("启动命令", "stdio 模式下运行的本地可执行程序。"),
            "args" => ("启动参数", "逐行填写命令行参数。"),
            "env" => ("环境变量", "JSON 键值对，附加到该进程的运行环境。"),
            "cwd" => ("工作目录", "MCP 进程的工作目录。"),
            "type" => ("传输方式", "stdio 为本地进程，http 为远程服务。"),
            "allow" => (
                "工具白名单",
                "逐行填写允许暴露的工具名，支持 * 通配；留空不限制。",
            ),
            "deny" => ("工具黑名单", "逐行填写禁用的工具名，支持 * 通配。"),
            "supports_stop" => (
                "支持停止序列",
                "关闭后不发送 `stop` / `stop_sequences`，包括扩展请求体中的设置。",
            ),
            "supports_prompt_cache_key" => (
                "支持提示缓存键",
                "关闭后不发送 `prompt_cache_key`、`prompt_cache_options` 和 `prompt_cache_retention`。",
            ),
            "max_output_tokens_field" => (
                "Chat 输出上限字段名",
                "Chat 使用 `max_tokens` 或 `max_completion_tokens`；不影响 Responses 与 Anthropic。",
            ),
            "reasoning_field" => ("Reasoning 字段名", "回传思考内容时使用的 JSON 字段名。"),
            "reasoning_field_aliases" => (
                "Reasoning 响应别名",
                "逐行填写解析响应思考内容时识别的字段别名。",
            ),
            "tool_choice_dialect" => ("tool_choice 方言", "按网关兼容性选择标准、改写或不发送。"),
            "parallel_tool_calls" => ("并行工具调用", "上游接口是否支持 parallel_tool_calls。"),
            "developer_role" => (
                "支持 developer 角色",
                "关闭后将 developer 消息降级为 system 角色发送。",
            ),
            "response_format_dialect" => ("response_format 方言", "结构化输出协议兼容模式。"),
            "echo_reasoning" => ("回传历史思考", "多轮工具调用时是否按协议回传历史思考内容。"),
            "requires_reasoning_content_for_tool_calls" => (
                "工具消息需携带思考字段",
                "发送工具调用历史时强制保留思考内容字段。",
            ),
            "requires_reasoning_content_for_all_assistant_turns" => (
                "全助手轮次需携带思考字段",
                "所有历史助手消息都强制携带思考内容字段。",
            ),
            "requires_assistant_content_for_tool_calls" => (
                "工具消息需携带 content",
                "包含工具调用的历史消息强制携带非空 content 字段。",
            ),
            "toggle_field" => (
                "思考开关字段名",
                "请求体中控制思考开关的字段名；留空不发送。",
            ),
            "enabled_value" => ("开启取值", "请求体中表示开启思考的字段值。"),
            "disabled_value" => ("关闭取值", "请求体中表示关闭思考的字段值。"),
            "effort_field" => (
                "思考强度字段名",
                "请求体中控制思考强度的字段名；留空不发送。",
            ),
            "effort_map" => (
                "思考等级映射",
                "将思考等级映射为上游接口接受的值；未配置项原样发送。",
            ),
            "toggle_extra" => ("思考附加字段", "JSON：合并到思考开关对象中的附加字段。"),
            _ => (key, "自定义配置项。"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice_options(value: &Value, key: &str) -> Vec<String> {
        let fields = fields(value);
        let field = fields
            .iter()
            .find(|field| {
                field.path.len() >= 2
                    && field.path[field.path.len() - 2] == "compat"
                    && field.path.last().is_some_and(|name| name == key)
            })
            .unwrap_or_else(|| panic!("{key} 出现在配置目录"));
        let FieldKind::Choice(options) = &field.kind else {
            panic!("{key} 应为候选列表");
        };
        options.iter().map(|option| option.value.clone()).collect()
    }

    #[test]
    fn echo_reasoning_template_and_choices_stay_inside_the_schema() {
        let template = model_template();
        let value = template["compat"]["echo_reasoning"].clone();
        serde_json::from_value::<moc_llm::ReasoningEcho>(value)
            .expect("模型模板写出的 echo_reasoning 必须是合法枚举值");
        let options = choice_options(
            &json!({"providers": {"p": {"models": {"m": template}}}}),
            "echo_reasoning",
        );
        assert!(!options.is_empty(), "候选列表不得为空");
        for option in &options {
            serde_json::from_value::<moc_llm::ReasoningEcho>(json!(option))
                .unwrap_or_else(|error| panic!("候选值 {option} 不在 schema 内：{error}"));
        }
    }

    #[test]
    fn provider_compat_fields_follow_api_without_removing_saved_values() {
        for (api, chat, stop, cache) in [
            ("openai_chat", true, true, true),
            ("openai_responses", false, false, true),
            ("anthropic_messages", false, true, false),
        ] {
            let mut effective = json!({"providers":{"p.with.dot":{
                "api":api,"models":{"m.with.dot":{"compat":{
                    "thinking":{"effort_field":"vendor_effort"}
                }}}
            }}});
            complete_view(&mut effective);
            let before = effective.clone();
            let catalog = fields(&effective);
            for (key, visible) in [
                ("max_output_tokens_field", chat),
                ("reasoning_field", chat),
                ("reasoning_field_aliases", chat),
                ("thinking", chat),
                ("developer_role", chat),
                ("supports_stop", stop),
                ("supports_prompt_cache_key", cache),
                ("tool_choice_dialect", true),
                ("response_format_dialect", true),
                ("parallel_tool_calls", true),
                ("echo_reasoning", true),
                ("requires_reasoning_content_for_tool_calls", true),
                ("requires_reasoning_content_for_all_assistant_turns", true),
                ("requires_assistant_content_for_tool_calls", true),
                ("extra_body", true),
                ("headers", true),
            ] {
                let prefix = [
                    "providers",
                    "p.with.dot",
                    "models",
                    "m.with.dot",
                    "compat",
                    key,
                ]
                .map(str::to_owned);
                assert_eq!(
                    catalog.iter().any(|field| field.path.starts_with(&prefix)),
                    visible,
                    "{api} 的 {key} 可见性"
                );
            }
            let expected = if chat {
                vec!["standard", "any_for_required", "unsupported"]
            } else {
                vec!["standard", "unsupported"]
            };
            assert_eq!(choice_options(&effective, "tool_choice_dialect"), expected);
            assert_eq!(effective, before, "隐藏字段不得修改配置值");
        }

        let mut defaults = json!({"defaults":{"models":{"*":{}}}});
        complete_view(&mut defaults);
        assert!(
            fields(&defaults).iter().any(|field| field.path
                == [
                    "defaults",
                    "models",
                    "*",
                    "compat",
                    "thinking",
                    "effort_field"
                ]),
            "跨 API 的模型默认规则仍可编辑协议兼容项"
        );
    }
}
