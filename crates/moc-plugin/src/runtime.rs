//! 插件运行时：每个插件一个专用线程，线程内建 QuickJS `Runtime`/`Context`。
//!
//! 线程内的一切都是同步的：JS 调 `moc.tools.call()` 时通过通道把请求发给宿主、
//! 阻塞等回包；宿主在 `Plugin::invoke` 的 `select!` 里服务这些请求。因此不需要
//! PTC 那套 step 状态机。
//!
//! 中断：宿主设置 `interrupt` 标志，QuickJS 的 interrupt handler 在 JS 执行期间轮询，
//! 用来实现超时与取消。被中断的线程不再复用——下一次调用会重建 Runtime。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rquickjs::{
    CaughtError, Context, Ctx, Function, Module, Object, Persistent, Runtime, Value as JsValue,
};
use serde_json::Value;

use crate::descriptor::{CommandDecl, Descriptor, HookKind, ToolDecl, ToolTier};
use crate::error::{PluginError, Result};
use crate::kv::Kv;
use crate::loader::{PluginLoader, PluginResolver};
use crate::manifest::{valid_command_name, valid_tool_name};

/// 宿主 API 版本；插件可用 `version` 判断兼容性。
pub const API_VERSION: &str = "1";
/// JS 栈上限，与 PTC worker 一致。
const STACK_BYTES: usize = 256 * 1024;

/// 运行时限额。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory_limit_bytes: usize,
    pub call_timeout: Duration,
    pub load_timeout: Duration,
}

/// 一次插件调用；`payload` 的形状由 [`CallOp`] 决定。
pub(crate) struct CallRequest {
    pub op: CallOp,
    pub payload: Value,
    /// 会话快照（`{sessionId,cwd,mode,permission,model,isSubagent}` 的 JSON 文本）。
    pub context: String,
    pub tools_allowed: bool,
    pub deadline: Instant,
    pub host_tx: Option<tokio::sync::mpsc::UnboundedSender<HostRequest>>,
    pub reply: tokio::sync::oneshot::Sender<Result<Value>>,
}

/// 插件调用的宿主能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallOp {
    Activate,
    Deactivate,
    Tool,
    Command,
    BeforeToolCall,
    AfterToolCall,
    Prompt,
}

/// JS 在 `moc.tools.call()` 里发起的一次宿主调用。
pub(crate) struct HostRequest {
    pub name: String,
    pub args: Value,
    pub reply: std::sync::mpsc::Sender<Result<Value>>,
}

enum Command {
    Call(CallRequest),
    Shutdown,
}

/// 插件线程的句柄。丢弃即请求退出；线程自行结束，不阻塞调用方。
pub(crate) struct Worker {
    name: String,
    commands: std::sync::mpsc::Sender<Command>,
    interrupt: Arc<AtomicBool>,
}

