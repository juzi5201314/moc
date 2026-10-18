//! PTC guest：每个实例只执行一段 JavaScript，宿主通过 start/resume 推进其中待完成的 Promise。

#[path = "../../../crates/moc-core/src/tools/ptc/wire.rs"]
mod wire;

use rquickjs::{Context, Function, Object, Persistent, Runtime};
use std::{cell::RefCell, slice};
use wire::{GuestState, GuestStep, HostCompletion, ScriptError, StartRequest};

struct Script {
    control: Persistent<Object<'static>>,
    context: Context,
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
}

fn error(code: &str, message: impl ToString) -> ScriptError {
    ScriptError { code: code.into(), message: message.to_string(), result: None }
}

fn failed(code: &str, message: impl ToString) -> GuestStep {
    GuestStep { requests: Vec::new(), state: GuestState::Failed { error: error(code, message) } }
}

fn drive(script: &Script) -> GuestStep {
    script.context.with(|ctx| {
        while ctx.execute_pending_job() {}
        let control = match script.control.clone().restore(&ctx) {
            Ok(control) => control,
            Err(err) => return failed("E_PTC_PROTOCOL", err),
        };
        let read = |name| -> Result<String, rquickjs::Error> {
            control.get::<_, Function>(name)?.call(())
        };
        let decoded = (|| -> Result<_, ScriptError> {
            let requests = read("take").map_err(|err| error("E_PTC_PROTOCOL", err))?;
            let status = read("status").map_err(|err| error("E_PTC_PROTOCOL", err))?;
            let requests = serde_json::from_str(&requests).map_err(|err| error("E_PTC_PROTOCOL", err))?;
            let state = serde_json::from_str(&status).map_err(|err| error("E_PTC_PROTOCOL", err))?;
            Ok((requests, state))
        })();
        let (requests, state) = match decoded {
            Ok(value) => value,
            Err(error) => return GuestStep { requests: Vec::new(), state: GuestState::Failed { error } },
        };
        GuestStep { requests, state }
    })
}

fn start(bytes: &[u8]) -> GuestStep {
    let request: StartRequest = match serde_json::from_slice(bytes) {
        Ok(request) => request,
        Err(err) => return failed("E_PTC_PROTOCOL", err),
    };
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => return failed("E_MEMORY_LIMIT", err),
    };
    runtime.set_max_stack_size(256 * 1024);
    runtime.set_memory_limit(request.memory_limit_bytes);
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(err) => return failed("E_MEMORY_LIMIT", err),
    };
    let setup = context.with(|ctx| -> rquickjs::Result<_> {
        let control: Object = ctx.eval(include_str!("prelude.js"))?;
        control.get::<_, Function>("start")?.call::<_, ()>((request.code, request.max_pending_calls, request.max_output_bytes))?;
        Ok(Persistent::save(&ctx, control))
    });
    let control = match setup {
        Ok(control) => control,
        Err(err) => return failed("E_JS", err),
    };
    SCRIPT.with(|slot| {
        let mut slot = slot.borrow_mut();
        *slot = Some(Script { control, context });
        drive(slot.as_ref().expect("script 刚刚设置"))
    })
}

fn resume(bytes: &[u8]) -> GuestStep {
    let completions: Vec<HostCompletion> = match serde_json::from_slice(bytes) {
        Ok(completions) => completions,
        Err(err) => return failed("E_PTC_PROTOCOL", err),
    };
    SCRIPT.with(|slot| {
        let slot = slot.borrow();
        let Some(script) = slot.as_ref() else { return failed("E_PTC_PROTOCOL", "guest has not started"); };
        let submitted = script.context.with(|ctx| -> rquickjs::Result<()> {
            let control = script.control.clone().restore(&ctx)?;
            let complete: Function = control.get("complete")?;
            for completion in &completions {
                let (value, error) = match &completion.result {
                    Ok(value) => (serde_json::to_string(value).expect("JSON 可序列化"), "null".into()),
                    Err(err) => ("null".into(), serde_json::to_string(err).expect("错误可序列化")),
                };
                let value = ctx.json_parse(value)?;
                let error = ctx.json_parse(error)?;
                complete.call::<_, ()>((completion.id, value, error))?;
            }
            Ok(())
        });
        match submitted {
            Ok(()) => drive(script),
            Err(err) => failed("E_PTC_PROTOCOL", err),
        }
    })
}

fn respond(step: GuestStep) -> u64 {
    let bytes = serde_json::to_vec(&step).expect("guest 结果可序列化").into_boxed_slice();
    let len = u32::try_from(bytes.len()).expect("WASM32 单个缓冲区不会超出地址空间");
    let ptr = Box::into_raw(bytes) as *mut u8 as u32;
    (u64::from(len) << 32) | u64::from(ptr)
}

#[unsafe(no_mangle)]
pub extern "C" fn ptc_abi_version() -> u32 { wire::ABI_VERSION }

#[unsafe(no_mangle)]
pub extern "C" fn ptc_alloc(len: u32) -> u32 {
    let bytes = vec![0u8; len as usize].into_boxed_slice();
    Box::into_raw(bytes) as *mut u8 as u32
}

/// # Safety
/// `ptr` 必须来自 ptc_alloc 或 respond，`len` 必须是分配时的确切长度，且未被释放。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ptc_free(ptr: u32, len: u32) {
    if len > 0 {
        // SAFETY: 由 ABI 调用者保证指针及长度匹配原 Box<[u8]> 分配。
        unsafe { drop(Box::from_raw(slice::from_raw_parts_mut(ptr as *mut u8, len as usize))); }
    }
}

/// # Safety
/// `ptr` 必须指向有效的 `len` 字节输入缓冲区，且调用期间不发生别名写入。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ptc_start(ptr: u32, len: u32) -> u64 {
    // SAFETY: 宿主按 ABI 分配与写入，长度经内存边界检查。
    let bytes = unsafe { slice::from_raw_parts(ptr as *const u8, len as usize) };
    respond(start(bytes))
}

/// # Safety
/// `ptr` 必须指向有效的 `len` 字节输入缓冲区，且调用期间不发生别名写入。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ptc_resume(ptr: u32, len: u32) -> u64 {
    // SAFETY: 宿主按 ABI 分配与写入，长度经内存边界检查。
    let bytes = unsafe { slice::from_raw_parts(ptr as *const u8, len as usize) };
    respond(resume(bytes))
}
