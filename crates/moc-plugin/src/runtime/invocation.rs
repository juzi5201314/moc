use super::CallHost;
use super::CallOp;
use super::Functions;
use super::Invocation;
use crate::error::PluginError;
use crate::error::Result;
use rquickjs::CaughtError;
use rquickjs::Ctx;
use rquickjs::Function;
use rquickjs::Object;
use rquickjs::Value as JsValue;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;

pub(super) fn invoke<'js>(
    ctx: &Ctx<'js>,
    functions: &Functions,
    host: &Rc<RefCell<Option<CallHost>>>,
    invocation: Invocation,
) -> Result<Value> {
    let Invocation {
        op,
        payload,
        session,
        tools_allowed,
        deadline,
        host_tx,
    } = invocation;
    *host.borrow_mut() = Some(CallHost {
        tx: host_tx,
        session: session.clone(),
        tools_allowed,
        deadline,
        fault: None,
        state: None,
    });
    let result = dispatch(ctx, functions, op, &payload, &session);
    let call = host.borrow_mut().take().expect("调用上下文已登记");
    match call.fault {
        Some(error) => Err(error),
        None => result,
    }
}

pub(super) fn dispatch<'js>(
    ctx: &Ctx<'js>,
    functions: &Functions,
    op: CallOp,
    payload: &Value,
    _session: &str,
) -> Result<Value> {
    let context_value = || -> Result<JsValue<'js>> {
        functions
            .context
            .clone()
            .restore(ctx)
            .and_then(|factory| factory.call(()))
            .map_err(|error| js_error(ctx, error))
    };
    match op {
        CallOp::Activate | CallOp::Deactivate => {
            let export = export_object(ctx, functions)?;
            let name = if op == CallOp::Activate {
                "activate"
            } else {
                "deactivate"
            };
            let Some(function) = optional_function(ctx, &export, name)? else {
                return Ok(Value::Null);
            };
            // 生命周期钩子不带会话：加载发生在会话之外，`moc.session()` 此时不可用。
            let result: JsValue = function.call(()).map_err(|error| js_error(ctx, error))?;
            settle(ctx, result)?;
            Ok(Value::Null)
        }
        CallOp::Tool => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "工具调用缺少 name"))?;
            let function = functions
                .tools
                .get(name)
                .ok_or_else(|| PluginError::new("E_PLUGIN_TOOL", format!("工具 `{name}` 未注册")))?
                .clone()
                .restore(ctx)
                .map_err(|error| js_error(ctx, error))?;
            let arguments = payload.get("args").cloned().unwrap_or(Value::Null);
            let arguments: JsValue = ctx
                .json_parse(arguments.to_string())
                .map_err(|error| js_error(ctx, error))?;
            let result: JsValue = function
                .call((arguments, context_value()?))
                .map_err(|error| js_error(ctx, error))?;
            settle(ctx, result)
        }
        CallOp::Command => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "命令调用缺少 name"))?;
            let function = functions
                .commands
                .get(name)
                .ok_or_else(|| {
                    PluginError::new("E_PLUGIN_COMMAND", format!("命令 `{name}` 未注册"))
                })?
                .clone()
                .restore(ctx)
                .map_err(|error| js_error(ctx, error))?;
            let arguments = payload
                .get("args")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let result: JsValue = function
                .call((arguments, context_value()?))
                .map_err(|error| js_error(ctx, error))?;
            settle(ctx, result)
        }
        CallOp::Hook(kind) => {
            let export = export_object(ctx, functions)?;
            let hooks: JsValue = export.get("hooks").map_err(|error| js_error(ctx, error))?;
            let hooks = hooks
                .into_object()
                .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "模块没有导出 hooks"))?;
            let name = kind.js_name();
            let function = optional_function(ctx, &hooks, name)?.ok_or_else(|| {
                PluginError::new("E_PLUGIN_STATE", format!("钩子 `{name}` 不存在"))
            })?;
            let event: JsValue = ctx
                .json_parse(payload.to_string())
                .map_err(|error| js_error(ctx, error))?;
            let result: JsValue = function
                .call((event, context_value()?))
                .map_err(|error| js_error(ctx, error))?;
            settle(ctx, result)
        }
        CallOp::Prompt => {
            let export = export_object(ctx, functions)?;
            let function = optional_function(ctx, &export, "prompt")?
                .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "模块没有导出 prompt"))?;
            let result: JsValue = function
                .call::<_, JsValue>((context_value()?,))
                .map_err(|error| js_error(ctx, error))?;
            settle(ctx, result)
        }
    }
}

pub(super) fn export_object<'js>(ctx: &Ctx<'js>, functions: &Functions) -> Result<Object<'js>> {
    functions
        .export
        .clone()
        .restore(ctx)
        .map_err(|error| js_error(ctx, error))
}

pub(super) fn optional_function<'js>(
    ctx: &Ctx<'js>,
    object: &Object<'js>,
    key: &str,
) -> Result<Option<Function<'js>>> {
    let value: JsValue = object.get(key).map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .into_function()
        .map(Some)
        .ok_or_else(|| PluginError::new("E_PLUGIN_DESCRIPTOR", format!("`{key}` 必须是函数")))
}