impl Worker {
    pub fn call(&self, request: CallRequest) -> Result<()> {
        self.commands.send(Command::Call(request)).map_err(|_| {
            PluginError::new(
                "E_PLUGIN_CRASH",
                format!("插件 `{}` 的线程已退出", self.name),
            )
        })
    }

    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// 启动插件线程并等待模块求值完成；返回值里带解析好的描述符。
pub(crate) fn spawn(
    name: String,
    dir: PathBuf,
    entry: PathBuf,
    kv: Arc<Kv>,
    limits: Limits,
) -> (Worker, tokio::sync::oneshot::Receiver<Result<Descriptor>>) {
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let interrupt = Arc::new(AtomicBool::new(false));
    let worker = Worker {
        name: name.clone(),
        commands: command_tx,
        interrupt: Arc::clone(&interrupt),
    };
    let thread_name = format!("moc-plugin-{name}");
    let spawned = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || run_thread(dir, entry, kv, limits, interrupt, command_rx, ready_tx));
    if let Err(error) = spawned {
        // 线程都起不来时把错误通过同一个通道交回调用方。
        let worker = Worker {
            name,
            commands: std::sync::mpsc::channel().0,
            interrupt: Arc::new(AtomicBool::new(false)),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(Err(PluginError::new(
            "E_PLUGIN_THREAD",
            format!("无法启动插件线程：{error}"),
        )));
        return (worker, rx);
    }
    (worker, ready_rx)
}

struct CallHost {
    tx: Option<tokio::sync::mpsc::UnboundedSender<HostRequest>>,
    session: String,
    tools_allowed: bool,
    deadline: Instant,
}

struct Functions {
    export: Persistent<Object<'static>>,
    tools: BTreeMap<String, Persistent<Function<'static>>>,
    commands: BTreeMap<String, Persistent<Function<'static>>>,
}

fn run_thread(
    dir: PathBuf,
    entry: PathBuf,
    kv: Arc<Kv>,
    limits: Limits,
    interrupt: Arc<AtomicBool>,
    commands: std::sync::mpsc::Receiver<Command>,
    ready: tokio::sync::oneshot::Sender<Result<Descriptor>>,
) {
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(PluginError::new("E_PLUGIN_RUNTIME", error.to_string())));
            return;
        }
    };
    runtime.set_memory_limit(limits.memory_limit_bytes);
    runtime.set_max_stack_size(STACK_BYTES);
    let flag = Arc::clone(&interrupt);
    runtime.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::SeqCst))));
    runtime.set_loader(
        PluginResolver::new(dir.clone()),
        PluginLoader::new(dir.clone()),
    );
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => {
            let _ = ready.send(Err(PluginError::new("E_PLUGIN_RUNTIME", error.to_string())));
            return;
        }
    };
    let host: Rc<RefCell<Option<CallHost>>> = Rc::new(RefCell::new(None));
    let initialized = context.with(|ctx| init(&ctx, &entry, &host, &kv));
    let (descriptor, functions) = match initialized {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let deadline = Instant::now() + limits.load_timeout;
    if let Err(error) = context.with(|ctx| {
        invoke(
            &ctx,
            &functions,
            &host,
            Invocation {
                op: CallOp::Activate,
                payload: Value::Null,
                session: "null".into(),
                tools_allowed: false,
                deadline,
                host_tx: None,
            },
        )
    }) {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(descriptor));

    for command in commands {
        match command {
            Command::Shutdown => {
                let deadline = Instant::now() + Duration::from_secs(1);
                let _ = context.with(|ctx| {
                    invoke(
                        &ctx,
                        &functions,
                        &host,
                        Invocation {
                            op: CallOp::Deactivate,
                            payload: Value::Null,
                            session: "null".into(),
                            tools_allowed: false,
                            deadline,
                            host_tx: None,
                        },
                    )
                });
                break;
            }
            Command::Call(request) => {
                let CallRequest {
                    op,
                    payload,
                    context: session,
                    tools_allowed,
                    deadline,
                    host_tx,
                    reply,
                } = request;
                let result = context.with(|ctx| {
                    invoke(
                        &ctx,
                        &functions,
                        &host,
                        Invocation {
                            op,
                            payload,
                            session,
                            tools_allowed,
                            deadline,
                            host_tx,
                        },
                    )
                });
                let _ = reply.send(result);
            }
        }
    }
}

/// 建全局桥、求值模块、读描述符；失败即插件加载失败。
fn init<'js>(
    ctx: &Ctx<'js>,
    entry: &Path,
    host: &Rc<RefCell<Option<CallHost>>>,
    kv: &Arc<Kv>,
) -> Result<(Descriptor, Functions)> {
    install_globals(ctx, host, kv)?;
    let name = entry.to_string_lossy().into_owned();
    let source = std::fs::read(entry).map_err(|error| {
        PluginError::new(
            "E_PLUGIN_LOAD",
            format!("读取 {}: {error}", entry.display()),
        )
    })?;
    let module =
        Module::declare(ctx.clone(), name, source).map_err(|error| js_error(ctx, error))?;
    let (module, promise) = module.eval().map_err(|error| js_error(ctx, error))?;
    promise
        .finish::<()>()
        .map_err(|error| js_error(ctx, error))?;
    // `moc` 模块已经把宿主对象捕获进闭包，全局桥不必再暴露给插件代码。
    let _ = ctx.eval::<(), _>("delete globalThis.__moc");
    let namespace = module.namespace().map_err(|error| js_error(ctx, error))?;
    let default: JsValue = namespace
        .get("default")
        .map_err(|error| js_error(ctx, error))?;
    let object = default.into_object().ok_or_else(|| {
        PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            "`export default` 必须是对象；导出形状见 docs/plugins.md",
        )
    })?;
    let (descriptor, functions) = parse_descriptor(ctx, &object)?;
    Ok((descriptor, functions))
}

