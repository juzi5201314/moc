//! 内嵌 guest 模块的 Wasmtime 生命周期，以及有长度上限的 ABI 数据交换。

use super::ToolError;
use super::wire::{ABI_VERSION, GuestStep};
use std::sync::Arc;
use wasmtime::{
    Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc,
};

const GUEST: &[u8] = include_bytes!("../../../assets/ptc-guest.wasm");

pub struct PtcRuntime {
    engine: Engine,
    module: Module,
}

struct GuestStore {
    limits: StoreLimits,
}

pub struct Guest {
    store: Store<GuestStore>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    start: TypedFunc<(u32, u32), u64>,
    resume: TypedFunc<(u32, u32), u64>,
}

fn runtime_error(error: impl std::fmt::Debug) -> ToolError {
    ToolError::new("E_PTC_RUNTIME", format!("{error:?}"))
}

impl PtcRuntime {
    pub fn compile() -> Result<Arc<Self>, ToolError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(runtime_error)?;
        let module = Module::new(&engine, GUEST).map_err(runtime_error)?;
        let mut imports = module.imports();
        let Some(import) = imports.next() else {
            return Err(runtime_error("guest is missing the clock import"));
        };
        if import.module() != "env"
            || import.name() != "__rquickjs_host_now_us"
            || imports.next().is_some()
        {
            return Err(runtime_error("guest import does not match the fixed ABI"));
        }
        let import_type = import.ty();
        let Some(ty) = import_type.func() else {
            return Err(runtime_error("clock import has the wrong type"));
        };
        if ty.params().len() != 0
            || !matches!(
                ty.results().collect::<Vec<_>>().as_slice(),
                [wasmtime::ValType::F64]
            )
        {
            return Err(runtime_error("clock import has the wrong signature"));
        }
        drop(imports);
        drop(import);
        Ok(Arc::new(Self { engine, module }))
    }

    pub async fn instantiate(&self, memory_limit: usize) -> Result<Guest, ToolError> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_limit)
            .memories(1)
            .instances(1)
            .tables(1)
            .table_elements(65536)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&self.engine, GuestStore { limits });
        store.limiter(|data| &mut data.limits);
        store.set_fuel(u64::MAX).map_err(runtime_error)?;
        store
            .fuel_async_yield_interval(Some(100_000))
            .map_err(runtime_error)?;
        let mut linker = Linker::new(&self.engine);
        linker
            .func_wrap("env", "__rquickjs_host_now_us", || -> f64 {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1_000_000.0
            })
            .map_err(runtime_error)?;
        let instance: Instance = linker
            .instantiate_async(&mut store, &self.module)
            .await
            .map_err(runtime_error)?;
        let version = instance
            .get_typed_func::<(), u32>(&mut store, "ptc_abi_version")
            .map_err(runtime_error)?;
        if version
            .call_async(&mut store, ())
            .await
            .map_err(runtime_error)?
            != ABI_VERSION
        {
            return Err(runtime_error("guest ABI version mismatch"));
        }
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| runtime_error("guest is missing memory"))?;
        let alloc = instance
            .get_typed_func(&mut store, "ptc_alloc")
            .map_err(runtime_error)?;
        let free = instance
            .get_typed_func(&mut store, "ptc_free")
            .map_err(runtime_error)?;
        let start = instance
            .get_typed_func(&mut store, "ptc_start")
            .map_err(runtime_error)?;
        let resume = instance
            .get_typed_func(&mut store, "ptc_resume")
            .map_err(runtime_error)?;
        Ok(Guest {
            store,
            memory,
            alloc,
            free,
            start,
            resume,
        })
    }
}

impl Guest {
    pub async fn step(
        &mut self,
        input: &[u8],
        first: bool,
        max_output: usize,
    ) -> Result<GuestStep, ToolError> {
        let len =
            u32::try_from(input.len()).map_err(|_| runtime_error("input exceeds the ABI range"))?;
        let ptr = self
            .alloc
            .call_async(&mut self.store, len)
            .await
            .map_err(runtime_error)?;
        self.memory
            .write(&mut self.store, ptr as usize, input)
            .map_err(runtime_error)?;
        let call = if first { &self.start } else { &self.resume };
        let packed = call
            .call_async(&mut self.store, (ptr, len))
            .await
            .map_err(runtime_error)?;
        self.free
            .call_async(&mut self.store, (ptr, len))
            .await
            .map_err(runtime_error)?;
        let output_ptr = u32::try_from(packed & u64::from(u32::MAX))
            .map_err(|_| runtime_error("output pointer exceeds the Wasm32 range"))?;
        let output_len = u32::try_from(packed >> 32)
            .map_err(|_| runtime_error("output length exceeds the Wasm32 range"))?;
        let max = max_output
            .checked_add(4096)
            .ok_or_else(|| runtime_error("output budget overflow"))?;
        if usize::try_from(output_len)
            .map_err(|_| runtime_error("output length exceeds the host range"))?
            > max
        {
            return Err(ToolError::new(
                "E_OUTPUT_LIMIT",
                "guest output exceeds the configured limit",
            ));
        }
        let mut bytes = vec![0; output_len as usize];
        self.memory
            .read(&self.store, output_ptr as usize, &mut bytes)
            .map_err(runtime_error)?;
        self.free
            .call_async(&mut self.store, (output_ptr, output_len))
            .await
            .map_err(runtime_error)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| ToolError::new("E_PTC_PROTOCOL", error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::wire::{HostCompletion, ScriptToolResult, StartRequest};
    use super::*;
    #[tokio::test]
    async fn ptc_real_guest_executes_arithmetic() {
        let runtime = PtcRuntime::compile().expect("编译固定 guest");
        let mut guest = runtime
            .instantiate(128 * 1024 * 1024)
            .await
            .expect("创建 guest");
        let start = serde_json::to_vec(&StartRequest {
            code: "return {sum: [1,2,3].reduce((a,b)=>a+b,0)};".into(),
            memory_limit_bytes: 64 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_pending_calls: 256,
        })
        .unwrap();
        let step = guest
            .step(&start, true, 4 * 1024 * 1024)
            .await
            .expect("执行脚本");
        assert!(
            matches!(step.state, super::super::wire::GuestState::Done { value } if value["sum"] == 6)
        );
    }
    #[tokio::test]
    async fn ptc_guest_round_trips_structured_tool_result() {
        let runtime = PtcRuntime::compile().expect("编译固定 guest");
        let mut guest = runtime
            .instantiate(128 * 1024 * 1024)
            .await
            .expect("创建 guest");
        let start = serde_json::to_vec(&StartRequest {
            code: "const r = await tools.example({q: 1}); return r.structuredContent.items[0].id;"
                .into(),
            memory_limit_bytes: 64 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_pending_calls: 256,
        })
        .expect("启动参数可序列化");
        let step = guest
            .step(&start, true, 4 * 1024 * 1024)
            .await
            .expect("提交工具请求");
        assert_eq!(step.requests.len(), 1);
        let completion = HostCompletion {
            id: step.requests[0].id,
            result: Ok(serde_json::to_value(ScriptToolResult {
                content: "查询完成".into(),
                summary: None,
                structured_content: Some(serde_json::json!({"items":[{"id": 7}]})),
                is_error: false,
                status: "completed".into(),
                artifact: None,
            })
            .expect("结果可序列化")),
        };
        let resume = serde_json::to_vec(&[completion]).expect("完成项可序列化");
        let step = guest
            .step(&resume, false, 4 * 1024 * 1024)
            .await
            .expect("恢复脚本");
        assert!(matches!(step.state, super::super::wire::GuestState::Done { value } if value == 7));
    }
}
