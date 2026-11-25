use super::*;
use crate::Transport;
use crate::client::ClientHandlerBridge;
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolResponse, CallToolResult, ContentBlock, ElicitRequest, ElicitRequestParams,
    ElicitationSchema, InputRequest, InputRequests, InputRequiredResult, ListToolsResult,
    ServerCapabilities, ServerConfig as RmcpServerInfo, ServerNotification, Tool, ToolsCapability,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::json;
use std::sync::atomic::AtomicUsize;

const CONNECT: u64 = 5;

/// MRTR 测试里要逐字节回显的 state（含各种需要原样存活的字符）。
const TRICKY_STATE: &str = "state {占} ~:/?#[]@!$&'()*+,;= 终";

struct TestState {
    /// 0：基础工具集；≥1：追加 `extra` 工具（供 list_changed 用例）。
    generation: AtomicUsize,
    /// 触发服务端退出（传输关闭，供重连用例）。
    kill: tokio::sync::Notify,
    connect_count: AtomicUsize,
    /// 一次 `tools/list` 已应答（供「握手成功随即退出」用例同步）。
    listed: tokio::sync::Notify,
    /// `slow` 工具的在飞数与观测到的峰值（供每 server 并发上限用例）。
    inflight: AtomicUsize,
    max_inflight: AtomicUsize,
    /// `loop` 工具收到的 `tools/call` 轮数（供 MRTR 上限用例）。
    rounds: AtomicUsize,
    started: tokio::sync::Notify,
    cancelled: tokio::sync::Notify,
    effects: AtomicUsize,
    /// `slow` 工具的放行闸：false 时在工具内排队，send(true) 全部放行。
    slow_open: tokio::sync::watch::Sender<bool>,
    /// `slow` 工具当前在飞数（watch 广播，测试事件同步，不丢通知）。
    slow_inflight: tokio::sync::watch::Sender<usize>,
}

impl Default for TestState {
    fn default() -> Self {
        Self {
            generation: AtomicUsize::new(0),
            kill: tokio::sync::Notify::new(),
            connect_count: AtomicUsize::new(0),
            listed: tokio::sync::Notify::new(),
            inflight: AtomicUsize::new(0),
            max_inflight: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            cancelled: tokio::sync::Notify::new(),
            effects: AtomicUsize::new(0),
            slow_open: tokio::sync::watch::channel(false).0,
            slow_inflight: tokio::sync::watch::channel(0usize).0,
        }
    }
}

struct TestServer {
    state: Arc<TestState>,
}

