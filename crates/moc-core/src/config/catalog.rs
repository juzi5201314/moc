//! /config 的字段目录：字段说明与类型来自此处，值仍由配置 schema 校验。
//! 文案全部外置 locales/core.toml：label/desc 按 `core.config.fields.<path_下划线>`、
//! `core.config.keys.<key>`、选项按 `core.config.opt.*` 查键，缺键回退字段名。

use std::borrow::Cow;

use moc_llm::Protocol;
use moc_protocol::{ChoiceOption, ConfigField, FieldKind, SessionPermission};
use rust_i18n::t;
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
    let tab: Cow<str> = if path.first().is_some_and(|part| part == "providers") {
        if path.get(2).is_some_and(|part| part == "models") {
            t!("core.config.tabs.models")
        } else {
            t!("core.config.tabs.providers")
        }
    } else if path.first().is_some_and(|part| part == "models") {
        t!("core.config.tabs.models")
    } else {
        match path.first().map_or("", String::as_str) {
            "tools" | "web_fetch" => t!("core.config.tabs.tools"),
            "sandbox" => t!("core.config.tabs.sandbox"),
            "compaction" | "limits" => t!("core.config.tabs.context"),
            "context_files" | "system_prompt" => t!("core.config.tabs.instructions"),
            "defaults" | "agents" => t!("core.config.tabs.models"),
            "skills" => t!("core.config.tabs.skills"),
            "mcp" => t!("core.config.tabs.mcp"),
            "plugins" => t!("core.config.tabs.plugins"),
            _ => t!("core.config.tabs.session"),
        }
    };
    let options: &[(&str, &str)] = match key {
        "language" => &[
            ("auto", "core.config.opt.language.auto"),
            ("zh", "core.config.opt.language.zh"),
            ("en", "core.config.opt.language.en"),
        ],
        "api" => &[
            ("openai_chat", "core.config.opt.api.openai_chat"),
            ("openai_responses", "core.config.opt.api.openai_responses"),
            (
                "anthropic_messages",
                "core.config.opt.api.anthropic_messages",
            ),
        ],
        "cache" => &[
            ("auto", "core.config.opt.cache.auto"),
            ("short", "core.config.opt.cache.short"),
            ("long", "core.config.opt.cache.long"),
            ("off", "core.config.opt.cache.off"),
        ],
        "overflow" => &[
            ("error", "core.config.opt.overflow.error"),
            ("wait", "core.config.opt.overflow.wait"),
        ],
        "max_output_tokens_field" => &[
            ("max_tokens", "core.config.opt.motf.max_tokens"),
            (
                "max_completion_tokens",
                "core.config.opt.motf.max_completion_tokens",
            ),
        ],
        "tool_choice_dialect" if api.is_some_and(|api| api != Protocol::OpenAiChat) => &[
            ("standard", "core.config.opt.tool_choice.standard"),
            ("unsupported", "core.config.opt.tool_choice.unsupported"),
        ],
        "tool_choice_dialect" => &[
            ("standard", "core.config.opt.tool_choice.standard"),
            (
                "any_for_required",
                "core.config.opt.tool_choice.any_for_required",
            ),
            ("unsupported", "core.config.opt.tool_choice.unsupported"),
        ],
        "response_format_dialect" => &[
            ("standard", "core.config.opt.tool_choice.standard"),
            ("unsupported", "core.config.opt.tool_choice.unsupported"),
        ],
        "echo_reasoning" => &[
            ("protocol", "core.config.opt.echo_reasoning.protocol"),
            ("always", "core.config.opt.echo_reasoning.always"),
            ("never", "core.config.opt.echo_reasoning.never"),
        ],
        "mode" if path.iter().any(|part| part == "threshold") => &[
            ("percent", "core.config.opt.threshold_mode.percent"),
            ("tokens", "core.config.opt.threshold_mode.tokens"),
        ],
        "method" if path.first().is_some_and(|part| part == "compaction") => &[
            ("moc", "core.config.opt.compaction_method.moc"),
            ("grok_build", "core.config.opt.compaction_method.grok_build"),
            ("eocc", "core.config.opt.compaction_method.eocc"),
        ],
        "default_mode" if path.first().is_some_and(|part| part == "sandbox") => {
            let support = crate::sandbox::detect();
            if support.supports(SessionPermission::Workspace)
                && support.supports(SessionPermission::WorkspaceOnly)
            {
                &[
                    ("full", "core.config.opt.sandbox_mode.full"),
                    ("workspace", "core.config.opt.sandbox_mode.workspace"),
                    (
                        "workspace_only",
                        "core.config.opt.sandbox_mode.workspace_only",
                    ),
                ]
            } else if support.supports(SessionPermission::Workspace) {
                &[
                    ("full", "core.config.opt.sandbox_mode.full"),
                    ("workspace", "core.config.opt.sandbox_mode.workspace"),
                ]
            } else {
                &[("full", "core.config.opt.sandbox_mode.full_unsupported")]
            }
        }
        "on_denied" if path.first().is_some_and(|part| part == "sandbox") => &[
            ("ask", "core.config.opt.on_denied.ask"),
            ("deny", "core.config.opt.on_denied.deny"),
        ],
        _ => &[],
    };
    let kind = if !options.is_empty() {
        FieldKind::Choice(
            options
                .iter()
                .map(|(value, label)| ChoiceOption::new(value, &t!(*label)))
                .collect(),
        )
    } else if key == "input" && path.iter().any(|part| part == "models") {
        FieldKind::Choice(vec![
            ChoiceOption::new("text", &t!("core.config.opt.model_input.text")),
            ChoiceOption::new("text, image", &t!("core.config.opt.model_input.text_image")),
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
            t!(
                "core.config.desc_fmt",
                name = path[start..].join("."),
                desc = description.as_ref()
            )
            .into_owned()
        } else {
            description.into_owned()
        },
        kind,
        advanced,
    });
}

fn describe<'a>(path: &'a str, key: &'a str) -> (Cow<'static, str>, Cow<'static, str>) {
    // 精确路径文案 > mcp.servers 通配 > 通用字段名 > 字段名原样 + 默认描述。
    let stem = path.replace('.', "_");
    if let Some(pair) = describe_at(&format!("core.config.fields.{stem}")) {
        return pair;
    }
    if path.starts_with("mcp.servers.") {
        let specialized = match key {
            "timeout_secs" => "mcp_server_timeout_secs",
            "max_concurrent_calls" => "mcp_server_max_concurrent_calls",
            _ => "",
        };
        if !specialized.is_empty()
            && let Some(pair) = describe_at(&format!("core.config.overrides.{specialized}"))
        {
            return pair;
        }
    }
    if let Some(pair) = describe_at(&format!("core.config.keys.{key}")) {
        return pair;
    }
    (Cow::Owned(key.to_owned()), t!("core.config.fallback_desc"))
}

fn describe_at(stem: &str) -> Option<(Cow<'static, str>, Cow<'static, str>)> {
    let label = crate::i18n::try_t(&format!("{stem}.label"))?;
    let desc = crate::i18n::try_t(&format!("{stem}.desc"))
        .unwrap_or_else(|| t!("core.config.fallback_desc"));
    Some((label, desc))
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
