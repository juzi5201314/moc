use super::Functions;
use super::MAX_PRESET_PROMPT_CHARS;
use super::invocation::js_error;
use super::invocation::js_to_json;
use crate::descriptor::AgentPresetDecl;
use crate::descriptor::CommandDecl;
use crate::descriptor::ConfigDecl;
use crate::descriptor::ConfigKind;
use crate::descriptor::Descriptor;
use crate::descriptor::HookKind;
use crate::descriptor::ModeMask;
use crate::descriptor::ToolDecl;
use crate::error::PluginError;
use crate::error::Result;
use crate::manifest::valid_command_name;
use crate::manifest::valid_tool_name;
use rquickjs::Ctx;
use rquickjs::Function;
use rquickjs::Object;
use rquickjs::Persistent;
use rquickjs::Value as JsValue;
use moc_i18n::t;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn parse_descriptor<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
) -> Result<(Descriptor, Functions)> {
    const ALLOWED: [&str; 9] = [
        "name",
        "version",
        "description",
        "tools",
        "commands",
        "hooks",
        "prompt",
        "activate",
        "deactivate",
    ];
    for key in object.keys::<String>() {
        let key = key.map_err(|error| js_error(ctx, error))?;
        if !ALLOWED.contains(&key.as_str()) {
            return Err(PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                t!("plugin.err.export_unknown_field", key = key.as_str()).into_owned(),
            ));
        }
    }
    let mut descriptor = Descriptor {
        name: optional_string(ctx, object, "name")?,
        version: optional_string(ctx, object, "version")?,
        description: optional_string(ctx, object, "description")?,
        ..Descriptor::default()
    };

    let mut tool_functions = BTreeMap::new();
    if let Some(tools) = optional_object(ctx, object, "tools")? {
        for key in tools.keys::<String>() {
            let name = key.map_err(|error| js_error(ctx, error))?;
            if !valid_tool_name(&name) {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.tool_name_invalid", name = name.as_str()).into_owned(),
                ));
            }
            let value: JsValue = tools
                .get(name.as_str())
                .map_err(|error| js_error(ctx, error))?;
            let declaration = value.into_object().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.tool_decl_object", name = name.as_str()).into_owned(),
                )
            })?;
            for key in declaration.keys::<String>() {
                let key = key.map_err(|error| js_error(ctx, error))?;
                if !["description", "parameters", "modes", "parallel", "run"]
                    .contains(&key.as_str())
                {
                    return Err(PluginError::new(
                        "E_PLUGIN_DESCRIPTOR",
                        t!(
                            "plugin.err.tool_unknown_field",
                            name = name.as_str(),
                            key = key.as_str()
                        )
                        .into_owned(),
                    ));
                }
            }
            let description = required_text(ctx, &declaration, "description", &name)?;
            let parameters_value: JsValue = declaration
                .get("parameters")
                .map_err(|error| js_error(ctx, error))?;
            let parameters = js_to_json(ctx, parameters_value)?;
            if !parameters.is_object() {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.tool_params_schema", name = name.as_str()).into_owned(),
                ));
            }
            let modes = optional_modes(ctx, &declaration, &name)?;
            let parallel = optional_bool(ctx, &declaration, "parallel")?.unwrap_or(false);
            let run: JsValue = declaration
                .get("run")
                .map_err(|error| js_error(ctx, error))?;
            let run = run.into_function().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.tool_missing_run", name = name.as_str()).into_owned(),
                )
            })?;
            tool_functions.insert(name.clone(), Persistent::save(ctx, run));
            descriptor.tools.push(ToolDecl {
                name,
                description,
                parameters,
                modes,
                parallel,
            });
        }
    }

    let mut command_functions = BTreeMap::new();
    if let Some(commands) = optional_object(ctx, object, "commands")? {
        for key in commands.keys::<String>() {
            let name = key.map_err(|error| js_error(ctx, error))?;
            if !valid_command_name(&name) {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.cmd_name_invalid", name = name.as_str()).into_owned(),
                ));
            }
            let value: JsValue = commands
                .get(name.as_str())
                .map_err(|error| js_error(ctx, error))?;
            let declaration = value.into_object().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.cmd_decl_object", name = name.as_str()).into_owned(),
                )
            })?;
            let description = required_text(ctx, &declaration, "description", &name)?;
            let usage = optional_string(ctx, &declaration, "usage")?.unwrap_or_default();
            let run: JsValue = declaration
                .get("run")
                .map_err(|error| js_error(ctx, error))?;
            let run = run.into_function().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.cmd_missing_run", name = name.as_str()).into_owned(),
                )
            })?;
            command_functions.insert(name.clone(), Persistent::save(ctx, run));
            descriptor.commands.push(CommandDecl {
                name,
                description,
                usage,
            });
        }
    }

    let hooks_value: JsValue = object.get("hooks").map_err(|error| js_error(ctx, error))?;
    if let Some(hooks) = hooks_value.into_object() {
        for key in hooks.keys::<String>() {
            let name = key.map_err(|error| js_error(ctx, error))?;
            let Some(kind) = HookKind::from_js_name(&name) else {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!(
                        "plugin.err.hook_unknown",
                        name = name.as_str(),
                        supported = HookKind::supported()
                    )
                    .into_owned(),
                ));
            };
            let value: JsValue = hooks
                .get(name.as_str())
                .map_err(|error| js_error(ctx, error))?;
            if value.is_undefined() {
                continue;
            }
            if !value.is_function() {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    t!("plugin.err.hook_not_fn", name = name.as_str()).into_owned(),
                ));
            }
            descriptor.hooks.push(kind);
        }
        descriptor.hooks.sort_unstable();
        descriptor.hooks.dedup();
    }

    let prompt_value: JsValue = object.get("prompt").map_err(|error| js_error(ctx, error))?;
    if !prompt_value.is_undefined() {
        if !prompt_value.is_function() {
            return Err(PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                t!("plugin.err.prompt_not_fn").into_owned(),
            ));
        }
        descriptor.has_prompt = true;
    }

    Ok((
        descriptor,
        Functions {
            export: Persistent::save(ctx, object.clone()),
            context: Persistent::save(
                ctx,
                ctx.globals()
                    .get::<_, Object>("__moc")
                    .and_then(|host| host.get::<_, Function>("context"))
                    .map_err(|error| js_error(ctx, error))?,
            ),
            tools: tool_functions,
            commands: command_functions,
        },
    ))
}