fn schema(properties: serde_json::Value) -> Arc<rmcp::model::JsonObject> {
    Arc::new(
        json!({"type": "object", "properties": properties, "required": []})
            .as_object()
            .cloned()
            .unwrap(),
    )
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> RmcpServerInfo {
        let mut capabilities = ServerCapabilities::default();
        let mut tools = ToolsCapability::default();
        tools.list_changed = Some(true);
        capabilities.tools = Some(tools);
        RmcpServerInfo::new(capabilities).with_instructions("Use echo to inspect values.")
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
        self.state.listed.notify_one();
        let mut tools = vec![
            Tool::new(
                "echo",
                "回显文本",
                schema(json!({"text": {"type": "string"}})),
            ),
            Tool::new("refresh", "触发 list_changed", schema(json!({}))),
            Tool::new("die", "让服务端退出", schema(json!({}))),
            Tool::new("slow", "延迟应答，用于观测并发", schema(json!({}))),
            Tool::new("elicit", "先要一次用户名再应答", schema(json!({}))),
            Tool::new("stately", "只靠 requestState 续跑一轮", schema(json!({}))),
            Tool::new("loop", "永远 input_required", schema(json!({}))),
        ];
        if self.state.generation.load(Ordering::SeqCst) >= 1 {
            tools.push(Tool::new("extra", "刷新后出现", schema(json!({}))));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::model::ErrorData> {
        match request.name.as_ref() {
            "echo" => {
                let text = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Ok(
                    CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))])
                        .into(),
                )
            }
            "refresh" => {
                self.state.generation.store(1, Ordering::SeqCst);
                let _ = context
                    .peer
                    .send_notification(ServerNotification::ToolListChangedNotification(
                        rmcp::model::ToolListChangedNotification {
                            method: Default::default(),
                            extensions: Default::default(),
                        },
                    ))
                    .await;
                Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
            }
            "die" => {
                self.state.kill.notify_waiters();
                Ok(CallToolResult::success(vec![ContentBlock::text("bye")]).into())
            }
            "slow" => {
                let inflight = self.state.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                self.state
                    .max_inflight
                    .fetch_max(inflight, Ordering::SeqCst);
                self.state.slow_inflight.send_replace(inflight);
                // 门控放行：测试显式决定并发窗口，无需真实延迟。
                let mut open = self.state.slow_open.subscribe();
                while !*open.borrow_and_update() {
                    if open.changed().await.is_err() {
                        break;
                    }
                }
                self.state.inflight.fetch_sub(1, Ordering::SeqCst);
                Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
            }
            "elicit" => match request.request_state.as_deref() {
                None => {
                    let schema: ElicitationSchema = serde_json::from_value(json!({
                        "type": "object",
                        "properties": {"name": {"type": "string"}},
                        "required": ["name"],
                    }))
                    .expect("表单 schema 可解析");
                    let mut requests = InputRequests::new();
                    requests.insert(
                        "who".into(),
                        InputRequest::Elicitation(ElicitRequest::new(
                            ElicitRequestParams::FormElicitationParams {
                                meta: None,
                                message: "Please provide your name".into(),
                                requested_schema: schema,
                            },
                        )),
                    );
                    Ok(InputRequiredResult::new(Some(requests), Some(TRICKY_STATE.into())).into())
                }
                Some(state) => {
                    assert_eq!(state, TRICKY_STATE, "requestState 必须逐字节回显");
                    let name = request
                        .input_responses
                        .as_ref()
                        .and_then(|responses| responses.get("who"))
                        .and_then(|response| response["content"]["name"].as_str())
                        .unwrap_or("<missing>");
                    Ok(
                        CallToolResult::success(vec![ContentBlock::text(format!("hello {name}"))])
                            .into(),
                    )
                }
            },
            "stately" => match request.request_state.as_deref() {
                None => {
                    assert!(request.input_responses.is_none());
                    Ok(InputRequiredResult::from_request_state("s-1").into())
                }
                Some("s-1") => Ok(CallToolResult::success(vec![ContentBlock::text("done")]).into()),
                Some(other) => Err(rmcp::model::ErrorData::invalid_params(
                    format!("unexpected state {other}"),
                    None,
                )),
            },
            "loop" => {
                self.state.rounds.fetch_add(1, Ordering::SeqCst);
                Ok(InputRequiredResult::from_request_state("again").into())
            }
            "lost" => {
                self.state.effects.fetch_add(1, Ordering::SeqCst);
                self.state.kill.notify_one();
                std::future::pending().await
            }
            "blocked" => {
                let state = Arc::clone(&self.state);
                tokio::spawn(async move {
                    context.ct.cancelled().await;
                    state.cancelled.notify_one();
                });
                self.state.started.notify_one();
                std::future::pending().await
            }
            other => Err(rmcp::model::ErrorData::invalid_params(
                format!("未知工具 {other}"),
                None,
            )),
        }
    }
}

