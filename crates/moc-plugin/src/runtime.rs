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

use crate::descriptor::{
    CommandDecl, ConfigDecl, ConfigKind, Descriptor, HookKind, ModeMask, ToolDecl,
};
use crate::error::{PluginError, Result};
use crate::host::PersistenceOp;
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

/// JS 发起的一次宿主调用；持久化不经过工具白名单。
pub(crate) struct HostRequest {
    pub operation: HostOperation,
    pub reply: std::sync::mpsc::Sender<Result<Value>>,
}

pub(crate) enum HostOperation {
    Tool { name: String, args: Value },
    Persistence(PersistenceOp),
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

/// 单测兼容入口：模块求值后直接激活。
#[cfg(test)]
pub(crate) fn spawn(
    name: String,
    dir: PathBuf,
    entry: PathBuf,
    limits: Limits,
) -> (Worker, tokio::sync::oneshot::Receiver<Result<Descriptor>>) {
    spawn_mode(name, dir, entry, limits, true)
}

pub(crate) fn spawn_unactivated(
    name: String,
    dir: PathBuf,
    entry: PathBuf,
    limits: Limits,
) -> (Worker, tokio::sync::oneshot::Receiver<Result<Descriptor>>) {
    spawn_mode(name, dir, entry, limits, false)
}

fn spawn_mode(
    name: String,
    dir: PathBuf,
    entry: PathBuf,
    limits: Limits,
    activate_on_load: bool,
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
        .spawn(move || {
            run_thread(
                dir,
                entry,
                limits,
                interrupt,
                command_rx,
                ready_tx,
                activate_on_load,
            )
        });
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
    fault: Option<PluginError>,
    /// 仅活跃回调内缓存；首次 state 读取后点查和枚举不重复访问磁盘。
    state: Option<serde_json::Map<String, Value>>,
}

struct Registrations {
    fields: Option<Vec<ConfigDecl>>,
    error: Option<PluginError>,
}

struct Functions {
    export: Persistent<Object<'static>>,
    context: Persistent<Function<'static>>,
    tools: BTreeMap<String, Persistent<Function<'static>>>,
    commands: BTreeMap<String, Persistent<Function<'static>>>,
}

/// 加载阶段的看门狗：模块求值与 `activate` 都在它的预算里。
///
/// 求值发生在同步的插件线程上，宿主没有别的办法打断它，只能靠 QuickJS 的 interrupt
/// handler：到点置标志，JS 执行期间轮询到就抛出不可捕获异常。
struct LoadGuard {
    stop: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
    fired: Arc<AtomicBool>,
}

impl LoadGuard {
    fn start(interrupt: &Arc<AtomicBool>, timeout: Duration) -> std::io::Result<Self> {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(interrupt);
        let timed_out = Arc::clone(&fired);
        let handle = std::thread::Builder::new()
            .name("moc-plugin-load".into())
            .spawn(move || {
                if stopped.recv_timeout(timeout).is_err() {
                    timed_out.store(true, Ordering::SeqCst);
                    flag.store(true, Ordering::SeqCst);
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
            fired,
        })
    }

    /// 结束看门狗、复位中断标志，返回它是否真的到点过。
    fn finish(mut self, interrupt: &Arc<AtomicBool>) -> bool {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        interrupt.store(false, Ordering::SeqCst);
        self.fired.load(Ordering::SeqCst)
    }
}

impl Drop for LoadGuard {
    /// 线程异常退出时也要放走看门狗，别让它把「调用方没了」记成超时。
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

fn run_thread(
    dir: PathBuf,
    entry: PathBuf,
    limits: Limits,
    interrupt: Arc<AtomicBool>,
    commands: std::sync::mpsc::Receiver<Command>,
    ready: tokio::sync::oneshot::Sender<Result<Descriptor>>,
    activate_on_load: bool,
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
    // 看门狗覆盖模块求值与 activate：两者都是同步 JS，跑飞了只能靠中断标志收场。
    let guard = match LoadGuard::start(&interrupt, limits.load_timeout) {
        Ok(guard) => guard,
        Err(error) => {
            let _ = ready.send(Err(PluginError::new(
                "E_PLUGIN_THREAD",
                format!("无法启动加载看门狗：{error}"),
            )));
            return;
        }
    };
    let deadline = Instant::now() + limits.load_timeout;
    let loaded =
        context
            .with(|ctx| init(&ctx, &entry, &host))
            .and_then(|(descriptor, functions)| {
                if activate_on_load {
                    context.with(|ctx| {
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
                    })?;
                }
                Ok((descriptor, functions))
            });
    // 无论成功与否都要停掉看门狗并复位中断标志，否则后续调用会被立刻打断。
    let timed_out = guard.finish(&interrupt);
    let (descriptor, functions) = match (loaded, timed_out) {
        // 加载在窗口内做完了：看门狗是否在边界上响过都不影响结果。
        (Ok(value), _) => value,
        (Err(_), true) => {
            let _ = ready.send(Err(PluginError::new(
                "E_PLUGIN_TIMEOUT",
                format!(
                    "插件在 {} ms 内没有完成加载（模块求值 + activate）",
                    limits.load_timeout.as_millis()
                ),
            )));
            return;
        }
        (Err(error), false) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
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
) -> Result<(Descriptor, Functions)> {
    let registrations = Rc::new(RefCell::new(Registrations {
        fields: Some(Vec::new()),
        error: None,
    }));
    install_globals(ctx, host, &registrations)?;
    let (_, prelude) = Module::declare(ctx.clone(), "moc", crate::loader::PRELUDE)
        .and_then(Module::eval)
        .map_err(|error| js_error(ctx, error))?;
    prelude
        .finish::<()>()
        .map_err(|error| js_error(ctx, error))?;
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
    promise.finish::<()>().map_err(|error| match error {
        // 没有定时器与事件循环：顶层 await 等不到的东西永远等不到。
        rquickjs::Error::WouldBlock => PluginError::new(
            "E_PLUGIN_STATE",
            "模块顶层 await 没有落定：插件里没有定时器或事件循环",
        ),
        error => js_error(ctx, error),
    })?;
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
    if let Some(error) = registrations.borrow_mut().error.take() {
        return Err(error);
    }
    let configs = registrations
        .borrow_mut()
        .fields
        .take()
        .expect("注册表只冻结一次");
    let (mut descriptor, functions) = parse_descriptor(ctx, &object)?;
    descriptor.configs = configs;
    let _ = ctx.eval::<(), _>("delete globalThis.__moc");
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
            for key in declaration.keys::<String>() {
                let key = key.map_err(|error| js_error(ctx, error))?;
                if !["description", "parameters", "modes", "parallel", "run"]
                    .contains(&key.as_str())
                {
                    return Err(PluginError::new(
                        "E_PLUGIN_DESCRIPTOR",
                        format!("工具 `{name}` 的声明里有未知字段 `{key}`"),
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
                    format!("工具 `{name}` 的 parameters 必须是 JSON Schema 对象"),
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
                    format!("工具 `{name}` 缺少 run 函数"),
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

/// 可选 `modes`：缺失时三种模式全开；存在时必须是 `normal`/`ask`/`plan`
/// 的非空字符串数组。`ptc` 不开放：PTC 只暴露 `run_code`，插件工具在那里不可达。
fn optional_modes<'js>(ctx: &Ctx<'js>, object: &Object<'js>, owner: &str) -> Result<ModeMask> {
    let value: JsValue = object.get("modes").map_err(|error| js_error(ctx, error))?;
    if value.is_undefined() {
        return Ok(ModeMask::ALL);
    }
    let items = value.into_array().ok_or_else(|| {
        PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            format!("工具 `{owner}` 的 modes 必须是字符串数组"),
        )
    })?;
    let mut modes = ModeMask::empty();
    for item in items.iter::<JsValue>() {
        let item = item.map_err(|error| js_error(ctx, error))?;
        let name = item.into_string().ok_or_else(|| {
            PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                format!("工具 `{owner}` 的 modes 必须是字符串数组"),
            )
        })?;
        let name = name.to_string().map_err(|error| js_error(ctx, error))?;
        modes = modes.union(ModeMask::parse(&name).ok_or_else(|| {
            PluginError::new(
                "E_PLUGIN_DESCRIPTOR",
                format!("工具 `{owner}` 的 modes 含未知模式 `{name}`（normal/ask/plan）"),
            )
        })?);
    }
    if modes.is_empty() {
        return Err(PluginError::new(
            "E_PLUGIN_DESCRIPTOR",
            format!("工具 `{owner}` 的 modes 不能是空数组；不想暴露就删掉这个工具"),
        ));
    }
    Ok(modes)
}

/// 建宿主桥：`log`、`session`、`tools.call`。
fn install_globals<'js>(
    ctx: &Ctx<'js>,
    host: &Rc<RefCell<Option<CallHost>>>,
    registrations: &Rc<RefCell<Registrations>>,
) -> Result<()> {
    let bridge = Object::new(ctx.clone()).map_err(|error| js_error(ctx, error))?;
    bridge
        .set("version", API_VERSION)
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
                Some(call) if call.session != "null" => Ok(call.session.clone()),
                _ => Err(throw(
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
                    operation: HostOperation::Tool { name, args },
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

    let registry = Rc::clone(registrations);
    let current = Rc::clone(host);
    let register = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, definition: String| -> rquickjs::Result<()> {
            let result = (|| -> Result<()> {
                let mut registry = registry.borrow_mut();
                let fields = registry
                    .fields
                    .as_mut()
                    .ok_or_else(|| PluginError::new("E_PLUGIN_CONFIG", "配置只能在模块顶层注册"))?;
                let declaration = config_declaration(key, &definition)?;
                if fields.iter().any(|field| field.key == declaration.key) {
                    return Err(PluginError::new("E_PLUGIN_CONFIG", "配置键不能重复注册"));
                }
                fields.push(declaration);
                Ok(())
            })();
            result.map_err(|error| {
                registry.borrow_mut().error.get_or_insert(error.clone());
                if let Some(call) = current.borrow_mut().as_mut() {
                    call.fault = Some(error.clone());
                }
                throw(&ctx, &error)
            })
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("configRegister", register)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let registry = Rc::clone(registrations);
    let invalid = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, message: String| -> rquickjs::Result<()> {
            let error = PluginError::new("E_PLUGIN_VALUE", message);
            registry.borrow_mut().error.get_or_insert(error.clone());
            if let Some(call) = current.borrow_mut().as_mut() {
                call.fault = Some(error.clone());
            }
            Err(throw(&ctx, &error))
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("invalidValue", invalid)
        .map_err(|error| js_error(ctx, error))?;

    let current = Rc::clone(host);
    let persistence = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>,
              operation: String,
              key: String,
              value: String|
              -> rquickjs::Result<String> {
            let result = (|| -> Result<Value> {
                let (tx, deadline) = {
                    let borrowed = current.borrow();
                    let call = borrowed.as_ref().ok_or_else(|| {
                        PluginError::new("E_PLUGIN_STATE", "没有正在进行的插件调用")
                    })?;
                    (
                        call.tx
                            .clone()
                            .ok_or_else(|| PluginError::new("E_PLUGIN_STATE", "持久化桥不可用"))?,
                        call.deadline,
                    )
                };
                let op = match operation.as_str() {
                    "configGet" => PersistenceOp::ConfigGet { key },
                    "configSet" => PersistenceOp::ConfigSet {
                        key,
                        value: serde_json::from_str(&value).map_err(|error| {
                            PluginError::new("E_PLUGIN_VALUE", error.to_string())
                        })?,
                    },
                    "stateGet" => PersistenceOp::StateGet { key },
                    "stateSet" => PersistenceOp::StateSet {
                        key,
                        value: serde_json::from_str(&value).map_err(|error| {
                            PluginError::new("E_PLUGIN_VALUE", error.to_string())
                        })?,
                    },
                    "stateDelete" => PersistenceOp::StateDelete { key },
                    "stateKeys" => PersistenceOp::StateKeys,
                    _ => return Err(PluginError::new("E_PLUGIN_STATE", "未知持久化操作")),
                };
                let request = |op| -> Result<Value> {
                    let (reply, receiver) = std::sync::mpsc::channel();
                    tx.send(HostRequest {
                        operation: HostOperation::Persistence(op),
                        reply,
                    })
                    .map_err(|_| PluginError::new("E_PLUGIN_STATE", "持久化桥已关闭"))?;
                    receiver
                        .recv_timeout(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .max(Duration::from_millis(1)),
                        )
                        .map_err(|_| PluginError::new("E_PLUGIN_TIMEOUT", "持久化请求超时"))?
                };
                if let PersistenceOp::StateGet { key }
                | PersistenceOp::StateSet { key, .. }
                | PersistenceOp::StateDelete { key } = &op
                {
                    if key.is_empty() || key.len() > 128 {
                        return Err(PluginError::new(
                            "E_PLUGIN_STATE",
                            "state 键必须是 1..128 UTF-8 字节",
                        ));
                    }
                }
                if matches!(
                    &op,
                    PersistenceOp::StateGet { .. } | PersistenceOp::StateKeys
                ) {
                    let needs_snapshot = current
                        .borrow()
                        .as_ref()
                        .expect("当前调用存在")
                        .state
                        .is_none();
                    if needs_snapshot {
                        let snapshot = request(PersistenceOp::StateSnapshot)?;
                        let object = snapshot
                            .as_object()
                            .ok_or_else(|| {
                                PluginError::new("E_PLUGIN_STATE", "state 快照必须是对象")
                            })?
                            .clone();
                        current.borrow_mut().as_mut().expect("当前调用存在").state = Some(object);
                    }
                    let borrowed = current.borrow();
                    let state = borrowed
                        .as_ref()
                        .and_then(|call| call.state.as_ref())
                        .expect("快照已加载");
                    return Ok(match &op {
                        PersistenceOp::StateGet { key } => match state.get(key) {
                            Some(value) => serde_json::json!({"found":true,"value":value}),
                            None => serde_json::json!({"found":false}),
                        },
                        _ => serde_json::json!(state.keys().collect::<Vec<_>>()),
                    });
                }
                let result = request(op.clone())?;
                if let Some(state) = current
                    .borrow_mut()
                    .as_mut()
                    .and_then(|call| call.state.as_mut())
                {
                    match op {
                        PersistenceOp::StateSet { key, value } => {
                            state.insert(key, value);
                        }
                        PersistenceOp::StateDelete { key } => {
                            state.remove(&key);
                        }
                        _ => {}
                    }
                }
                Ok(result)
            })();
            match result {
                Ok(value) => Ok(value.to_string()),
                Err(error) => {
                    if let Some(call) = current.borrow_mut().as_mut() {
                        call.fault = Some(error.clone());
                    }
                    Err(throw(&ctx, &error))
                }
            }
        },
    )
    .map_err(|error| js_error(ctx, error))?;
    bridge
        .set("persistence", persistence)
        .map_err(|error| js_error(ctx, error))?;

    ctx.globals()
        .set("__moc", bridge)
        .map_err(|error| js_error(ctx, error))
}

fn config_declaration(key: String, definition: &str) -> Result<ConfigDecl> {
    let invalid = |message: &str| PluginError::new("E_PLUGIN_CONFIG", message);
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(invalid("配置键必须是 1–128 字节且不含控制字符"));
    }
    let value: Value =
        serde_json::from_str(definition).map_err(|error| invalid(&error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("配置声明必须是对象"))?;
    if object
        .keys()
        .any(|key| !["type", "default", "label", "description", "choices"].contains(&key.as_str()))
    {
        return Err(invalid("配置声明包含未知字段"));
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
        _ => return Err(invalid("未知配置字段类型")),
    };
    let mut choices = Vec::new();
    if let Some(value) = object.get("choices") {
        let values = value
            .as_array()
            .ok_or_else(|| invalid("choices 必须是字符串数组"))?;
        for item in values {
            let choice = item
                .as_str()
                .ok_or_else(|| invalid("choices 必须是字符串数组"))?
                .to_owned();
            if choices.contains(&choice) {
                return Err(invalid("choices 不能有重复值"));
            }
            choices.push(choice);
        }
    }
    if (kind == ConfigKind::Choice) != !choices.is_empty() {
        return Err(invalid("只有 choice 类型必须提供非空 choices"));
    }
    let text = |name: &str, fallback: &str| -> Result<String> {
        match object.get(name) {
            None => Ok(fallback.to_owned()),
            Some(Value::String(value)) => Ok(value.clone()),
            _ => Err(invalid("label 与 description 必须是字符串")),
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
            .map_err(|error| invalid(&error))?;
    }
    Ok(declaration)
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

fn dispatch<'js>(
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

/// 把插件函数的返回值转成 JSON；返回 Promise 时先驱动 QuickJS 的任务队列直到落定。
///
/// 插件里没有定时器和事件循环，`finish` 在队列耗尽仍 Pending 时返回 `WouldBlock`：
/// 这种 Promise 永远等不到结果，按状态错误报出去，而不是静默变成 `{}`。
fn settle<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Result<Value> {
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

fn caught<'js>(ctx: &Ctx<'js>, error: CaughtError<'js>) -> PluginError {
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
fn thrown_object(value: &rquickjs::Value<'_>) -> Option<PluginError> {
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
fn clip_stack(stack: String) -> String {
    const LIMIT: usize = 2048;
    match stack.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}…", &stack[..end]),
        None => stack,
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

    async fn load(dir: &Path, call: Duration) -> (Worker, Descriptor) {
        let (worker, ready) = spawn(
            "demo".into(),
            dir.to_path_buf(),
            dir.join("index.js"),
            limits(call),
        );
        let descriptor = ready.await.expect("插件线程存活").expect("插件加载成功");
        (worker, descriptor)
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
    async fn evaluates_tools_commands() {
        let temp = tempfile::tempdir().unwrap();
        write_plugin(
            temp.path(),
            "index.js",
            r#"
            import { session } from "moc";
            export default {
              tools: {
                greet: {
                  description: "打招呼",
                  parameters: { type: "object", properties: { name: { type: "string" } } },
                  run(input, ctx) { return `hi ${input.name} from ${ctx.model}`; }
                }
              },
              commands: { echo: { description: "回显", run(args) { return `echo:${args}`; } } },
              prompt(ctx) { return `plugin prompt for ${ctx.mode}`; }
            };
            "#,
        );
        let (worker, descriptor) = load(temp.path(), Duration::from_secs(2)).await;
        assert_eq!(descriptor.tools.len(), 1);
        assert_eq!(descriptor.tools[0].modes, ModeMask::ALL);
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
    async fn tool_modes_filter_the_declared_surface() {
        let temp = tempfile::tempdir().unwrap();
        write_plugin(
            temp.path(),
            "index.js",
            r#"
            export default {
              tools: {
                plan_only: {
                  description: "只在计划模式可见",
                  parameters: { type: "object" },
                  modes: ["plan"],
                  run() { return "ok"; }
                },
                everywhere: {
                  description: "默认全开",
                  parameters: { type: "object" },
                  run() { return "ok"; }
                }
              }
            };
            "#,
        );
        let (_worker, descriptor) = load(temp.path(), Duration::from_secs(2)).await;
        assert_eq!(descriptor.tool("plan_only").unwrap().modes, ModeMask::PLAN);
        assert_eq!(descriptor.tool("everywhere").unwrap().modes, ModeMask::ALL);
    }

    #[tokio::test]
    async fn rejects_unknown_tool_fields() {
        let temp = tempfile::tempdir().unwrap();
        for (index, field) in ["mode", "unexpected"].into_iter().enumerate() {
            let dir = temp.path().join(format!("case{index}"));
            write_plugin(
                &dir,
                "index.js",
                &format!(
                    "export default {{ tools: {{ x: {{ description: \"x\", parameters: {{ type: \"object\" }}, {field}: [\"normal\"], run: () => \"ok\" }} }} }};"
                ),
            );
            let (_worker, ready) = spawn(
                "demo".into(),
                dir.clone(),
                dir.join("index.js"),
                limits(Duration::from_secs(1)),
            );
            let error = ready
                .await
                .expect("插件线程存活")
                .expect_err("未知字段应加载失败");
            assert_eq!(error.code, "E_PLUGIN_DESCRIPTOR");
            assert!(error.message.contains(field), "{error}");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_tool_modes() {
        let temp = tempfile::tempdir().unwrap();
        for (index, modes) in [
            r#"["ptc"]"#,
            r#"[]"#,
            r#""plan""#,
            "null",
            r#"["plan", 1]"#,
            r#"{ toJSON: () => ["plan"] }"#,
        ]
        .into_iter()
        .enumerate()
        {
            let dir = temp.path().join(format!("case{index}"));
            write_plugin(
                &dir,
                "index.js",
                &format!(
                    "export default {{ tools: {{ x: {{ description: \"x\", parameters: {{ type: \"object\" }}, modes: {modes}, run: () => \"ok\" }} }} }};"
                ),
            );
            let (_worker, ready) = spawn(
                "demo".into(),
                dir.clone(),
                dir.join("index.js"),
                limits(Duration::from_secs(1)),
            );
            let error = ready
                .await
                .expect("插件线程存活")
                .expect_err("非法 modes 应加载失败");
            assert_eq!(error.code, "E_PLUGIN_DESCRIPTOR", "modes={modes}");
        }
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
        let (worker, descriptor) = load(temp.path(), Duration::from_secs(2)).await;
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
        let (worker, _) = load(temp.path(), Duration::from_secs(2)).await;
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
        let (worker, _) = load(temp.path(), Duration::from_secs(2)).await;
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
            let (_worker, ready) = spawn(
                "demo".into(),
                dir.clone(),
                dir.join("index.js"),
                limits(Duration::from_secs(1)),
            );
            let error = ready.await.expect("线程存活").expect_err("必须失败");
            assert!(error.to_string().contains(code), "{body} -> {error}");
        }
    }
}
