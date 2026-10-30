use std::time::{Duration, Instant};
use wasmi::{Config, Engine, Error, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

pub(super) const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;

const MAX_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_TABLE_ELEMENTS: usize = 16_384;
const MAX_FUEL: u64 = 10_000_000;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(5);

struct StoreState {
    limits: StoreLimits,
    deadline: Instant,
}

/// 在没有宿主能力的前提下执行插件入口。
///
/// WASM 模块只能访问自身线性内存；文件、网络、进程和环境变量均不暴露。
pub(super) async fn execute(module_bytes: Vec<u8>, request: Vec<u8>) -> Result<Vec<u8>, String> {
    if module_bytes.len() > MAX_MODULE_BYTES {
        return Err("插件 WASM 模块不能超过 16 MiB".to_string());
    }
    if request.len() > MAX_REQUEST_BYTES {
        return Err("插件请求不能超过 2 MiB".to_string());
    }

    let task = tokio::task::spawn_blocking(move || execute_sync(&module_bytes, &request));
    match tokio::time::timeout(EXECUTION_TIMEOUT + Duration::from_secs(1), task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(format!("插件执行任务异常终止：{error}")),
        Err(_) => Err("插件执行超过 5 秒限制".to_string()),
    }
}

fn execute_sync(module_bytes: &[u8], request: &[u8]) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + EXECUTION_TIMEOUT;
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);

    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY_BYTES)
        .table_elements(MAX_TABLE_ELEMENTS)
        .instances(1)
        .memories(1)
        .tables(1)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(&engine, StoreState { limits, deadline });
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(MAX_FUEL)
        .map_err(|error| format!("无法初始化插件燃料限制：{error}"))?;
    store.call_hook(|state, _| {
        if Instant::now() >= state.deadline {
            Err(Error::new("插件执行超时"))
        } else {
            Ok(())
        }
    });

    let module = Module::new(&engine, module_bytes)
        .map_err(|error| format!("插件 WASM 模块无效：{error}"))?;
    if module.imports().next().is_some() {
        return Err("插件 WASM 不允许导入任何宿主函数或资源".to_string());
    }

    let linker = Linker::new(&engine);
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|error| format!("无法实例化插件 WASM：{error}"))?
        .start(&mut store)
        .map_err(|error| format!("插件 WASM 启动失败：{error}"))?;
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or_else(|| "插件 WASM 必须导出 memory".to_string())?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, "alloc")
        .map_err(|error| format!("插件 WASM 缺少 alloc(len: i32) -> i32：{error}"))?;
    let route = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&store, "deeptutor_route")
        .map_err(|error| {
            format!(
                "插件 WASM 缺少 deeptutor_route(req_ptr, req_len, out_ptr, out_capacity) -> i32：{error}"
            )
        })?;

    let request_len =
        i32::try_from(request.len()).map_err(|_| "插件请求长度超出 WASM 地址空间".to_string())?;
    let request_ptr = alloc
        .call(&mut store, request_len)
        .map_err(|error| format!("插件无法分配请求缓冲区：{error}"))?;
    write_guest_memory(&memory, &mut store, request_ptr, request, "请求缓冲区")?;

    let output_capacity =
        i32::try_from(MAX_RESPONSE_BYTES).map_err(|_| "插件响应缓冲区上限无效".to_string())?;
    let output_ptr = alloc
        .call(&mut store, output_capacity)
        .map_err(|error| format!("插件无法分配响应缓冲区：{error}"))?;
    ensure_guest_range(
        &memory,
        &store,
        output_ptr,
        MAX_RESPONSE_BYTES,
        "响应缓冲区",
    )?;

    let response_len = route
        .call(
            &mut store,
            (request_ptr, request_len, output_ptr, output_capacity),
        )
        .map_err(|error| format!("插件 WASM 执行失败：{error}"))?;
    if response_len < 0 {
        return Err(format!("插件 WASM 返回错误码 {response_len}"));
    }
    let response_len =
        usize::try_from(response_len).map_err(|_| "插件 WASM 响应长度无效".to_string())?;
    if response_len > MAX_RESPONSE_BYTES {
        return Err("插件 WASM 响应超过 4 MiB 限制".to_string());
    }
    let data = memory.data(&store);
    let start = usize::try_from(output_ptr).map_err(|_| "插件 WASM 响应指针无效".to_string())?;
    let end = start
        .checked_add(response_len)
        .ok_or_else(|| "插件 WASM 响应范围溢出".to_string())?;
    data.get(start..end)
        .map(|bytes| bytes.to_vec())
        .ok_or_else(|| "插件 WASM 响应超出线性内存".to_string())
}

fn write_guest_memory(
    memory: &wasmi::Memory,
    store: &mut Store<StoreState>,
    pointer: i32,
    bytes: &[u8],
    label: &str,
) -> Result<(), String> {
    ensure_guest_range(memory, store, pointer, bytes.len(), label)?;
    let start = usize::try_from(pointer).map_err(|_| format!("{label}指针无效"))?;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| format!("{label}范围溢出"))?;
    memory
        .data_mut(store)
        .get_mut(start..end)
        .ok_or_else(|| format!("{label}超出线性内存"))?
        .copy_from_slice(bytes);
    Ok(())
}

fn ensure_guest_range(
    memory: &wasmi::Memory,
    store: &Store<StoreState>,
    pointer: i32,
    length: usize,
    label: &str,
) -> Result<(), String> {
    let start = usize::try_from(pointer).map_err(|_| format!("{label}指针不能为负数"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| format!("{label}范围溢出"))?;
    if end > memory.data(store).len() {
        return Err(format!("{label}超出插件线性内存"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(function_body: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"
            (module
                (memory (export "memory") 80)
                (global $heap (mut i32) (i32.const 1024))
                (func (export "alloc") (param $len i32) (result i32)
                    (local $pointer i32)
                    (local.set $pointer (global.get $heap))
                    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
                    (local.get $pointer)
                )
                (data (i32.const 128) "ok")
                (func (export "deeptutor_route")
                    (param $request_pointer i32)
                    (param $request_length i32)
                    (param $output_pointer i32)
                    (param $output_capacity i32)
                    (result i32)
                    {function_body}
                )
            )
            "#
        ))
        .expect("测试 WAT 有效")
    }

    #[tokio::test]
    async fn executes_module_without_host_imports() {
        let wasm = module(
            r#"
            (memory.copy (local.get $output_pointer) (i32.const 128) (i32.const 2))
            (i32.const 2)
            "#,
        );
        let response = execute(wasm, b"{}".to_vec()).await.expect("插件应执行成功");
        assert_eq!(response, b"ok");
    }

    #[tokio::test]
    async fn rejects_module_with_host_imports() {
        let source = r#"
            (module
                (import "env" "read_file" (func $read_file))
                (memory (export "memory") 1)
                (func (export "alloc") (param i32) (result i32) (i32.const 1024))
                (func (export "deeptutor_route")
                    (param i32) (param i32) (param i32) (param i32)
                    (result i32)
                    (i32.const 0)
                )
            )
        "#;
        let error = execute(
            wat::parse_str(source).expect("测试 WAT 有效"),
            b"{}".to_vec(),
        )
        .await
        .expect_err("带宿主导入的插件必须被拒绝");
        assert!(error.contains("不允许导入"));
    }
}