fn test_connector(state: Arc<TestState>) -> Connector {
    Arc::new(move |tool_list_changed| {
        let state = state.clone();
        Box::pin(async move {
            state.connect_count.fetch_add(1, Ordering::SeqCst);
            let (client_io, mut client_relay) = tokio::io::duplex(1 << 16);
            let (server_io, mut server_relay) = tokio::io::duplex(1 << 16);
            let kill_state = Arc::clone(&state);
            tokio::spawn(async move {
                // kill 模拟进程崩溃：立即断开传输，不走 rmcp 的优雅关闭排空。
                tokio::select! {
                    _ = kill_state.kill.notified() => {}
                    _ = tokio::io::copy_bidirectional(&mut client_relay, &mut server_relay) => {}
                }
            });
            let serve = TestServer {
                state: state.clone(),
            }
            .serve(server_io);
            tokio::spawn(async move {
                match serve.await {
                    // 保持服务句柄直到对端关闭；断线由上面的传输代理控制。
                    Ok(service) => {
                        let _ = service.waiting().await;
                    }
                    Err(error) => eprintln!("[test-server] serve 失败：{error}"),
                }
            });
            crate::client::serve_bridge(
                "test",
                ClientHandlerBridge::new(tool_list_changed),
                client_io,
            )
            .await
        })
    })
}

/// 「握手成功但随即退出」的连接器：先让客户端完成列出工具，再关掉传输。
///
/// 这正是重连预算必须能耗尽的场景：若握手成功就清零预算，监督任务会以最小退避
/// 无限重启子进程，`Phase::Failed` 永不出现。
fn flapping_connector(state: Arc<TestState>) -> Connector {
    Arc::new(move |tool_list_changed| {
        let state = Arc::clone(&state);
        Box::pin(async move {
            state.connect_count.fetch_add(1, Ordering::SeqCst);
            let (client_io, server_io) = tokio::io::duplex(1 << 16);
            let server_state = Arc::clone(&state);
            tokio::spawn(async move {
                let serving = TestServer {
                    state: Arc::clone(&server_state),
                }
                .serve(server_io);
                if let Ok(service) = serving.await {
                    // 等本次 tools/list 应答完再退出，保证走的是「握手成功」路径
                    server_state.listed.notified().await;
                    drop(service);
                }
            });
            crate::client::serve_bridge(
                "test",
                ClientHandlerBridge::new(tool_list_changed),
                client_io,
            )
            .await
        })
    })
}

/// 小参数策略：毫秒级退避与轮询，让预算耗尽路径能在测试里跑完。
fn fast_policy(max_attempts: u32, stable_runtime: Duration) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        base_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
        stable_runtime,
        close_poll: Duration::from_millis(10),
    }
}

fn test_server(name: &str, enabled: bool) -> ServerConfig {
    ServerConfig {
        name: name.to_owned(),
        transport: Transport::Stdio {
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
        },
        enabled,
        tools: None,
        connect_timeout_secs: CONNECT,
        call_timeout_secs: CONNECT,
        max_concurrent_calls: 1,
    }
}