/// 把插件函数的返回值转成 JSON；返回 Promise 时先驱动 QuickJS 的任务队列直到落定。
///
/// 插件里没有定时器和事件循环，`finish` 在队列耗尽仍 Pending 时返回 `WouldBlock`：
/// 这种 Promise 永远等不到结果，按状态错误报出去，而不是静默变成 `{}`。
pub(super) fn settle<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Result<Value> {
    if !value.is_promise() {
        return js_to_json(ctx, value);
    }
    let promise = value
        .into_promise()
        .expect("is_promise 判定之后一定是 Promise");
    match promise.finish::<JsValue>() {
        Ok(resolved) => js_to_json(ctx, resolved),
        Err(rquickjs::Error::WouldBlock) => Err(PluginError::new(
            "E_PLUGIN_STATE",
            "函数返回的 Promise 没有落定：插件里没有定时器或事件循环，Promise 必须能自行完成",
        )),
        Err(error) => Err(js_error(ctx, error)),
    }
}

pub(super) fn js_to_json<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Result<Value> {
    if value.is_undefined() || value.is_null() {
        return Ok(Value::Null);
    }
    match ctx.json_stringify(value) {
        Ok(Some(text)) => {
            let text = text.to_string().map_err(|error| js_error(ctx, error))?;
            serde_json::from_str(&text)
                .map_err(|error| PluginError::new("E_PLUGIN_VALUE", error.to_string()))
        }
        // `JSON.stringify` 对 undefined/函数/符号返回 undefined。
        Ok(None) => Ok(Value::Null),
        Err(error) => Err(js_error(ctx, error)),
    }
}

pub(super) fn throw(ctx: &Ctx<'_>, error: &PluginError) -> rquickjs::Error {
    match rquickjs::Exception::from_message(ctx.clone(), &error.message) {
        Ok(exception) => {
            let _ = exception.as_object().set("code", error.code);
            exception.throw()
        }
        Err(error) => error,
    }
}

pub(super) fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> PluginError {
    caught(ctx, CaughtError::from_error(ctx, error))
}

pub(super) fn caught<'js>(ctx: &Ctx<'js>, error: CaughtError<'js>) -> PluginError {
    match error {
        CaughtError::Exception(exception) => {
            let message = exception
                .message()
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| "插件脚本抛出异常".to_owned());
            let code = exception
                .as_object()
                .get::<_, Option<String>>("code")
                .ok()
                .flatten()
                .map(|code| PluginError::known_code(&code))
                .unwrap_or_else(|| default_code(&message));
            // 栈只给用户看：模型可见的工具结果保持单行，排查时 /plugins 与通知里有位置。
            PluginError::new(code, message)
                .with_detail(exception.stack().map(clip_stack).unwrap_or_default())
        }
        // `throw "文本"` / `throw {code, message}` / `Promise.reject(值)`：把值本身变成
        // 可读消息，并且认识约定的错误对象——否则会退化成带引号的 JSON 文字。
        CaughtError::Value(value) => match thrown_object(&value) {
            Some(error) => error,
            None => {
                let message = if value.is_string() {
                    value.get::<String>().unwrap_or_default()
                } else {
                    ctx.json_stringify(value)
                        .ok()
                        .flatten()
                        .and_then(|text| text.to_string().ok())
                        .unwrap_or_else(|| "插件抛出了无法序列化的值".to_owned())
                };
                PluginError::new("E_PLUGIN", message)
            }
        },
        CaughtError::Error(error) => map_error(error),
    }
}

/// `throw {code, message}` 的约定形状；缺 `code` 的对象仍按普通值处理。
pub(super) fn thrown_object(value: &rquickjs::Value<'_>) -> Option<PluginError> {
    if !value.is_object() {
        return None;
    }
    let object = value.clone().into_object()?;
    let code = object.get::<_, Option<String>>("code").ok().flatten()?;
    let message = object
        .get::<_, Option<String>>("message")
        .ok()
        .flatten()
        .unwrap_or_else(|| "插件抛出了错误对象".to_owned());
    Some(PluginError::new(PluginError::known_code(&code), message))
}

/// 栈可能很长（深递归），截断到 2 KiB：诊断要能定位，不是把日志搬进界面。
pub(super) fn clip_stack(stack: String) -> String {
    const LIMIT: usize = 2048;
    match stack.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}…", &stack[..end]),
        None => stack,
    }
}

pub(super) fn default_code(message: &str) -> &'static str {
    let lowered = message.to_lowercase();
    if lowered.contains("out of memory") {
        "E_PLUGIN_MEMORY"
    } else if lowered.contains("interrupt") {
        "E_PLUGIN_INTERRUPTED"
    } else {
        "E_PLUGIN"
    }
}

pub(super) fn map_error(error: rquickjs::Error) -> PluginError {
    PluginError::new(default_code(&error.to_string()), error.to_string())
}