fn parse_descriptor<'js>(ctx: &Ctx<'js>, object: &Object<'js>) -> Result<(Descriptor, Functions)> {
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
                format!("`export default` 里有未知字段 `{key}`"),
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
                    format!(
                        "工具名 `{name}` 不合法（1-64 位字母/数字/下划线/连字符，且不能用 mcp__ 前缀）"
                    ),
                ));
            }
            let value: JsValue = tools
                .get(name.as_str())
                .map_err(|error| js_error(ctx, error))?;
            let declaration = value.into_object().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    format!("工具 `{name}` 的声明必须是对象"),
                )
            })?;
            let description = required_text(ctx, &declaration, "description", &name)?;
            let parameters_value: JsValue = declaration
                .get("parameters")
                .map_err(|error| js_error(ctx, error))?;
            let parameters = js_to_json(ctx, parameters_value)?;
            if !parameters.is_object() {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    format!("工具 `{name}` 的 parameters 必须是 JSON Schema 对象"),
                ));
            }
            let tier = match optional_string(ctx, &declaration, "tier")?.as_deref() {
                None | Some("read") => ToolTier::Read,
                Some("write") => ToolTier::Write,
                Some("exec") => ToolTier::Exec,
                Some(other) => {
                    return Err(PluginError::new(
                        "E_PLUGIN_DESCRIPTOR",
                        format!("工具 `{name}` 的 tier `{other}` 无效（read/write/exec）"),
                    ));
                }
            };
            let parallel = optional_bool(ctx, &declaration, "parallel")?.unwrap_or(false);
            let run: JsValue = declaration
                .get("run")
                .map_err(|error| js_error(ctx, error))?;
            let run = run.into_function().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    format!("工具 `{name}` 缺少 run 函数"),
                )
            })?;
            tool_functions.insert(name.clone(), Persistent::save(ctx, run));
            descriptor.tools.push(ToolDecl {
                name,
                description,
                parameters,
                tier,
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
                    format!("命令名 `{name}` 不合法（与插件名同规则，用作 /{name}）"),
                ));
            }
            let value: JsValue = commands
                .get(name.as_str())
                .map_err(|error| js_error(ctx, error))?;
            let declaration = value.into_object().ok_or_else(|| {
                PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    format!("命令 `{name}` 的声明必须是对象"),
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
                    format!("命令 `{name}` 缺少 run 函数"),
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
            if !matches!(name.as_str(), "beforeToolCall" | "afterToolCall") {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    format!("未知钩子 `{name}`（支持 beforeToolCall / afterToolCall）"),
                ));
            }
            let value: JsValue = hooks
                .get(name.as_str())
                .map_err(|error| js_error(ctx, error))?;
            if value.is_undefined() {
                continue;
            }
            if !value.is_function() {
                return Err(PluginError::new(
                    "E_PLUGIN_DESCRIPTOR",
                    format!("钩子 `{name}` 必须是函数"),
                ));
            }
            descriptor.hooks.push(match name.as_str() {
                "beforeToolCall" => HookKind::BeforeToolCall,
                _ => HookKind::AfterToolCall,
            });
        }
        descriptor.hooks.sort_unstable();
        descriptor.hooks.dedup();
    }

    let prompt_value: JsValue = object.get("prompt").map_err(|error| js_error(ctx, error))?;
    if !prompt_value.is_undefined() {
        if !prompt_value.is_function() {
            return Err(PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                "`prompt` 必须是函数",
            ));
        }
        descriptor.has_prompt = true;
    }

    Ok((
        descriptor,
        Functions {
            export: Persistent::save(ctx, object.clone()),
            tools: tool_functions,
            commands: command_functions,
        },
    ))
}

fn required_text<'js>(
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
            format!("`{owner}` 的 `{key}` 必须是非空字符串"),
        )),
    }
}