pub(super) fn required_text<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
    owner: &str,
) -> Result<String> {
    let text = optional_string(ctx, object, key)?;
    match text.filter(|text| !text.trim().is_empty()) {
        Some(text) => Ok(text),
        None => Err(PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            t!("plugin.err.field_nonempty", owner = owner, key = key).into_owned(),
        )),
    }
}

pub(super) fn optional_string<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
) -> Result<Option<String>> {
    let value: JsValue = object.get(key).map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let Some(value) = value.into_string() else {
        return Err(PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            t!("plugin.err.field_string", key = key).into_owned(),
        ));
    };
    value
        .to_string()
        .map(Some)
        .map_err(|error| js_error(ctx, error))
}

/// 可选对象字段：缺失/`null` 返回 `None`，存在但不是对象直接报错。
pub(super) fn optional_object<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
) -> Result<Option<Object<'js>>> {
    let value: JsValue = object.get(key).map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    match value.into_object() {
        Some(object) => Ok(Some(object)),
        None => Err(PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            t!("plugin.err.field_object", key = key).into_owned(),
        )),
    }
}

pub(super) fn optional_bool<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
) -> Result<Option<bool>> {
    let value: JsValue = object.get(key).map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value.as_bool().map(Some).ok_or_else(|| {
        PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            t!("plugin.err.field_bool", key = key).into_owned(),
        )
    })
}

/// 可选 `modes`：缺失时三种模式全开；存在时必须是 `default`/`ask`/`plan`
/// 的非空字符串数组。没有引擎维度：原生和 execute 调用共用同一模式许可。
pub(super) fn optional_modes<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    owner: &str,
) -> Result<ModeMask> {
    let value: JsValue = object.get("modes").map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() {
        return Ok(ModeMask::ALL);
    }
    let items = value.into_array().ok_or_else(|| {
        PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            t!("plugin.err.modes_array", owner = owner).into_owned(),
        )
    })?;
    let mut modes = ModeMask::empty();
    for item in items.iter::<JsValue>() {
        let item = item.map_err(|error| js_error(ctx, error))?;
        let name = item.into_string().ok_or_else(|| {
            PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                t!("plugin.err.modes_array", owner = owner).into_owned(),
            )
        })?;
        let name = name.to_string().map_err(|error| js_error(ctx, error))?;
        modes = modes.union(ModeMask::parse(&name).ok_or_else(|| {
            PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                t!(
                    "plugin.err.modes_unknown",
                    owner = owner,
                    name = name.as_str()
                )
                .into_owned(),
            )
        })?);
    }
    if modes.is_empty() {
        return Err(PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            t!("plugin.err.modes_empty", owner = owner).into_owned(),
        ));
    }
    Ok(modes)
}

