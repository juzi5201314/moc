//! /config 的字段目录：字段说明与类型来自此处，值仍由配置 schema 校验。

use moc_protocol::{ConfigField, FieldKind};
use serde_json::{Value, json};

pub fn provider_template() -> Value {
    json!({"api":"openai_chat", "base_url":"", "api_key":"", "headers":{}, "extra_body":{},
        "timeout_secs":null, "connect_timeout_secs":20.0, "max_concurrency":0, "cache":"auto",
        "retry":{"max_attempts":10,"initial_backoff_secs":0.5,"max_backoff_secs":300.0,"jitter":true},
        "endpoint":"Default", "models":{}})
}

pub fn model_template() -> Value {
    json!({"context_window":200000,"max_output_tokens":32000,"reasoning":false,
        "thinking_levels":null,"input":["text"],"remote_compaction":null,"pricing":null,
        "compat":{"max_output_tokens_field":"max_tokens","supports_stop":true,
            "supports_prompt_cache_key":true,"reasoning_field":"reasoning_content",
            "reasoning_field_aliases":["reasoning_content","reasoning"],"tool_choice_dialect":"standard",
            "parallel_tool_calls":true,"developer_role":false,"response_format_dialect":"standard",
            "echo_reasoning":"auto","requires_reasoning_content_for_tool_calls":false,
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

fn walk(value: &Value, path: &mut Vec<String>, output: &mut Vec<ConfigField>) {
    let object_editor = path.last().is_some_and(|key| {
        matches!(
            key.as_str(),
            "headers"
                | "env"
                | "extra_body"
                | "effort_map"
                | "toggle_extra"
                | "approval"
                | "pricing"
                | "endpoint"
        )
    });
    if let Value::Object(values) = value
        && !object_editor
    {
        for (key, value) in values {
            path.push(key.clone());
            walk(value, path, output);
            path.pop();
        }
        return;
    }
    let key = path.last().map(String::as_str).unwrap_or_default();
    let joined = path.join(".");
    let (label, description) = describe(&joined, key);
    let advanced = path
        .iter()
        .any(|part| matches!(part.as_str(), "compat" | "shake"))
        || matches!(
            key,
            "noop_loop_threshold"
                | "hash_echo_min_hits"
                | "max_candidate_files"
                | "max_scan_entries"
                | "description_max_chars"
                | "line_max_bytes"
        );
    let tab = if path.first().is_some_and(|part| part == "providers") {
        if path.get(2).is_some_and(|part| part == "models") {
            "模型"
        } else {
            "供应商"
        }
    } else if path.first().is_some_and(|part| part == "models") {
        "模型"
    } else if matches!(
        key,
        "moc_user"
            | "moc_project"
            | "agents_user"
            | "agents_project"
            | "project_config"
            | "user_config"
    ) || (path.first().is_some_and(|part| part == "context_files")
        && matches!(key, "user" | "project"))
    {
        "来源"
    } else {
        match path.first().map(String::as_str).unwrap_or_default() {
            "tools" => "工具",
            "compaction" | "context_files" | "system_prompt" | "limits" | "workspace" => "上下文",
            "session" => "会话",
            "openviking" => "OpenViking",
            "skills" => "Skills",
            "mcp" => "MCP",
            _ => "高级",
        }
    };
    let options: &[&str] = match key {
        "api" => &["openai_chat", "openai_responses", "anthropic_messages"],
        "cache" => &["auto", "short", "long", "off"],
        "overflow" => &["error", "wait"],
        "approval_mode" => &["yolo", "prompt"],
        "peer_scope" => &["actor", "all"],
        "max_output_tokens_field" => &["max_tokens", "max_completion_tokens"],
        "tool_choice_dialect" => &["standard", "any_for_required", "unsupported"],
        "response_format_dialect" => &["standard", "unsupported"],
        "echo_reasoning" => &["auto", "always", "never"],
        "mode" if path.iter().any(|part| part == "threshold") => &["percent", "tokens"],
        _ => &[],
    };
    let kind = if !options.is_empty() {
        FieldKind::Choice(options.iter().map(|value| (*value).to_owned()).collect())
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
    } else if matches!(key, "timeout_secs" | "connect_timeout_secs") {
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
        description: description.into(),
        kind,
        advanced,
    });
}

fn describe<'a>(path: &'a str, key: &'a str) -> (&'a str, &'a str) {
    match path {
        "models.roles.default" => (
            "主模型",
            "默认会话使用的 provider/model[:思考等级]；首次启动至少配置一个模型。",
        ),
        "models.roles.smol" => ("轻量模型", "摘要等轻量工作使用的模型；inherit 继承主模型。"),
        "models.roles.slow" => ("深度模型", "需要深度分析时选择的模型；inherit 继承主模型。"),
        "models.roles.plan" => ("计划模型", "计划模式使用的模型；inherit 继承主模型。"),
        "context_files.user" => ("用户指令", "加载 MOC 用户目录中的 AGENTS.md。"),
        "context_files.project" => (
            "项目指令",
            "加载工作区祖先目录中的 AGENTS.md，并按配置展开引用。",
        ),
        "sources.project_config" => (
            "项目配置",
            "允许工作区 .moc/config.toml 覆盖全局配置；CLI 覆盖层仍具有最高优先级。",
        ),
        "compaction.threshold.mode" => (
            "压缩阈值类型",
            "percent 按上下文窗口百分比；tokens 使用固定 token 数。两者都不扣除模型输出上限。",
        ),
        "compaction.threshold.value" => (
            "压缩阈值",
            "百分比范围 1–100，默认 85；固定 token 必须大于 0。",
        ),
        "openviking.url" => ("服务 URL", "OpenViking 服务地址；留空即不连接。"),
        "openviking.api_key" => (
            "API 密钥",
            "直接填写 OpenViking 的 Bearer 密钥；留空不鉴权，不解析环境变量或命令。",
        ),
        "openviking.timeout_secs" => (
            "请求时限（秒）",
            "OpenViking 单次 HTTP 请求的总时限，包含读取完整响应。",
        ),
        "mcp.connect_timeout_secs" => (
            "初始化时限（秒）",
            "限制 MCP 连接初始化与工具列表获取，不是模型请求的建连时限。",
        ),
        _ if path.starts_with("mcp.servers.") && key == "timeout_secs" => (
            "工具调用时限（秒）",
            "此 MCP 服务单次工具调用的时限；留空继承 mcp.call_timeout_secs。",
        ),
        _ => match key {
            "enabled" => ("启用", "启用此分组的功能；关闭后不执行该功能。"),
            "moc_user" => (
                "用户 MOC Skills",
                "发现 ~/.moc/skills；默认启用全部有效技能，同名只保留高优先级来源。",
            ),
            "moc_project" => (
                "项目 MOC Skills",
                "发现项目 .moc/skills；同名技能优先于用户目录。",
            ),
            "agents_user" => (
                "用户共享 Skills",
                "发现 ~/.agents/skills；同名技能由更高优先级来源遮蔽。",
            ),
            "agents_project" => (
                "项目共享 Skills",
                "发现项目 .agents/skills；不会重复加载同名或同一真实路径的技能。",
            ),
            "user_config" => (
                "用户 MCP 文件",
                "加载 ~/.moc/mcp.json；原生 TOML 中的 MCP 设置不受此开关影响。",
            ),
            "project_config" => (
                "项目 MCP 文件",
                "加载项目 .mcp.json/mcp.json；其中 stdio 服务会启动本地程序。",
            ),
            "api" => (
                "API 格式",
                "按供应商实际支持的 Chat、Responses 或 Anthropic Messages 协议发送请求。",
            ),
            "base_url" => ("服务地址", "自动补 /v1；末尾加 # 则原样使用路径。"),
            "api_key" => (
                "API 密钥",
                "可填密钥、env:变量名或 !取密钥命令；界面默认遮蔽，空值表示不鉴权。",
            ),
            "headers" => (
                "附加请求头",
                "JSON 对象，例如自定义 User-Agent；不要把密钥提交到项目仓库。",
            ),
            "extra_body" => (
                "扩展请求体",
                "JSON 对象，追加兼容网关所需字段；标准请求字段优先。",
            ),
            "timeout_secs" => (
                "请求空闲时限（秒）",
                "留空不限时；流式约束首包和相邻数据间隔，非流式约束总时长。",
            ),
            "connect_timeout_secs" => (
                "连接时限（秒）",
                "只限制 DNS/TCP/TLS 建连，不限制模型思考；留空使用客户端设置。",
            ),
            "max_concurrency" => (
                "并发请求上限",
                "同一 Provider 在本 core 中共享名额，摘要与压缩也计入；0 表示不限。",
            ),
            "cache" => (
                "提示缓存",
                "auto 沿用协议默认；short 为短缓存；long 请求 Anthropic 1h 或 OpenAI 24h（需上游支持）；off 不发送缓存控制，不能禁止服务端隐式缓存。不额外预热。",
            ),
            "max_attempts" => ("请求尝试次数", "包含首次请求；失败重试不会超出此次数。"),
            "initial_backoff_secs" => ("初始重试间隔", "暂时性错误后的首个重试等待秒数。"),
            "max_backoff_secs" => ("重试间隔上限", "指数退避等待的秒数上限。"),
            "jitter" => ("随机退避", "分散同时重试的请求，避免集中再次触发限流。"),
            "endpoint" => (
                "端点风格",
                "通常填 \"Default\"；Azure 填 {\"Azure\":{\"api_version\":\"实际版本\"}}。",
            ),
            "context_window" => (
                "上下文窗口",
                "模型可接受的上下文 token 数，用于占用估算与自动压缩。",
            ),
            "max_output_tokens" => (
                "模型输出上限",
                "每次请求允许的输出 token 上限；不用于提前扣减压缩阈值。",
            ),
            "reasoning" => (
                "支持思考",
                "声明模型是否支持推理；具体档位在模型角色或会话中选择。",
            ),
            "thinking_levels" => (
                "可选思考等级",
                "逐行填写 low/medium/high/xhigh/max；留空沿用默认集合，off 始终可单独选择。",
            ),
            "input" => (
                "输入能力",
                "逐行填写 text、image；只有声明 image 才向模型发送图片。",
            ),
            "remote_compaction" => (
                "原生远程压缩",
                "留空自动判断；只适用于 Responses。上游不支持时可关闭。",
            ),
            "pricing" => (
                "每百万 token 价格",
                "JSON 对象：input/output 必填，cache_read/cache_write 可选；美元，留空隐藏费用。",
            ),
            "approval_mode" => (
                "现有工具审批模式",
                "yolo 自动允许，prompt 询问写入和执行；已有 deny 规则仍生效。",
            ),
            "approval" => (
                "现有工具审批覆盖",
                "工具名到 allow/deny/prompt 的 JSON 对象，名称支持 glob。",
            ),
            "head_bytes" => (
                "输出开头预算",
                "工具输出超限后保留的开头字节数，完整结果仍写入 artifact。",
            ),
            "tail_bytes" => (
                "输出结尾预算",
                "工具输出超限后保留的结尾字节数；Bash 与 grep 共享此策略。",
            ),
            "max_file_bytes" => (
                "单文件容量上限",
                "文件操作允许处理的字节数；grep 分组内则仅限制搜索候选文件。",
            ),
            "default_timeout_secs" => (
                "命令默认时限",
                "Bash 未指定 timeout 时使用的秒数；0 表示不限时。",
            ),
            "max_timeout_secs" => ("命令时限上限", "Bash 单次请求的 timeout 不得超过此秒数。"),
            "auto_background_ms" => (
                "自动转后台",
                "前台命令超过此毫秒数可转后台；0 关闭；无名额时继续前台等待。",
            ),
            "deny_patterns" => (
                "现有危险命令规则",
                "逐行填写拒绝执行的正则；这是命令提示保护，不是操作系统沙箱。",
            ),
            "default_limit" => ("默认读取行数", "未指定行数时 read 返回的行数。"),
            "max_bytes" => ("读取输出预算", "一次 read 向模型返回的正文总字节数上限。"),
            "line_max_bytes" => (
                "单行读取上限",
                "超过此字节数的行只显示超限提示，不授予该行编辑锚点。",
            ),
            "max_edits" => ("单次编辑数", "一次 edit 最多包含的编辑操作数。"),
            "noop_loop_threshold" => (
                "无效编辑循环阈值",
                "同一编辑连续无变化达到此次数后提醒 agent 重新读取。",
            ),
            "hash_echo_min_hits" => (
                "锚点误写识别阈值",
                "正文包含多少个已服务锚点后拒绝 write，避免把行标记写回源文件。",
            ),
            "max_files" => ("搜索展示文件数", "一次 grep 最多展示多少个命中文件。"),
            "max_matches_per_file" => ("每文件命中上限", "单个文件最多返回多少处匹配。"),
            "max_matches_total" => ("总命中上限", "一次 grep 最多返回的匹配数量。"),
            "line_max_chars" => ("搜索行字符预算", "搜索输出每行的字符上限，不影响原文件。"),
            "context_before" => ("前置上下文行", "匹配前附带的行数。"),
            "context_after" => ("后置上下文行", "匹配后附带的行数。"),
            "timeout_ms" => ("搜索时限（毫秒）", "文件遍历或搜索允许的运行时长。"),
            "max_candidate_files" => (
                "候选文件上限",
                "最多收集多少个搜索候选，限制大型目录的资源占用。",
            ),
            "limit" => ("默认结果数量", "未显式指定时返回的结果数。"),
            "max_running" => (
                "后台进程上限",
                "同一 core 所有会话共享的后台进程名额，默认 4 个。",
            ),
            "overflow" => (
                "后台名额用尽",
                "error 立即告诉 agent 超限；wait 在工具调用中等待空位，可取消。",
            ),
            "method_order" => (
                "压缩方法顺序",
                "逐行填写 remote/shake/soft；前一种不适用或失败时尝试下一种。",
            ),
            "keep_recent_tokens" => (
                "摘要后保留历史",
                "摘要压缩保留的最近历史 token 预算，尽量保持完整用户轮次。",
            ),
            "protect_recent_tool_tokens" => {
                ("工具结果保护预算", "本地修剪时不清理最近这部分工具结果。")
            }
            "mid_turn" => (
                "轮次内压缩",
                "在工具调用边界检查阈值，避免长轮次持续撑大上下文。",
            ),
            "auto" => (
                "按阈值自动压缩",
                "关闭后仍可手动压缩，并在上游报告上下文溢出时恢复。",
            ),
            "auto_continue" => ("压缩后继续", "自动压缩完成后继续当前任务。"),
            "soft_trim_chars" => ("工具结果修剪阈值", "超过此字符数的旧工具结果才做首尾修剪。"),
            "head_chars" => ("修剪保留开头", "旧工具结果修剪后保留的开头字符数。"),
            "tail_chars" => ("修剪保留结尾", "旧工具结果修剪后保留的结尾字符数。"),
            "hard_clear_turns" => (
                "旧结果清空年龄",
                "工具结果超过此轮数且不在保护区时可清空正文。",
            ),
            "keep_last_turns" => ("保护最近轮数", "最近这些轮次的工具结果不做清空。"),
            "supersede_reads" => (
                "移除被新读取替代的旧读取",
                "同文件的新读取覆盖旧结果时，压缩掉不再需要的旧正文。",
            ),
            "server_side" => (
                "请求内服务端压缩",
                "Responses 请求发送 context_management；仅对声明支持的模型启用。",
            ),
            "retained_message_tokens" => (
                "远程压缩保留历史",
                "原生压缩之后同时保留的最近消息 token 预算。",
            ),
            "max_steps_per_turn" => ("每轮步骤上限", "一次用户请求最多执行多少次模型请求步骤。"),
            "max_length_continues" => ("长度截断续写次数", "模型因输出长度停止后允许继续的次数。"),
            "override" => (
                "基础系统提示",
                "替换内置身份说明，不移除工具规则、项目指令和环境信息。",
            ),
            "append" => ("附加系统提示", "在基础系统提示后追加长期指令。"),
            "max_tokens" => (
                "指令注入预算",
                "所有指令文件共享的 token 预算；超限保留原文件路径并说明省略。",
            ),
            "import_depth" => ("指令引用深度", "AGENTS.md 中 @文件 引用最多展开的层数。"),
            "watch" => (
                "工作区变更检测",
                "在工具边界对比工作区快照，报告外部修改与只读模式违规。",
            ),
            "max_scan_entries" => (
                "工作区扫描上限",
                "单次工作区快照或文件遍历最多处理的条目数。",
            ),
            "url" => ("服务 URL", "HTTP 服务的完整地址。"),
            "recall" => ("自动召回记忆", "每轮开始按当前问题召回工作区相关知识。"),
            "capture" => (
                "自动记录经验",
                "把会话消息和工具结果发送给 OpenViking 提取记忆。",
            ),
            "skills" => (
                "远程 Skills",
                "从 OpenViking 获取技能目录；本地同名技能优先。",
            ),
            "peer_scope" => (
                "召回范围",
                "actor 限当前工作区及共享内容；all 允许跨工作区召回。",
            ),
            "max_context_tokens" => (
                "记忆注入预算",
                "每轮 OpenViking 召回内容可占用的 token 数。",
            ),
            "dir" => (
                "会话目录",
                "会话和产物存储位置；更改后重启生效，不自动搬迁旧文件。",
            ),
            "archive_after_days" => (
                "冷会话归档天数",
                "超过这些天且不在保留名单的会话可压缩归档，恢复时自动解压。",
            ),
            "retain_newest_global" => ("全局保留最近会话", "最近这些会话不归档也不清理产物。"),
            "retain_newest_per_workspace" => {
                ("每工作区保留最近会话", "每个工作区最近这些会话始终保留。")
            }
            "artifacts_days" => (
                "未引用产物保留天数",
                "只清理过期且所有会话/归档/备份均未引用的 artifact。维护先预览再确认。",
            ),
            "logs_days" => (
                "未引用日志保留天数",
                "只清理过期且未被历史引用的 job 日志；运行中的会话不清理。",
            ),
            "call_timeout_secs" => (
                "MCP 调用时限",
                "一次 MCP 工具调用允许的秒数；server 可独立覆盖。",
            ),
            "max_output_chars" => (
                "MCP 输出预算",
                "MCP 超长结果保留预览，完整文本写入 artifact。",
            ),
            "dirs" => (
                "额外 Skills 目录",
                "逐行填写目录，支持 ~；发现的有效技能默认全部启用。",
            ),
            "disable" => (
                "禁用技能名称",
                "逐行填写不希望使用的技能名；不提供允许名单。",
            ),
            "catalog_char_budget" => ("技能目录字节预算", "超预算时缩短说明，技能名仍然保留可见。"),
            "description_max_chars" => ("单个技能说明长度", "每条技能简介最多显示的字符数。"),
            "command" => (
                "MCP 启动程序",
                "stdio MCP 使用的可执行程序；不通过 shell 解释。",
            ),
            "args" => ("程序参数", "逐行填写启动参数，每行一个完整参数。"),
            "env" => ("环境变量", "JSON 键值对象，仅追加到该服务的进程环境。"),
            "cwd" => ("服务工作目录", "MCP 子进程的工作目录。"),
            "type" => (
                "MCP 传输类型",
                "stdio 为本地程序，http 为 Streamable HTTP 服务。",
            ),
            "allow" => ("MCP 工具允许模式", "逐行填写工具名 glob；空列表不限制。"),
            "deny" => ("MCP 工具拒绝模式", "逐行填写工具名 glob；拒绝规则优先。"),
            "supports_stop" => ("支持停止序列", "上游是否接受 stop/stop_sequences 字段。"),
            "supports_prompt_cache_key" => (
                "接受缓存扩展",
                "Chat/Responses 是否接受 prompt_cache_key；不影响 Anthropic 缓存标记。",
            ),
            "max_output_tokens_field" => (
                "输出上限字段",
                "Chat 上游使用 max_tokens 还是 max_completion_tokens。",
            ),
            "reasoning_field" => ("推理回传字段", "向上游回传思维链时使用的字段名。"),
            "reasoning_field_aliases" => ("推理响应字段", "逐行填写识别上游思维链响应的字段名。"),
            "tool_choice_dialect" => (
                "工具选择方言",
                "按网关支持情况选择标准、required→any 或不发送。",
            ),
            "parallel_tool_calls" => (
                "支持并行工具调用字段",
                "上游是否接受 parallel_tool_calls，不改变本地工具并行安全判断。",
            ),
            "developer_role" => (
                "支持 developer 角色",
                "关闭时将 developer 消息转换为 system。",
            ),
            "response_format_dialect" => (
                "结构化响应方言",
                "上游不支持 response_format 时选择 unsupported。",
            ),
            "echo_reasoning" => (
                "历史思维链回传",
                "auto 按请求需要处理，always 总是回传，never 不回传。",
            ),
            "requires_reasoning_content_for_tool_calls" => (
                "工具历史要求推理字段",
                "为工具调用历史补齐上游要求的 reasoning_content。",
            ),
            "requires_reasoning_content_for_all_assistant_turns" => (
                "全部助手历史要求推理字段",
                "所有助手历史必须携带推理字段；缺失时补显式空内容。",
            ),
            "requires_assistant_content_for_tool_calls" => (
                "工具历史要求助手正文",
                "工具调用消息必须有 content，即便为空字符串。",
            ),
            "toggle_field" => (
                "思考开关字段",
                "Chat 请求中思考对象的字段名；留空不发送该对象。",
            ),
            "enabled_value" => ("思考开启值", "思考对象表示开启的类型值。"),
            "disabled_value" => ("思考关闭值", "思考对象表示关闭的类型值。"),
            "effort_field" => ("思考强度字段", "Chat 请求的推理强度字段名；留空不发送。"),
            "effort_map" => (
                "思考等级改写表",
                "JSON 对象，将通用等级映射为供应商接受的值。",
            ),
            "toggle_extra" => (
                "思考对象扩展",
                "JSON 对象，追加到 thinking 对象内部的厂商字段。",
            ),
            _ => (key, "此项由所属服务定义；修改前请确认该服务接受的值。"),
        },
    }
}