fn optional_string<'js>(ctx: &Ctx<'js>, object: &Object<'js>, key: &str) -> Result<Option<String>> {
    let value: JsValue = object.get(key).map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let Some(value) = value.into_string() else {
        return Err(PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            format!("`{key}` 必须是字符串"),
        ));
    };
    value
        .to_string()
        .map(Some)
        .map_err(|error| js_error(ctx, error))
}

/// 可选对象字段：缺失/`null` 返回 `None`，存在但不是对象直接报错。
fn optional_object<'js>(
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
            format!("`{key}` 必须是对象"),
        )),
    }
}

fn optional_bool<'js>(ctx: &Ctx<'js>, object: &Object<'js>, key: &str) -> Result<Option<bool>> {
    let value: JsValue = object.get(key).map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| PluginError::new("E_PLUGIN_DESCRIPTOR", format!("`{key}` 必须是布尔值")))
}

/// 建宿主桥：`kv`、`log`、`session`、`tools.call`。
fn install_globals<'js>(
    ctx: &Ctx<'js>,
    host: &Rc<RefCell<Option<CallHost>>>,
    kv: &Arc<Kv>,
) -> Result<()> {
    let bridge = Object::new(ctx.clone()).map_err(|error| js_error(ctx, error))?;
    bridge
        .set("version", API_VERSION)
        .map_err(|error| js_error(ctx, error))?;

    let store = Arc::clone(kv);
    let get = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<Option<String>> {
            match store.get(&key) {
                Ok(value) => Ok(value.map(|value| value.to_string())),
                Err(error) => Err(throw(&ctx, &error)),
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("kvGet", get)
        .map_err(|error| js_error(ctx, error))?;

    let store = Arc::clone(kv);
    let set = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, text: String| -> rquickjs::Result<()> {
            let value: Value = serde_json::from_str(&text).map_err(|error| {
                throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_VALUE", format!("kv 值不是合法 JSON：{error}")),
                )
            })?;
            store.set(&key, value).map_err(|error| throw(&ctx, &error))
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("kvSet", set)
        .map_err(|error| js_error(ctx, error))?;

    let store = Arc::clone(kv);
    let delete = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<bool> {
            store.delete(&key).map_err(|error| throw(&ctx, &error))
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("kvDelete", delete)
        .map_err(|error| js_error(ctx, error))?;

    let store = Arc::clone(kv);
    let keys = Function::new(ctx.clone(), move || -> String {
        serde_json::to_string(&store.keys()).unwrap_or_else(|_| "[]".into())
    })
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("kvKeys", keys)
        .map_err(|error| js_error(ctx, error))?;

    let store = Arc::clone(kv);
    let clear = Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<()> {
        store.clear().map_err(|error| throw(&ctx, &error))
    })
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("kvClear", clear)
        .map_err(|error| js_error(ctx, error))?;

    let log = Function::new(
        ctx.clone(),
        move |level: String, message: String| match level.as_str() {
            "debug" => tracing::debug!(target: "moc_plugin", "{message}"),
            "warn" => tracing::warn!(target: "moc_plugin", "{message}"),
            "error" => tracing::error!(target: "moc_plugin", "{message}"),
            _ => tracing::info!(target: "moc_plugin", "{message}"),
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("log", log)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let session = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>| -> rquickjs::Result<String> {
            let borrowed = current.borrow();
            match borrowed.as_ref() {
                Some(call) => Ok(call.session.clone()),
                None => Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_STATE", "没有正在进行的插件调用"),
                )),
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("session", session)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let call_tool = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, name: String, args: String| -> rquickjs::Result<String> {
            let (tx, deadline, allowed) = {
                let borrowed = current.borrow();
                match borrowed.as_ref() {
                    Some(call) => (call.tx.clone(), call.deadline, call.tools_allowed),
                    None => {
                        return Err(throw(
                            &ctx,
                            &PluginError::new("E_PLUGIN_STATE", "没有正在进行的插件调用"),
                        ));
                    }
                }
            };
            if !allowed {
                return Err(throw(
                    &ctx,
                    &PluginError::new(
                        "E_PLUGIN_TOOLS_UNAVAILABLE",
                        "只有工具的 run 里可以调用宿主工具",
                    ),
                ));
            }
            let Some(tx) = tx else {
                return Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_STATE", "宿主桥没有登记工具调用"),
                ));
            };
            let args: Value = serde_json::from_str(&args).map_err(|error| {
                throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_VALUE", format!("工具参数不是合法 JSON：{error}")),
                )
            })?;
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            if tx
                .send(HostRequest {
                    name,
                    args,
                    reply: reply_tx,
                })
                .is_err()
            {
                return Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_STATE", "宿主调用通道已关闭"),
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match reply_rx.recv_timeout(remaining.max(Duration::from_millis(1))) {
                Ok(Ok(value)) => Ok(value.to_string()),
                Ok(Err(error)) => Err(throw(&ctx, &error)),
                Err(_) => Err(throw(
                    &ctx,
                    &PluginError::new("E_PLUGIN_TIMEOUT", "宿主工具调用超时"),
                )),
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("toolCall", call_tool)
        .map_err(|error| js_error(ctx, error))?;

    ctx.globals()
        .set("__moc", bridge)
        .map_err(|error| js_error(ctx, error))
}

/// 一次待执行的插件调用：宿主在调用期间登记它与会话的关联。
struct Invocation {
    op: CallOp,
    payload: Value,
    session: String,
    tools_allowed: bool,
    deadline: Instant,
    host_tx: Option<tokio::sync::mpsc::UnboundedSender<HostRequest>>,
}

fn invoke<'js>(
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
    });
    let result = dispatch(ctx, functions, op, &payload, &session);
    *host.borrow_mut() = None;
    result
}

fn dispatch<'js>(
    ctx: &Ctx<'js>,
    functions: &Functions,
    op: CallOp,
    payload: &Value,
    session: &str,
) -> Result<Value> {
    let context_value = || -> Result<JsValue<'js>> {
        ctx.json_parse(session)
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
            function
                .call::<_, JsValue>(())
                .map_err(|error| js_error(ctx, error))?;
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
            js_to_json(ctx, result)
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
            js_to_json(ctx, result)
        }
        CallOp::BeforeToolCall | CallOp::AfterToolCall => {
            let export = export_object(ctx, functions)?;
            let hooks: JsValue = export.get("hooks").map_err(|error| js_error(ctx, error))?;
            let hooks = hooks
                .into_object()
                .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "模块没有导出 hooks"))?;
            let function = optional_function(ctx, &hooks, op_name(op))?.ok_or_else(|| {
                PluginError::new("E_PLUGIN_STATE", format!("钩子 `{}` 不存在", op_name(op)))
            })?;
            let event: JsValue = ctx
                .json_parse(payload.to_string())
                .map_err(|error| js_error(ctx, error))?;
            let result: JsValue = function
                .call((event, context_value()?))
                .map_err(|error| js_error(ctx, error))?;
            js_to_json(ctx, result)
        }
        CallOp::Prompt => {
            let export = export_object(ctx, functions)?;
            let function = optional_function(ctx, &export, "prompt")?
                .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "模块没有导出 prompt"))?;
            let result: JsValue = function
                .call::<_, JsValue>((context_value()?,))
                .map_err(|error| js_error(ctx, error))?;
            js_to_json(ctx, result)
        }
    }
}