pub(super) fn config_declaration(key: String, definition: &str) -> Result<ConfigDecl> {
    let invalid =
        |message: std::borrow::Cow<'static, str>| PluginError::new("E_PLUGIN_CONFIG", message);
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(invalid(t!("plugin.err.config_key_invalid")));
    }
    let value: Value =
        serde_json::from_str(definition).map_err(|error| invalid(error.to_string().into()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid(t!("plugin.err.config_decl_object")))?;
    if object
        .keys()
        .any(|key| !["type", "default", "label", "description", "choices"].contains(&key.as_str()))
    {
        return Err(invalid(t!("plugin.err.config_unknown_field")));
    }
    let kind = match object.get("type").and_then(Value::as_str) {
        Some("bool") => ConfigKind::Bool,
        Some("integer") => ConfigKind::Integer,
        Some("number") => ConfigKind::Number,
        Some("text") => ConfigKind::Text,
        Some("secret") => ConfigKind::Secret,
        Some("multiline") => ConfigKind::Multiline,
        Some("strings") => ConfigKind::Strings,
        Some("object") => ConfigKind::Object,
        Some("choice") => ConfigKind::Choice,
        _ => return Err(invalid(t!("plugin.err.config_type_unknown"))),
    };
    let mut choices = Vec::new();
    if let Some(value) = object.get("choices") {
        let values = value
            .as_array()
            .ok_or_else(|| invalid(t!("plugin.err.choices_array")))?;
        for item in values {
            let choice = item
                .as_str()
                .ok_or_else(|| invalid(t!("plugin.err.choices_array")))?
                .to_owned();
            if choices.contains(&choice) {
                return Err(invalid(t!("plugin.err.choices_dup")));
            }
            choices.push(choice);
        }
    }
    if (kind == ConfigKind::Choice) != !choices.is_empty() {
        return Err(invalid(t!("plugin.err.choices_required")));
    }
    let text = |name: &str, fallback: &str| -> Result<String> {
        match object.get(name) {
            None => Ok(fallback.to_owned()),
            Some(Value::String(value)) => Ok(value.clone()),
            _ => Err(invalid(t!("plugin.err.label_desc_string"))),
        }
    };
    let declaration = ConfigDecl {
        label: text("label", &key)?,
        description: text("description", "")?,
        key,
        kind,
        default: object.get("default").cloned(),
        choices,
    };
    if let Some(value) = &declaration.default {
        declaration
            .validate(value)
            .map_err(|error| invalid(error.into()))?;
    }
    Ok(declaration)
}

/// 解析 `agents.registerPreset` 的声明对象。
pub(super) fn agent_preset_declaration(definition: &str) -> Result<AgentPresetDecl> {
    let invalid =
        |message: std::borrow::Cow<'static, str>| PluginError::new("E_PLUGIN_AGENT", message);
    let value: Value =
        serde_json::from_str(definition).map_err(|error| invalid(error.to_string().into()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid(t!("plugin.err.preset_object")))?;
    if object.keys().any(|key| {
        !["name", "description", "model", "systemPrompt", "tools"].contains(&key.as_str())
    }) {
        return Err(invalid(t!("plugin.err.preset_unknown_field")));
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // 与 moc-core 的 preset 名同形：小写字母开头的小写字母/数字/连字符。
    if !(1..=64).contains(&name.len())
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err(invalid(t!("plugin.err.preset_name")));
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| invalid(t!("plugin.err.preset_desc")))?;
    let model = match object.get("model") {
        None => "inherit",
        Some(value) => value
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| invalid(t!("plugin.err.preset_model")))?,
    };
    let system_prompt = object
        .get("systemPrompt")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(t!("plugin.err.preset_prompt")))?;
    if system_prompt.trim().is_empty() {
        return Err(invalid(t!("plugin.err.preset_prompt_empty")));
    }
    if system_prompt.chars().count() > MAX_PRESET_PROMPT_CHARS {
        return Err(invalid(t!("plugin.err.preset_prompt_long")));
    }
    let mut allow = None;
    let mut deny = None;
    if let Some(tools) = object.get("tools") {
        let tools = tools
            .as_object()
            .ok_or_else(|| invalid(t!("plugin.err.preset_tools_object")))?;
        if tools
            .keys()
            .any(|key| !["allow", "deny"].contains(&key.as_str()))
        {
            return Err(invalid(t!("plugin.err.preset_tools_fields")));
        }
        let names = |key: &str| -> Result<Option<Vec<String>>> {
            match tools.get(key) {
                None => Ok(None),
                Some(items) => {
                    let items = items
                        .as_array()
                        .ok_or_else(|| invalid(t!("plugin.err.preset_tools_array")))?;
                    items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| invalid(t!("plugin.err.preset_tools_array")))
                        })
                        .collect::<Result<Vec<_>>>()
                        .map(Some)
                }
            }
        };
        allow = names("allow")?;
        deny = names("deny")?;
        if allow.is_some() && deny.is_some() {
            return Err(invalid(t!("plugin.err.preset_tools_exclusive")));
        }
    }
    Ok(AgentPresetDecl {
        name: name.to_owned(),
        description: description.to_owned(),
        model: model.to_owned(),
        system_prompt: system_prompt.to_owned(),
        allow,
        deny,
    })
}