async fn connected_tools(manager: &McpManager, timeout: Duration) -> Vec<Arc<McpToolInfo>> {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = manager.snapshot().await;
        if !snapshot.tools.is_empty() {
            return snapshot.tools;
        }
        if Instant::now() >= deadline {
            panic!("server 未在时限内连接并产出工具：{:?}", snapshot.statuses);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 等 `slow` 工具的在飞数达到 `expected`：watch 事件同步，不睡。
async fn slow_inflight(state: &TestState, expected: usize) {
    let mut rx = state.slow_inflight.subscribe();
    while *rx.borrow_and_update() < expected {
        rx.changed().await.expect("slow 通道存活");
    }
}

#[tokio::test]
async fn connects_lists_calls_and_reports_images() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    manager.wait_ready(&CancellationToken::new()).await.unwrap();
    let snapshot = manager.snapshot().await;
    assert_eq!(
        snapshot.instructions["test"].as_ref(),
        "Use echo to inspect values."
    );
    let tools = snapshot.tools;
    assert_eq!(tools.len(), 7);
    let echo = tools.iter().find(|t| t.name == "echo").unwrap();
    assert_eq!(echo.qualified, "mcp__test__echo");
    assert!(!echo.read_only);

    let output = manager
        .call(
            echo,
            json!({"text": "你好"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(output.text, "echo: 你好");

    let snapshot = manager.snapshot().await;
    assert!(matches!(
        snapshot.statuses.get("test"),
        Some(ServerStatus::Connected { tool_count: 7 })
    ));
    manager.shutdown().await;
}

#[tokio::test]
async fn list_changed_notification_refreshes_tools() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    assert_eq!(tools.len(), 7);

    let before_version = manager.status_version();
    let refresh = tools.iter().find(|t| t.name == "refresh").unwrap();
    manager
        .call(refresh, json!({}), CancellationToken::new(), None)
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = manager.snapshot().await.tools.len();
        if count == 8 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "list_changed 后工具数未更新：{count}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        manager.status_version() > before_version,
        "工具列表更新后应递增状态版本号以通知目录同步"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn transport_death_triggers_reconnect() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build_with_policy(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
        fast_policy(2, Duration::from_secs(30)),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;

    let die = tools.iter().find(|t| t.name == "die").unwrap();
    let result = manager
        .call(die, json!({}), CancellationToken::new(), None)
        .await;
    assert!(result.is_ok() || matches!(result, Err(McpError::OutcomeUnknown(_))));

    // 服务端退出 → 传输关闭 → 重连；期间调用应报连接错误而非 panic。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = state.connect_count.load(Ordering::SeqCst);
        if count >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "未发生重连：{count} 次连接");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = connected_tools(&manager, Duration::from_secs(5)).await;
    manager.shutdown().await;
}

#[tokio::test]
async fn response_loss_never_replays_effects() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build_with_policy(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(Arc::clone(&state)),
        fast_policy(2, Duration::from_secs(30)),
    );
    manager.start().await;
    manager.wait_ready(&CancellationToken::new()).await.unwrap();
    let mut info = (*manager.snapshot().await.tools[0]).clone();
    info.name = "lost".into();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        manager.call(&info, json!({}), CancellationToken::new(), None),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(McpError::OutcomeUnknown(_))));
    assert_eq!(state.effects.load(Ordering::SeqCst), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn timeout_cancellation_and_dropped_future_notify_server() {
    // 真实超时路径：时限缩到 50ms，错误上报的仍是配置原值 Timeout(1)。
    for path in 0..3 {
        let state = Arc::new(TestState::default());
        let mut config = test_server("test", true);
        config.call_timeout_secs = 1;
        let manager = McpManager::build_with_call_timeout(
            vec![config],
            CancellationToken::new(),
            |_| test_connector(Arc::clone(&state)),
            Duration::from_millis(50),
        );
        manager.start().await;
        manager.wait_ready(&CancellationToken::new()).await.unwrap();
        let mut info = (*manager.snapshot().await.tools[0]).clone();
        info.name = "blocked".into();
        let cancel = CancellationToken::new();
        let call = tokio::spawn({
            let manager = Arc::clone(&manager);
            let cancel = cancel.clone();
            async move { manager.call(&info, json!({}), cancel, None).await }
        });
        tokio::time::timeout(Duration::from_secs(2), state.started.notified())
            .await
            .unwrap();
        match path {
            0 => assert!(matches!(call.await.unwrap(), Err(McpError::Timeout(1)))),
            1 => {
                cancel.cancel();
                assert!(matches!(call.await.unwrap(), Err(McpError::Cancelled)));
            }
            _ => {
                call.abort();
                let _ = call.await;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), state.cancelled.notified())
            .await
            .expect("服务端必须收到取消");
        manager.shutdown().await;
    }
}

#[test]
fn duplicate_wire_names_are_rejected_before_catalog_construction() {
    let tool = Tool::new("a.b", "first", schema(json!({})));
    assert!(matches!(
        convert_tools("test", &[tool.clone(), tool], None),
        Err(McpError::Tool(_))
    ));
    let tools = [
        Tool::new("a.b", "dot", schema(json!({}))),
        Tool::new("a_b", "underscore", schema(json!({}))),
    ];
    let converted = convert_tools("test", &tools, None).unwrap();
    assert_ne!(converted[0].qualified, converted[1].qualified);
    assert_eq!(converted[0].name, "a.b");
    assert_eq!(converted[1].name, "a_b");
}

#[tokio::test]
async fn disabled_server_never_connects_and_reports_status() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("off", false)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    // disabled 不应发起连接：让出若干轮，潜在线程若有误启动会在
    // connector 首步就把 connect_count 自增，断言即可捕获。
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let snapshot = manager.snapshot().await;
    assert_eq!(snapshot.tools.len(), 0);
    assert!(matches!(
        snapshot.statuses.get("off"),
        Some(ServerStatus::Disabled)
    ));
    assert_eq!(state.connect_count.load(Ordering::SeqCst), 0);
    manager.shutdown().await;
}

#[tokio::test]
async fn tool_deny_filter_hides_matching_tools() {
    let state = Arc::new(TestState::default());
    let mut config = test_server("test", true);
    config.tools = crate::config::ToolFilter {
        allow: Vec::new(),
        deny: vec![
            "refresh".into(),
            "die".into(),
            "slow".into(),
            "elicit".into(),
            "stately".into(),
            "loop".into(),
        ],
    }
    .compile()
    .unwrap();
    let manager = McpManager::build(vec![config], CancellationToken::new(), |_| {
        test_connector(state.clone())
    });
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    manager.shutdown().await;
}

/// 并发发起的调用须由 slot 信号量串行化：默认上限 1，服务端观测不到交错。
#[tokio::test]
async fn same_server_calls_serialize_by_default() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let slow = Arc::clone(tools.iter().find(|t| t.name == "slow").unwrap());
    let mut calls = Vec::new();
    for _ in 0..3 {
        let manager = Arc::clone(&manager);
        let info = Arc::clone(&slow);
        calls.push(tokio::spawn(async move {
            manager
                .call(&info, json!({}), CancellationToken::new(), None)
                .await
        }));
    }
    // 第一个 slow 拿到 slot 进入工具，其余仍在信号量外排队。
    slow_inflight(&state, 1).await;
    state.slow_open.send_replace(true);
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert_eq!(
        state.max_inflight.load(Ordering::SeqCst),
        1,
        "默认并发上限为 1，同 server 调用不得交错"
    );
    manager.shutdown().await;
}