fn op_name(op: CallOp) -> &'static str {
    match op {
        CallOp::BeforeToolCall => "beforeToolCall",
        CallOp::AfterToolCall => "afterToolCall",
        _ => "unknown",
    }
}

fn export_object<'js>(ctx: &Ctx<'js>, functions: &Functions) -> Result<Object<'js>> {
    functions
        .export
        .clone()
        .restore(ctx)
        .map_err(|error| js_error(ctx, error))
}

fn optional_function<'js>(
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

fn js_to_json<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Result<Value> {
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

fn throw(ctx: &Ctx<'_>, error: &PluginError) -> rquickjs::Error {
    match rquickjs::Exception::from_message(ctx.clone(), &error.message) {
        Ok(exception) => {
            let _ = exception.as_object().set("code", error.code);
            exception.throw()
        }
        Err(error) => error,
    }
}

fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> PluginError {
    caught(ctx, CaughtError::from_error(ctx, error))
}

fn caught(_ctx: &Ctx<'_>, error: CaughtError<'_>) -> PluginError {
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
            PluginError::new(code, message)
        }
        CaughtError::Value(value) => PluginError::new("E_PLUGIN", value.type_name().to_owned()),
        CaughtError::Error(error) => map_error(error),
    }
}

fn default_code(message: &str) -> &'static str {
    let lowered = message.to_lowercase();
    if lowered.contains("out of memory") {
        "E_PLUGIN_MEMORY"
    } else if lowered.contains("interrupt") {
        "E_PLUGIN_INTERRUPTED"
    } else {
        "E_PLUGIN"
    }
}

