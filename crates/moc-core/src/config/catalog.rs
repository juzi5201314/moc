//! /config 的字段目录：字段说明与类型来自此处，值仍由配置 schema 校验。

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
        "thinking_levels":null,"input":["text"],"remote_compaction":null,"pricing":null,
        "compat":{"max_output_tokens_field":"max_tokens","supports_stop":true,
            "supports_prompt_cache_key":true,"reasoning_field":"reasoning_content",
            "reasoning_field_aliases":["reasoning_content","reasoning"],"tool_choice_dialect":"standard",
            "parallel_tool_calls":true,"developer_role":false,"instruction_role":"system",
            "instruction_system":["intro","mcp servers","environment"],"response_format_dialect":"standard",
            "echo_reasoning":"protocol","requires_reasoning_content_for_tool_calls":false,
            "requires_reasoning_content_for_all_assistant_turns":false,"requires_assistant_content_for_tool_calls":false,
            "thinking":{"toggle_field":"thinking","enabled_value":"enabled","disabled_value":"disabled",
                "effort_field":"reasoning_effort","effort_map":{},"toggle_extra":{}},"extra_body":{}}})
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
            if let Some(models) = defaults.get_mut("models").and_then(Value::as_object_mut) {
                for model in models.values_mut() {
                    let mut defaults = model_template();
                    merge_json(&mut defaults, std::mem::take(model));
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
    walk(effective, &mut Vec::new(), &mut output);
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
        // 模型角色：主模型 → 探索子代理 → 计划
        "default" => 0,
        "explore" => 1,
        "plan" => 2,
        "user" => 1,
        "project" => 2,
        "max_tokens" => 3,
        "import_depth" => 4,
        "user_config" => 1,
        "project_config" => 2,
        "connect_timeout_secs" => 3,
        "call_timeout_secs" => 4,
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
        "method_order" => 5,
        "soft_method" => 6,
        "keep_recent_tokens" => 7,
        "protect_recent_tool_tokens" => 8,
        "remote" => 9,
        "shake" => 10,
        "foreground_after_secs" => 0,
        "monitored_timeout_secs" => 1,
        "slow_warning_interval_secs" => 2,
        "max_timeout_secs" => 3,
        "max_output_bytes" => 4,
        "persistent_log_bytes" => 5,
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
        // OpenViking
        "recall" => 6,
        "capture" => 7,
        "skills" => 8,
        "peer_scope" => 9,
        "max_context_tokens" => 10,
        // 会话：压缩开关优先于存储目录与保留策略
        "compress" => 0,
        "dir" => 1,
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

fn walk(value: &Value, path: &mut Vec<String>, output: &mut Vec<ConfigField>) {
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
            walk(&values[key], path, output);
            path.pop();
        }
        return;
    }
    let key = path.last().map(String::as_str).unwrap_or_default();
    let joined = path.join(".");
    let (label, description) = describe(&joined, key);
    // 片段名清单由规范名单生成：字段说明与加载期校验共用同一份来源，不会漂移。
    let description = if key == "instruction_system" {
        format!(
            "{description}可用片段：{}。",
            crate::system_prompt::PIECE_NAMES.join("、")
        )
    } else {
        description.to_owned()
    };
    let advanced = path
        .iter()
        .any(|part| matches!(part.as_str(), "compat" | "shake"))
        || matches!(
            key,
            "noop_loop_threshold"
                | "hash_echo_min_hits"
                | "max_candidate_files"
                | "max_scan_entries"
                | "catalog_budget_min_tokens"
                | "description_max_chars"
                | "line_max_bytes"
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
            "tools" | "workspace" | "web_search" | "web_fetch" => "工具",
            "sandbox" => "沙盒",
            "compaction" | "limits" => "上下文",
            "context_files" | "system_prompt" | "sources" => "指令",
            "defaults" => "模型",
            "session" => "会话",
            "openviking" => "OpenViking",
            "skills" => "技能",
            "mcp" => "MCP",
            _ => "高级",
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
        "peer_scope" => &[("actor", "仅当前工作区"), ("all", "全部工作区")],
        "max_output_tokens_field" => &[
            ("max_tokens", "max_tokens"),
            ("max_completion_tokens", "max_completion_tokens"),
        ],
        "tool_choice_dialect" => &[
            ("standard", "标准"),
            ("any_for_required", "required 改写为 any"),
            ("unsupported", "不发送"),
        ],
        "response_format_dialect" => &[("standard", "标准"), ("unsupported", "不发送")],
        "instruction_role" => &[("system", "system 角色"), ("user", "user 角色")],
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
        "soft_method" => &[("moc", "Moc（当前实现）"), ("grok_build", "Grok Build")],
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
    } else if key == "remote_compaction" {
        FieldKind::Bool
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
        description,
        kind,
        advanced,
    });
}