/// 调高 `max_concurrent_calls` 后，同 server 并发可达且不超过配置上限。
#[tokio::test]
async fn raised_limit_allows_same_server_concurrency() {
    let state = Arc::new(TestState::default());
    let mut config = test_server("test", true);
    config.max_concurrent_calls = 2;
    let manager = McpManager::build(vec![config], CancellationToken::new(), |_| {
        test_connector(state.clone())
    });
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let slow = Arc::clone(tools.iter().find(|t| t.name == "slow").unwrap());
    let mut calls = Vec::new();
    for _ in 0..4 {
        let manager = Arc::clone(&manager);
        let info = Arc::clone(&slow);
        calls.push(tokio::spawn(async move {
            manager
                .call(&info, json!({}), CancellationToken::new(), None)
                .await
        }));
    }
    // 两个 slow 并发进入工具，说明上限真的放到 2；放行后其余依次通过。
    slow_inflight(&state, 2).await;
    state.slow_open.send_replace(true);
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert_eq!(
        state.max_inflight.load(Ordering::SeqCst),
        2,
        "并发应达到且不超过配置的上限"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn call_before_connect_fails_gracefully() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build_with_call_timeout(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
        Duration::from_millis(50),
    );
    // 未 start：slot 保持 Connecting，调用应得到结构化错误。
    let info = McpToolInfo {
        server: "test".into(),
        name: "echo".into(),
        qualified: "mcp__test__echo".into(),
        title: None,
        description: None,
        input_schema: json!({"type": "object"}),
        output_schema: None,
        read_only: false,
    };
    let result = manager
        .call(&info, json!({}), CancellationToken::new(), None)
        .await;
    assert!(matches!(result, Err(McpError::Transport(_))));
    manager.shutdown().await;
}

// 杀死旧传输并使用新连接的 peer，验证调用确实走了重连后的连接。
#[tokio::test]
async fn call_waits_for_reconnecting_slot_to_become_connected() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build_with_call_timeout(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
        Duration::from_secs(2),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let echo = Arc::clone(tools.iter().find(|tool| tool.name == "echo").unwrap());
    let slot = manager.slots.get("test").unwrap();
    let (tools, instructions) = match &*slot.phase.read() {
        Phase::Connected {
            tools,
            instructions,
            ..
        } => (tools.clone(), instructions.clone()),
        _ => panic!("测试 server 应已连接"),
    };
    slot.set_phase(Phase::Reconnecting {
        attempts: 1,
        error: "test disconnect".into(),
    });

    let call_manager = Arc::clone(&manager);
    let call = tokio::spawn(async move {
        call_manager
            .call(
                &echo,
                json!({"text": "after reconnect"}),
                CancellationToken::new(),
                None,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!call.is_finished(), "重连期间的调用应等待 phase 稳定");

    state.kill.notify_waiters();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (changed_tx, _changed_rx) = tokio::sync::mpsc::channel::<()>(1);
    let service2 = (slot.connector)(changed_tx)
        .await
        .expect("第二次连接应成功");
    slot.set_phase(Phase::Connected {
        peer: service2.peer().clone(),
        tools,
        instructions,
    });
    let output = call.await.unwrap().unwrap();
    assert_eq!(output.text, "echo: after reconnect");
    assert!(
        state.connect_count.load(Ordering::SeqCst) >= 2,
        "应至少建立两次连接"
    );
    manager.shutdown().await;
}

/// 握手成功但立即退出的 server 必须耗尽重连预算，而不是无限重启。
#[tokio::test]
async fn handshake_then_immediate_exit_exhausts_reconnect_budget() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build_with_policy(
        vec![test_server("flap", true)],
        CancellationToken::new(),
        |_| flapping_connector(Arc::clone(&state)),
        fast_policy(1, Duration::from_secs(30)),
    );
    manager.start().await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = manager.snapshot().await;
        if matches!(
            snapshot.statuses.get("flap"),
            Some(ServerStatus::Failed { .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "重连预算未耗尽，仍在重启：{:?}（已连接 {} 次）",
            snapshot.statuses,
            state.connect_count.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let connects = state.connect_count.load(Ordering::SeqCst);
    assert!(connects <= 3, "预算耗尽后不得继续重启：{connects} 次连接");
    manager.shutdown().await;
}

/// 稳定运行满 `stable_runtime` 的连接才清零预算：达标后掉线继续重连，不进入 Failed。
#[tokio::test]
async fn stable_connection_resets_the_reconnect_budget() {
    let state = Arc::new(TestState::default());
    // stable_runtime = 0：每次连接都算「稳定」，预算每轮清零
    let manager = McpManager::build_with_policy(
        vec![test_server("flap", true)],
        CancellationToken::new(),
        |_| flapping_connector(Arc::clone(&state)),
        fast_policy(1, Duration::ZERO),
    );
    manager.start().await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while state.connect_count.load(Ordering::SeqCst) < 3 {
        assert!(Instant::now() < deadline, "稳定期达标后应当继续重连");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !matches!(
            manager.snapshot().await.statuses.get("flap"),
            Some(ServerStatus::Failed { .. })
        ),
        "预算已清零，不该进入 Failed"
    );
    manager.shutdown().await;
}

/// MRTR 全链路：`input_required`（elicitation + requestState）→ fulfiller
/// 作答 → 带 `inputResponses` 与逐字节回显的 state 重试 → 最终结果。
#[tokio::test]
async fn mrtr_elicitation_round_completes_the_call() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let elicit = tools.iter().find(|t| t.name == "elicit").unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fulfiller: InputFulfiller = {
        let seen = seen.clone();
        Arc::new(move |pending| {
            seen.lock().unwrap().push(format!("{pending:?}"));
            Box::pin(async move {
                match pending {
                    crate::mrtr::PendingInput::ElicitForm { message, schema } => {
                        assert_eq!(message, "Please provide your name");
                        assert!(schema["properties"]["name"].is_object());
                        Ok(json!({"action": "accept", "content": {"name": "小明"}}))
                    }
                    other => Err(McpError::Tool(format!("unexpected request: {other:?}"))),
                }
            })
        })
    };

    let output = manager
        .call(
            elicit,
            json!({}),
            CancellationToken::new(),
            Some(&fulfiller),
        )
        .await
        .unwrap();
    assert_eq!(output.text, "hello 小明");
    assert_eq!(seen.lock().unwrap().len(), 1);
    manager.shutdown().await;
}

/// fulfiller 回 decline 也是合法答复：重试照常发出，server 给最终结果。
#[tokio::test]
async fn mrtr_decline_is_a_valid_response() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let elicit = tools.iter().find(|t| t.name == "elicit").unwrap();
    let output = manager
        .call(elicit, json!({}), CancellationToken::new(), None)
        .await
        .unwrap();
    // 默认兜底回 decline：server 收到的 content 缺 name，按 <missing> 收场。
    assert_eq!(output.text, "hello <missing>");
    manager.shutdown().await;
}

/// 只有 `requestState` 的轮次不传 `inputResponses`，直接续跑至最终结果。
#[tokio::test]
async fn mrtr_state_only_round_retries_immediately() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let stately = tools.iter().find(|t| t.name == "stately").unwrap();
    let output = manager
        .call(stately, json!({}), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(output.text, "done");
    manager.shutdown().await;
}

/// server 无限 input_required 时，调用在轮数上限处以明确错误收场。
#[tokio::test]
async fn mrtr_round_cap_fails_instead_of_spinning() {
    let state = Arc::new(TestState::default());
    let manager = McpManager::build(
        vec![test_server("test", true)],
        CancellationToken::new(),
        |_| test_connector(state.clone()),
    );
    manager.start().await;
    let tools = connected_tools(&manager, Duration::from_secs(5)).await;
    let loopy = tools.iter().find(|t| t.name == "loop").unwrap();
    let error = manager
        .call(loopy, json!({}), CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, McpError::Tool(ref message) if message.contains("requesting input")),
        "unexpected error: {error}"
    );
    assert_eq!(
        state.rounds.load(Ordering::SeqCst),
        rmcp::model::DEFAULT_MRTR_MAX_ROUNDS,
        "轮数应正好到上限"
    );
    manager.shutdown().await;
}

/// 已进入 `Failed` 的 server 不会被后续 `start()` 自动复活或再次阻塞；`reconnect()` 重置预算并恢复。
#[tokio::test]
async fn failed_server_does_not_resurrect_on_start_but_recovers_on_reconnect() {
    let fail = Arc::new(AtomicBool::new(true));
    let attempts = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(TestState::default());
    let fail_c = Arc::clone(&fail);
    let attempts_c = Arc::clone(&attempts);
    let state_c = Arc::clone(&state);
    let manager = McpManager::build_with_policy(
        vec![test_server("flaky", true)],
        CancellationToken::new(),
        move |_| {
            let fail = Arc::clone(&fail_c);
            let attempts = Arc::clone(&attempts_c);
            let inner = test_connector(Arc::clone(&state_c));
            Arc::new(move |tool_list_changed| {
                let fail = Arc::clone(&fail);
                let attempts = Arc::clone(&attempts);
                let inner = Arc::clone(&inner);
                Box::pin(async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    if fail.load(Ordering::Acquire) {
                        Err(McpError::Transport(
                            "simulated handshake failure".to_owned(),
                        ))
                    } else {
                        inner(tool_list_changed).await
                    }
                })
            })
        },
        fast_policy(2, Duration::from_secs(30)),
    );
    let mut status_rx = manager.subscribe_status();
    manager.start().await;
    // 等到预算耗尽进入 Failed。
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                manager.snapshot_now().statuses.get("flaky"),
                Some(ServerStatus::Failed { .. })
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("应耗尽重连预算进入 Failed");
    let count_after_fail = attempts.load(Ordering::SeqCst);
    assert!(count_after_fail >= 2);

    // 后续提示再次调用 `start()`：不得复活 Failed server，也不得增加尝试次数。
    manager.start().await;
    manager.wait_ready(&CancellationToken::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        count_after_fail,
        "start() 不得复活已 Failed 的 server"
    );

    // 故障排除后手动 reconnect 应立即恢复到 Connected。
    fail.store(false, Ordering::Release);
    manager.reconnect(Some("flaky")).await.unwrap();
    let tools = connected_tools(&manager, Duration::from_secs(3)).await;
    assert_eq!(tools.len(), 7);
    manager.shutdown().await;
}

/// 宽限期满后 `wait_ready_within` 立刻返回，慢 server 仍在后台完成握手并推送状态版本号。
#[tokio::test]
async fn wait_ready_within_grace_returns_early_and_slow_server_finishes_in_background() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let state = Arc::new(TestState::default());
    let gate_c = Arc::clone(&gate);
    let state_c = Arc::clone(&state);
    let manager = McpManager::build(
        vec![test_server("slow", true)],
        CancellationToken::new(),
        move |_| {
            let gate = Arc::clone(&gate_c);
            let inner = test_connector(Arc::clone(&state_c));
            Arc::new(move |tool_list_changed| {
                let gate = Arc::clone(&gate);
                let inner = Arc::clone(&inner);
                Box::pin(async move {
                    gate.notified().await;
                    inner(tool_list_changed).await
                })
            })
        },
    );
    let mut status_rx = manager.subscribe_status();
    manager.start().await;
    let started = Instant::now();
    manager
        .wait_ready_within(&CancellationToken::new(), Duration::from_millis(50))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "宽限期等待不应长时间阻塞"
    );
    assert!(matches!(
        manager.snapshot_now().statuses.get("slow"),
        Some(ServerStatus::Connecting)
    ));

    // 放行慢握手：后台应继续完成并递增状态版本号。
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                manager.snapshot_now().statuses.get("slow"),
                Some(ServerStatus::Connected { tool_count: 7 })
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("慢 server 应在后台完成连接");
    manager.shutdown().await;
}

