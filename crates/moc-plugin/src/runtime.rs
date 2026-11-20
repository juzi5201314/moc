//! 插件运行时：每个插件一个专用线程，线程内建 QuickJS `Runtime`/`Context`。
//!
//! 线程内的一切都是同步的：JS 调 `moc.tools.call()` 时通过通道把请求发给宿主、
//! 阻塞等回包；宿主在 `Plugin::invoke` 的 `select!` 里服务这些请求。因此不需要
//! PTC 那套 step 状态机。
//!
//! 中断：QuickJS 的 interrupt handler 同时检查宿主标志、调用期限与结果接收端，
//! 即使宿主 future 被丢弃也能停止 JS。被中断的线程直接退出，不再复用 Runtime。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Module, Object, Persistent, Runtime, Value as JsValue};
use rust_i18n::t;
use serde_json::Value;

use crate::descriptor::{AgentPresetDecl, ConfigDecl, Descriptor, HookKind};
use crate::error::{PluginError, Result};
use crate::host::PersistenceOp;
use crate::loader::{PluginLoader, PluginResolver};

/// 宿主 API 版本；插件可用 `version` 判断兼容性。
///
/// `"2"` 在 `"1"` 之上增加了生命周期事件钩子；返回值契约见 `docs/plugins.md`。
pub const API_VERSION: &str = "2";
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
    /// 生命周期钩子；`payload` 是交给 JS 的 event，`context` 是会话快照。
    Hook(HookKind),
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
                t!("plugin.err.thread_gone", name = self.name.as_str()).into_owned(),
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
            t!("plugin.err.thread_spawn", error = error).into_owned(),
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

/// 由 worker 持有调用预算和回包通道，取消不能把中断机制一起丢掉。
struct CallInterrupt {
    deadline: Instant,
    reply: Option<tokio::sync::oneshot::Sender<Result<Value>>>,
    interrupted: bool,
    timed_out: bool,
}

struct Registrations {
    fields: Option<Vec<ConfigDecl>>,
    agent_presets: Option<Vec<AgentPresetDecl>>,
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
    let active: Rc<RefCell<Option<CallInterrupt>>> = Rc::new(RefCell::new(None));
    let current = Rc::clone(&active);
    let flag = Arc::clone(&interrupt);
    runtime.set_interrupt_handler(Some(Box::new(move || {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        let mut current = current.borrow_mut();
        let Some(call) = current.as_mut() else {
            return false;
        };
        call.timed_out |= Instant::now() >= call.deadline;
        call.interrupted |=
            call.timed_out || call.reply.as_ref().is_some_and(|reply| reply.is_closed());
        call.interrupted
    })));
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
                t!("plugin.err.watchdog_spawn", error = error).into_owned(),
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
                t!(
                    "plugin.err.load_timeout",
                    ms = limits.load_timeout.as_millis()
                )
                .into_owned(),
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
                // 退出钩子也必须受期限约束，不能在回收线程时再次跑飞。
                *active.borrow_mut() = Some(CallInterrupt {
                    deadline,
                    reply: None,
                    interrupted: false,
                    timed_out: false,
                });
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
                // 排队期间已取消的请求不执行，也不影响同一插件的其他调用。
                if reply.is_closed() {
                    continue;
                }
                *active.borrow_mut() = Some(CallInterrupt {
                    deadline,
                    reply: Some(reply),
                    interrupted: false,
                    timed_out: false,
                });
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
                let call = active.borrow_mut().take().expect("调用中断上下文已登记");
                // worker 可能先于宿主定时器发现超时，错误码仍须保持一致。
                let result = if call.timed_out {
                    Err(PluginError::new(
                        "E_PLUGIN_TIMEOUT",
                        t!("plugin.err.budget_exhausted"),
                    ))
                } else {
                    result
                };
                let _ = call.reply.expect("普通调用必须带回包通道").send(result);
                // 中断后的 JS 状态不再复用；关闭队列也会唤醒尚在排队的调用方。
                if call.interrupted || interrupt.load(Ordering::SeqCst) {
                    break;
                }
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
        agent_presets: Some(Vec::new()),
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
            t!(
                "plugin.err.entry_read",
                file = entry.display().to_string(),
                error = error
            )
            .into_owned(),
        )
    })?;
    let module =
        Module::declare(ctx.clone(), name, source).map_err(|error| js_error(ctx, error))?;
    let (module, promise) = module.eval().map_err(|error| js_error(ctx, error))?;
    promise.finish::<()>().map_err(|error| match error {
        // 没有定时器与事件循环：顶层 await 等不到的东西永远等不到。
        rquickjs::Error::WouldBlock => PluginError::new(
            "E_PLUGIN_STATE",
            t!("plugin.err.await_unsettled").into_owned(),
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
            t!("plugin.err.default_export").into_owned(),
        )
    })?;
    if let Some(error) = registrations.borrow_mut().error.take() {
        return Err(error);
    }
    let (configs, agent_presets) = {
        let mut registry = registrations.borrow_mut();
        (
            registry.fields.take().expect("注册表只冻结一次"),
            registry.agent_presets.take().expect("注册表只冻结一次"),
        )
    };
    let (mut descriptor, functions) = parse_descriptor(ctx, &object)?;
    descriptor.configs = configs;
    descriptor.agent_presets = agent_presets;
    let _ = ctx.eval::<(), _>("delete globalThis.__moc");
    Ok((descriptor, functions))
}

mod descriptor;
use descriptor::parse_descriptor;
mod globals;
use globals::install_globals;
mod invocation;
use invocation::invoke;
use invocation::js_error;

/// preset 系统提示的长度上限；与插件提示词贡献预算同量级。
const MAX_PRESET_PROMPT_CHARS: usize = 4000;

/// 一次待执行的插件调用：宿主在调用期间登记它与会话的关联。
struct Invocation {
    op: CallOp,
    payload: Value,
    session: String,
    tools_allowed: bool,
    deadline: Instant,
    host_tx: Option<tokio::sync::mpsc::UnboundedSender<HostRequest>>,
}

#[cfg(test)]
mod tests;