fn describe<'a>(path: &'a str, key: &'a str) -> (&'a str, &'a str) {
    match path {
        "models.roles.default" => (
            "主模型",
            "日常对话使用的模型，格式为 供应商/模型[:等级]；省略等级时用模型的最高等级。",
        ),
        "models.roles.explore" => (
            "探索子代理模型",
            "供 agent/spawn 的 explore 角色使用；inherit 表示沿用主模型，省略等级时用模型的最高等级。",
        ),
        "models.roles.plan" => (
            "计划模型",
            "制定计划时使用的模型；none 表示沿用当前会话模型，inherit 表示沿用主模型，省略等级时用模型的最高等级。",
        ),
        "context_files.user" => ("用户指令", "加载用户目录下的 AGENTS.md。"),
        "context_files.project" => ("项目指令", "加载工作区中的 AGENTS.md 及其引用的文件。"),
        "context_files.enabled" => ("启用指令文件", "关闭后不再加载任何指令文件。"),
        "skills.enabled" => ("启用技能", "关闭后不再加载任何技能。"),
        "mcp.enabled" => ("启用 MCP", "关闭后不再连接任何 MCP 服务。"),
        "compaction.enabled" => ("启用压缩", "关闭后不再自动压缩对话历史。"),
        "sources.project_config" => (
            "允许项目配置",
            "允许项目里的 .moc/config.toml 覆盖全局设置；默认关闭，开启即信任该仓库。",
        ),
        "sandbox.default_mode" => (
            "默认沙盒模式",
            "新会话的初始模式。full 不隔离；workspace 工作区可写、工作区外只读；workspace-only 工作区外不可读写。原生 Windows 仅支持 full。",
        ),
        "sandbox.network" => (
            "允许沙盒网络",
            "仅约束沙盒内 bash；关闭后 Linux 使用独立网络命名空间，macOS 拒绝所有网络。web_fetch、web_search 与 MCP 不受影响。",
        ),
        "sandbox.on_denied" => (
            "越界处理",
            "沙盒拒绝文件访问或命令升级时：ask 向用户申请一次或本会话授权；deny 直接拒绝且不弹窗。",
        ),
        "sandbox.allow_unsandboxed" => (
            "允许沙盒外重试",
            "允许模型在执行失败后申请以 full 在沙盒外重试一次；关闭后只能调整配置或使用允许路径。",
        ),
        "sandbox.overlay_home" => (
            "HOME 写时复制",
            "Linux 专属：把 $HOME 挂成会话级 copy-on-write 层，cargo/npm/uv 等写缓存不会失败也不会污染真实主目录，会话结束丢弃。关闭后 HOME 只读。",
        ),
        "sandbox.extra_read_paths" => (
            "额外只读路径",
            "逐行填写目录或文件；支持 ~，必须存在。工作区外仍可读，但不能写。",
        ),
        "sandbox.extra_write_paths" => (
            "额外可写路径",
            "逐行填写目录或文件；支持 ~，必须存在。写入会真实落盘；权限高于 HOME overlay。",
        ),
        "sandbox.deny_read_paths" => (
            "强制拒绝读取",
            "逐行填写路径；优先于允许列表，即使位于工作区内也会被拒绝。不存在的路径无法在 bash 沙盒中遮蔽。",
        ),
        "sandbox.deny_write_paths" => (
            "强制拒绝写入",
            "逐行填写路径；优先于允许列表，即使位于工作区内也无法写入。不存在的路径无法在 bash 沙盒中遮蔽。",
        ),
        "tools.bash.limits.enabled" => (
            "限制 bash 资源",
            "为模型发起的每条 bash 单独设置 CPU 与内存上限；手打的 !命令不受影响。默认关闭。",
        ),
        "tools.bash.limits.cpu_cores" => (
            "CPU 核数上限",
            "0 表示不限。Linux 通过 cgroup CPUQuota 限制，nproc 与 cargo 的并发会跟着降到该值；Windows 通过 Job Object 只做限速，不改变并发度。",
        ),
        "tools.bash.limits.memory_high_bytes" => (
            "内存软限（字节）",
            "超过后开始节流，进程明显变慢但尽量不失败；0 表示不节流。仅 Linux 生效，宿主没有 swap 时自动忽略。示例：8 GiB = 8589934592。",
        ),
        "tools.bash.limits.memory_max_bytes" => (
            "内存硬顶（字节）",
            "兜底上限，超过按 OOM 终止；0 表示不设，非 0 时不得小于软限。仅 Linux 生效。示例：12 GiB = 12884901888。",
        ),
        "tools.bash.limits.memory_swap_bytes" => (
            "swap 预算（字节）",
            "单条命令最多可占用的 swap；0 表示不限制。仅 Linux 生效。示例：4 GiB = 4294967296。",
        ),
        "compaction.threshold.mode" => (
            "压缩触发方式",
            "按上下文占用的比例触发，或按固定的内容长度触发。",
        ),
        "compaction.threshold.value" => ("压缩触发值", "按比例时填 1–100，按长度时填具体数值。"),
        "openviking.url" => ("服务地址", "OpenViking 服务地址；留空则不连接。"),
        "openviking.api_key" => ("API 密钥", "访问 OpenViking 的密钥；留空表示无需鉴权。"),
        "openviking.timeout_secs" => ("请求超时（秒）", "单次请求等待 OpenViking 回复的最长时间。"),
        "web_search.enabled" => (
            "启用",
            "关闭后 web_search 工具不再向模型开放；不影响 web_fetch。",
        ),
        "web_search.provider" => ("供应商", "网页搜索服务供应商；空格键切换。"),
        "web_search.api_key" => (
            "API 密钥",
            "Tavily 密钥，仅用于 web_search；可填 env:变量名 或 !命令。",
        ),
        "web_search.base_url" => ("接口地址", "留空使用官方 https://api.tavily.com。"),
        "web_search.timeout_secs" => ("请求超时（秒）", "单次 Tavily 搜索请求的最长等待时间。"),
        "web_fetch.enabled" => (
            "启用",
            "关闭后 web_fetch 工具不再向模型开放；不影响 web_search。",
        ),
        "web_fetch.timeout_secs" => (
            "请求超时（秒）",
            "单次页面抓取（含站点 API 调用）的最长等待时间。",
        ),
        "web_fetch.max_bytes" => (
            "响应字节上限",
            "单页原始响应的最大读取字节数；超出部分截断并标记。",
        ),
        "web_fetch.max_chars" => (
            "正文字符上限",
            "单页转换后 Markdown 的最大字符数；超出部分截断并标记。",
        ),
        "mcp.connect_timeout_secs" => (
            "连接超时（秒）",
            "启动 MCP 服务并获取工具列表的最长等待时间。",
        ),
        _ if path.starts_with("mcp.servers.") && key == "timeout_secs" => (
            "调用超时（秒）",
            "该服务单次工具调用的最长等待时间；留空用全局设置。",
        ),
        _ => match key {
            "enabled" => ("启用", "关闭后不再使用该功能。"),
            "moc_user" => ("用户技能目录", "加载 ~/.moc/skills 里的技能。"),
            "moc_project" => (
                "项目技能目录",
                "加载项目 .moc/skills 里的技能，同名技能优先。",
            ),
            "agents_user" => ("用户共享技能", "加载 ~/.agents/skills 里的共享技能。"),
            "agents_project" => ("项目共享技能", "加载项目 .agents/skills 里的共享技能。"),
            "user_config" => ("用户 MCP 配置", "加载 ~/.moc/mcp.json 里定义的 MCP 服务。"),
            "project_config" => (
                "项目 MCP 配置",
                "加载项目里的 .mcp.json / mcp.json；其中的本地服务会被启动。默认关闭。",
            ),
            "api" => ("API 格式", "按供应商支持的协议选择；可在供应商文档中确认。"),
            "base_url" => ("服务地址", "供应商接口地址；通常会自动补上 /v1。"),
            "api_key" => ("API 密钥", "可直接填写，或写 env:变量名 从环境变量读取。"),
            "headers" => ("附加请求头", "JSON 形式的自定义请求头，例如 User-Agent。"),
            "extra_body" => ("附加请求体", "JSON 形式的额外请求字段，用于兼容网关。"),
            "timeout_secs" => ("请求超时（秒）", "单次模型请求的最长等待时间；留空不限。"),
            "connect_timeout_secs" => (
                "连接超时（秒）",
                "建立网络连接的最长等待时间；留空用默认值。",
            ),
            "max_concurrency" => ("并发请求上限", "同时向该供应商发起的请求数；0 表示不限。"),
            "cache" => ("提示缓存", "复用重复内容以降低费用；不确定就选自动。"),
            "max_attempts" => ("尝试次数", "一次请求最多尝试几次，含第一次在内。"),
            "initial_backoff_secs" => ("首次重试等待（秒）", "第一次重试前的等待时间。"),
            "max_backoff_secs" => ("重试等待上限（秒）", "重试等待不会超过这个时间。"),
            "jitter" => (
                "重试随机浮动",
                "给重试等待加入随机量，避免多个请求同时重试。",
            ),
            "endpoint" => ("服务端点", "一般不用改；Azure 服务需要填写接口版本。"),
            "context_window" => (
                "上下文长度",
                "模型一次能记住的内容量，用于估算何时需要压缩对话。",
            ),
            "max_output_tokens" => ("回复长度上限", "模型单次回复的最大长度。"),
            "max_request_bytes" => (
                "请求体字节上限",
                "图片请求超过该字节数时按旧到新淘汰图片；留空使用 50 MiB 默认值。",
            ),
            "reasoning" => (
                "支持思考推理",
                "模型是否具备深度思考与推理能力（例如 o1、o3、DeepSeek-R1、Claude 3.7 Sonnet）。",
            ),
            "thinking_levels" => (
                "可选思考等级",
                "支持 off、low、medium、high、xhigh、max（也支持中文低、中、高，用逗号或换行分隔；留空使用默认等级）。",
            ),
            "input" => (
                "输入模态",
                "模型支持接收的内容类型：仅文本 或 文本与图片/多模态。",
            ),
            "remote_compaction" => ("由服务端压缩", "留空自动判断；仅部分接口支持。"),
            "pricing" => (
                "价格（每百万 token）",
                "JSON：input/output 必填，cache_read/cache_write 可选，单位美元。",
            ),
            "deny_patterns" => (
                "禁止执行的命令",
                "逐行填写正则表达式，匹配到的命令始终拒绝执行，与沙盒模式无关。",
            ),
            "head_bytes" => (
                "超长输出保留开头",
                "所有工具与 MCP 输出共用的开头字节预算；超出首尾总预算时归档完整结果。",
            ),
            "tail_bytes" => ("超长输出保留结尾", "工具输出过长时保留结尾的字节数。"),
            "max_file_bytes" => ("单文件大小上限", "工具处理单个文件的最大字节数。"),
            "foreground_after_secs" => (
                "前台等待秒数",
                "前台 monitored 命令超过该时间后自动转为后台；tools.job.enabled 关闭时保持前台。",
            ),
            "monitored_timeout_secs" => (
                "有限任务超时（秒）",
                "monitored 命令的默认硬超时，必须不超过 max_timeout_secs。",
            ),
            "slow_warning_interval_secs" => (
                "慢任务告警间隔（秒）",
                "monitored 任务每达到一个间隔就在下一轮提示中告警。",
            ),
            "max_timeout_secs" => (
                "命令超时上限（秒）",
                "单条 monitored 命令允许设置的最大秒数。",
            ),
            "max_output_bytes" => (
                "有限任务输出上限",
                "monitored 命令最多写入日志的字节数，超过后终止命令。",
            ),
            "persistent_log_bytes" => (
                "长期任务日志容量",
                "persistent 任务保留的环形日志字节数，写满后覆盖最旧内容但不终止进程。",
            ),
            "cross_filesystems" => (
                "跨文件系统扫描",
                "grep/find 是否递归进入扫描根下的挂载点；默认关闭。",
            ),
            "line_max_bytes" => ("单行长度上限", "超长行只显示提示，不能再直接编辑。"),
            "max_edits" => ("单次编辑条数", "一次编辑最多包含的修改处数。"),
            "noop_loop_threshold" => (
                "重复编辑提醒次数",
                "同一处编辑连续多次没有变化时，提醒模型重新读取。",
            ),
            "hash_echo_min_hits" => (
                "整文件写回保护",
                "识别到写回内容带着工具标记时拒绝写入，避免污染源文件。",
            ),
            "max_files" => ("搜索文件数上限", "一次搜索最多显示多少个文件。"),
            "max_matches_per_file" => ("每个文件匹配上限", "单个文件最多返回多少处匹配。"),
            "max_matches_total" => ("匹配总数上限", "一次搜索最多返回多少处匹配。"),
            "line_max_chars" => ("搜索行长度上限", "搜索结果每行最多显示的字符数。"),
            "context_before" => ("匹配前附带行数", "每处匹配上方附带的行数。"),
            "context_after" => ("匹配后附带行数", "每处匹配下方附带的行数。"),
            "output_bytes" => (
                "搜索输出字节上限",
                "一次搜索最多返回多少字节的结果；超出后用 skip 继续。",
            ),
            "timeout_ms" => ("搜索超时（毫秒）", "一次搜索允许运行的最长时间。"),
            "max_concurrent_calls" => (
                "工具并发上限",
                "标准模式一次工具批次中同时执行的只读工具数量。",
            ),
            "max_candidate_files" => ("扫描文件上限", "搜索时最多检查多少个文件。"),
            "limit" => ("结果数量", "未指定时返回的结果条数。"),
            "max_running" => ("后台任务上限", "可同时运行的后台任务数量。"),
            "overflow" => ("超出上限时", "直接报错，或等待有空位再继续。"),
            "method_order" => (
                "压缩方式顺序",
                "逐行填写 remote（服务端）、shake（本地修剪）、soft（摘要），按顺序尝试；soft 的摘要实现由「Soft 摘要方式」决定。",
            ),
            "soft_method" => (
                "Soft 摘要方式",
                "选择 soft 压缩使用的摘要实现；remote 与 shake 不受影响。Grok Build 使用其 9 段摘要与 full-replace 历史，忽略「保留最近对话」和「保留最近工具结果」。",
            ),
            "keep_recent_tokens" => ("保留最近对话", "压缩后保留的最近对话长度（token）。"),
            "protect_recent_tool_tokens" => {
                ("保留最近工具结果", "压缩时不清理最近这部分工具结果。")
            }
            "mid_prompt" => ("任务中途压缩", "长任务执行过程中也检查是否需要压缩。"),
            "auto" => ("自动压缩", "上下文接近上限时自动压缩；关闭后仍可手动压缩。"),
            "auto_continue" => ("压缩后继续", "自动压缩完成后继续当前任务。"),
            "soft_trim_chars" => (
                "旧结果修剪阈值",
                "超过这个长度的旧工具结果会被首尾保留、中间省略。",
            ),
            "head_chars" => ("修剪保留开头", "修剪时保留开头的字符数。"),
            "tail_chars" => ("修剪保留结尾", "修剪时保留结尾的字符数。"),
            "hard_clear_prompts" => (
                "清空旧结果提示数",
                "超过这么多次提示之前的旧工具结果会被清空。",
            ),
            "keep_last_prompts" => ("保护最近提示数", "最近这些次提示的工具结果不会被清空。"),
            "supersede_reads" => (
                "丢弃过期的读取",
                "同一文件被重新读取后，旧的读取内容不再保留。",
            ),
            "server_side" => ("请求内压缩", "在请求中交给服务端压缩历史；仅部分模型支持。"),
            "retained_message_tokens" => {
                ("服务端压缩保留量", "压缩后仍保留的最近对话长度（token）。")
            }
            "max_length_continues" => (
                "中断后继续次数",
                "回复因长度上限中断后，自动接着生成的次数。",
            ),
            "loop_detection" => (
                "循环检测",
                "默认启用；检测思考、正文和工具参数里完全重复的内容。",
            ),
            "connect_deadline_secs" => (
                "连接与重试时限（秒）",
                "流式请求收到响应头之前、非流式请求整个请求的硬时限；模型生成阶段不受限制。",
            ),
            "request_warn_secs" => (
                "请求耗时警告（秒）",
                "单次请求超过这么久仍未完成时发一条警告通知，不终止请求。",
            ),
            "override" => ("基础系统提示", "替换默认的身份与规则说明。"),
            "append" => ("附加系统提示", "在系统提示末尾追加自己的长期要求。"),
            "max_tokens" => (
                "指令长度上限",
                "所有指令文件合计的最大长度（token），超出部分省略。",
            ),
            "import_depth" => ("指令引用层数", "指令文件中 @文件 引用最多展开几层。"),
            "watch" => ("工作区变更检测", "检测工具之外的文件改动并报告。"),
            "max_scan_entries" => ("工作区扫描上限", "一次扫描最多检查多少个文件条目。"),
            "url" => ("服务地址", "OpenViking 服务的完整地址。"),
            "recall" => ("自动召回记忆", "每次提示时自动查找相关的历史记忆。"),
            "capture" => ("自动记录经验", "把对话内容交给 OpenViking 提取记忆。"),
            "skills" => ("远程技能", "从 OpenViking 获取技能；本地同名技能优先。"),
            "peer_scope" => ("召回范围", "只召回当前工作区，或允许跨工作区。"),
            "max_context_tokens" => ("记忆注入上限", "每次注入的记忆内容长度（token）。"),
            "compress" => (
                "压缩会话日志",
                "新建会话以 zstd 压缩保存（.jsonl.zst）；已有会话保持原格式，读取时自动识别两种格式。",
            ),
            "dir" => ("会话存储目录", "会话和运行记录的保存位置；更改后重启生效。"),
            "artifacts_days" => (
                "临时产物保留天数",
                "过期且没有被引用的临时产物会被清理；维护前先预览。",
            ),
            "logs_days" => ("任务日志保留天数", "过期且没有被引用的任务日志会被清理。"),
            "call_timeout_secs" => ("调用超时（秒）", "一次 MCP 工具调用的最长等待时间。"),
            "dirs" => ("额外技能目录", "逐行填写目录路径，支持 ~。"),
            "disable" => ("禁用的技能", "逐行填写技能名，这些技能不会被加载。"),
            "catalog_budget_percent" => (
                "技能列表预算比例",
                "技能目录最多占用模型上下文窗口的百分比；0 表示只用下限。",
            ),
            "catalog_budget_min_tokens" => (
                "技能列表预算下限",
                "按比例算出的预算低于此值时改用此值，单位 token。",
            ),
            "description_max_chars" => ("单个技能说明长度", "每条技能说明最多显示多少字符。"),
            "command" => ("启动程序", "本地 MCP 服务要运行的可执行程序。"),
            "args" => ("启动参数", "逐行填写，每行一个参数。"),
            "env" => ("环境变量", "JSON 键值，追加到该服务的运行环境。"),
            "cwd" => ("工作目录", "MCP 程序的运行目录。"),
            "type" => ("连接方式", "stdio 为本地程序，http 为远程服务。"),
            "allow" => (
                "允许的工具",
                "逐行填写工具名，可用 * 通配；留空表示不限制。",
            ),
            "deny" => ("禁止的工具", "逐行填写工具名，可用 * 通配。"),
            "supports_stop" => ("支持停止词", "上游是否接受停止序列参数。"),
            "supports_prompt_cache_key" => ("支持缓存键", "上游是否接受缓存标识参数。"),
            "max_output_tokens_field" => ("输出上限字段名", "上游要求的字段名，二选一。"),
            "reasoning_field" => ("思维链字段名", "回传思考内容时使用的字段名。"),
            "reasoning_field_aliases" => ("思维链响应字段", "逐行填写上游返回思考内容时的字段名。"),
            "tool_choice_dialect" => ("工具选择格式", "按网关支持情况选择标准、改写或不发送。"),
            "parallel_tool_calls" => ("支持并行调用", "上游是否接受同时调用多个工具的请求。"),
            "developer_role" => (
                "支持 developer 角色",
                "关闭后 developer 消息会转成 system 发送。",
            ),
            "instruction_role" => (
                "指令角色",
                "工具政策、仓库规则等指令消息以哪个角色下发；user 用于对 system 遵循度不足的上游。",
            ),
            "instruction_system" => (
                "保持 system 的片段",
                "指令角色为 user 时仍留在 system 消息里的片段名，逐行填写；留空表示整条指令都用 user。",
            ),
            "response_format_dialect" => ("结构化输出", "上游不支持时选择不发送。"),
            "echo_reasoning" => ("回传历史思考", "按协议默认、总是或从不回传历史思考内容。"),
            "requires_reasoning_content_for_tool_calls" => (
                "工具历史需思考字段",
                "为工具调用历史补齐上游要求的思考字段。",
            ),
            "requires_reasoning_content_for_all_assistant_turns" => {
                ("助手历史需思考字段", "所有助手历史都携带思考字段。")
            }
            "requires_assistant_content_for_tool_calls" => {
                ("工具历史需正文", "工具调用消息必须带 content 字段。")
            }
            "toggle_field" => ("思考开关字段名", "请求中思考开关的字段名；留空不发送。"),
            "enabled_value" => ("开启值", "表示开启思考的取值。"),
            "disabled_value" => ("关闭值", "表示关闭思考的取值。"),
            "effort_field" => ("强度字段名", "请求中思考强度的字段名；留空不发送。"),
            "effort_map" => ("等级映射", "JSON：把通用思考等级映射为上游接受的值。"),
            "toggle_extra" => ("思考附加字段", "JSON：追加到思考对象内部的字段。"),
            _ => (key, "自定义配置项；修改前请确认该服务接受的值。"),
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
}