/// Unix stdio 进程组隔离：握手超时后不仅杀掉直接子进程，连其派生的孙进程也一并回收，且保留 stderr 诊断尾部。
#[cfg(unix)]
#[tokio::test]
async fn stdio_process_group_kills_grandchild_on_timeout_and_captures_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let script = format!(
        "echo 'booting hanging server' >&2; sleep 30 & echo $! > '{}'; wait",
        pid_file.display()
    );
    let server = ServerConfig {
        name: "hang".into(),
        transport: Transport::Stdio {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            env: BTreeMap::new(),
            cwd: None,
        },
        enabled: true,
        connect_timeout_secs: 1,
        call_timeout_secs: 5,
        max_concurrent_calls: 1,
        tools: None,
    };
    let manager = McpManager::new(vec![server], CancellationToken::new());
    let mut status_rx = manager.subscribe_status();
    manager.start().await;

    // 等到首次 1s 连接超时进入 Reconnecting。
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let states = manager.server_states();
            if matches!(
                states.first().map(|s| &s.status),
                Some(ServerStatus::Reconnecting { .. } | ServerStatus::Failed { .. })
            ) {
                break;
            }
            status_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("挂起 stdio server 应在 connect_timeout_secs 后超时");

    let states = manager.server_states();
    assert!(
        states[0]
            .recent_stderr
            .iter()
            .any(|line| line.contains("booting hanging server")),
        "应捕获子进程 stderr 尾部：{:?}",
        states[0].recent_stderr
    );
    let grandchild_pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("应已写入孙进程 PID")
        .trim()
        .parse()
        .expect("有效 PID");
    manager.shutdown().await;

    // 给内核一点回收时间后验证孙进程已不存在（ESRCH）。
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let rc = unsafe { libc::kill(grandchild_pid, 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "孙进程 {grandchild_pid} 未被进程组清理"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