fn map_error(error: rquickjs::Error) -> PluginError {
    PluginError::new(default_code(&error.to_string()), error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits(call: Duration) -> Limits {
        Limits {
            memory_limit_bytes: 32 * 1024 * 1024,
            call_timeout: call,
            load_timeout: Duration::from_secs(2),
        }
    }

    fn write_plugin(dir: &Path, main: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"name":"demo","main":"{main}"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join(main), body).unwrap();
    }

    async fn load(dir: &Path, call: Duration) -> (Worker, Descriptor, Arc<Kv>) {
        let kv = Arc::new(Kv::open(&dir.join("data"), "demo", 1 << 20));
        let (worker, ready) = spawn(
            "demo".into(),
            dir.to_path_buf(),
            dir.join("index.js"),
            Arc::clone(&kv),
            limits(call),
        );
        let descriptor = ready.await.expect("插件线程存活").expect("插件加载成功");
        (worker, descriptor, kv)
    }

    async fn call(
        worker: &Worker,
        op: CallOp,
        payload: Value,
        session: &str,
        tools_allowed: bool,
        timeout: Duration,
    ) -> Result<Value> {
        let (reply, ready) = tokio::sync::oneshot::channel();
        worker.call(CallRequest {
            op,
            payload,
            context: session.to_owned(),
            tools_allowed,
            deadline: Instant::now() + timeout,
            host_tx: None,
            reply,
        })?;
        ready.await.expect("调用方存活")
    }

    const SESSION: &str = r#"{"sessionId":"s1","cwd":"/tmp","mode":"normal","permission":"full","model":"m","isSubagent":false}"#;

    #[tokio::test]
    async fn evaluates_tools_commands_and_kv() {
        let temp = tempfile::tempdir().unwrap();
        write_plugin(
            temp.path(),
            "index.js",
            r#"
            import { kv, session } from "moc";
            export default {
              tools: {
                greet: {
                  description: "打招呼",
                  parameters: { type: "object", properties: { name: { type: "string" } } },
                  run(input, ctx) { kv.set("last", input); return `hi ${input.name} from ${ctx.model}`; }
                }
              },
              commands: { echo: { description: "回显", run(args) { return `echo:${args}`; } } },
              prompt(ctx) { return `plugin prompt for ${ctx.mode}`; }
            };
            "#,
        );
        let (worker, descriptor, kv) = load(temp.path(), Duration::from_secs(2)).await;
        assert_eq!(descriptor.tools.len(), 1);
        assert_eq!(descriptor.tools[0].tier, ToolTier::Read);
        assert_eq!(descriptor.commands[0].name, "echo");
        assert!(descriptor.has_prompt);

        let value = call(
            &worker,
            CallOp::Tool,
            json!({"name":"greet","args":{"name":"moc"}}),
            SESSION,
            true,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(value, json!("hi moc from m"));
        assert_eq!(kv.get("last").unwrap(), Some(json!({"name":"moc"})));

        let value = call(
            &worker,
            CallOp::Command,
            json!({"name":"echo","args":"abc"}),
            SESSION,
            false,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(value, json!("echo:abc"));

        let value = call(
            &worker,
            CallOp::Prompt,
            Value::Null,
            SESSION,
            false,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(value, json!("plugin prompt for normal"));
    }

    #[tokio::test]
    async fn hooks_receive_events_and_can_return_rewrites() {
        let temp = tempfile::tempdir().unwrap();
        write_plugin(
            temp.path(),
            "index.js",
            r#"
            export default {
              hooks: {
                beforeToolCall(event) {
                  if (event.name === "blocked") return { deny: "不允许调用" };
                  return { arguments: { ...event.arguments, injected: true } };
                }
              }
            };
            "#,
        );
        let (worker, descriptor, _) = load(temp.path(), Duration::from_secs(2)).await;
        assert!(descriptor.has_hook(HookKind::BeforeToolCall));
        let value = call(
            &worker,
            CallOp::BeforeToolCall,
            json!({"name":"read","arguments":{"path":"a"}}),
            SESSION,
            false,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(value["arguments"]["injected"], json!(true));
        let value = call(
            &worker,
            CallOp::BeforeToolCall,
            json!({"name":"blocked","arguments":{}}),
            SESSION,
            false,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(value["deny"], json!("不允许调用"));
    }

    #[tokio::test]
    async fn tool_calls_require_the_host_flag_and_an_open_bridge() {
        let temp = tempfile::tempdir().unwrap();
        write_plugin(
            temp.path(),
            "index.js",
            r#"
            import { tools } from "moc";
            export default {
              tools: {
                nested: {
                  description: "调用宿主工具",
                  parameters: { type: "object" },
                  run() { return tools.call("read", { path: "x" }); }
                }
              }
            };
            "#,
        );
        let (worker, _, _) = load(temp.path(), Duration::from_secs(2)).await;
        let error = call(
            &worker,
            CallOp::Tool,
            json!({"name":"nested","args":{}}),
            SESSION,
            false,
            Duration::from_secs(2),
        )
        .await
        .expect_err("未授权调用");
        assert_eq!(error.code, "E_PLUGIN_TOOLS_UNAVAILABLE");
    }

    #[tokio::test]
    async fn interrupting_stops_a_runaway_loop() {
        let temp = tempfile::tempdir().unwrap();
        write_plugin(
            temp.path(),
            "index.js",
            r#"
            export default {
              tools: { spin: { description: "死循环", parameters: { type: "object" }, run() { while (true) {} } } }
            };
            "#,
        );
        let (worker, _, _) = load(temp.path(), Duration::from_secs(2)).await;
        let (reply, ready) = tokio::sync::oneshot::channel();
        worker
            .call(CallRequest {
                op: CallOp::Tool,
                payload: json!({"name":"spin","args":{}}),
                context: SESSION.into(),
                tools_allowed: false,
                deadline: Instant::now() + Duration::from_millis(200),
                host_tx: None,
                reply,
            })
            .unwrap();
        // 模拟宿主超时：置中断标志。
        tokio::time::sleep(Duration::from_millis(100)).await;
        worker.interrupt();
        let error = tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .expect("中断后应返回")
            .expect("调用方存活")
            .expect_err("死循环必须失败");
        assert_eq!(error.code, "E_PLUGIN_INTERRUPTED", "{error}");
    }

    #[tokio::test]
    async fn rejects_bad_descriptors_and_escaped_imports() {
        let temp = tempfile::tempdir().unwrap();
        let cases = [
            ("export default 1;", "E_PLUGIN_DESCRIPTOR"),
            (
                r#"export default { tools: { bad: { description: "x", parameters: { type: "object" }, run() {} } }, typo: 1 };"#,
                "E_PLUGIN_DESCRIPTOR",
            ),
            (
                r#"import "../outside.js"; export default {};"#,
                "E_PLUGIN_IMPORT",
            ),
            (
                r#"export default { tools: "nope" };"#,
                "E_PLUGIN_DESCRIPTOR",
            ),
            (
                r#"export default { tools: { x: { description: "x", parameters: { type: "object" }, run() {} } }, hooks: { nope() {} } };"#,
                "E_PLUGIN_DESCRIPTOR",
            ),
        ];
        for (index, (body, code)) in cases.iter().enumerate() {
            let dir = temp.path().join(format!("case{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
            std::fs::write(dir.join("index.js"), body).unwrap();
            let kv = Arc::new(Kv::open(&dir.join("data"), "demo", 1 << 20));
            let (_worker, ready) = spawn(
                "demo".into(),
                dir.clone(),
                dir.join("index.js"),
                kv,
                limits(Duration::from_secs(1)),
            );
            let error = ready.await.expect("线程存活").expect_err("必须失败");
            assert!(error.to_string().contains(code), "{body} -> {error}");
        }
    }
}
