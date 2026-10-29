//! 会话流测试：用 crate 客户端跑完整调用链（握手 → prompt → 锁 → 生命周期）。

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use moc_protocol::methods::{
    AskAnswer, AskResult, ClientCapabilities, ConfigGet, ConfigGetParams, PlanAction,
    ServerCapabilities, SessionCancel, SessionCancelParams, SessionCompact, SessionCompactParams,
    SessionDelete, SessionDeleteParams, SessionList, SessionListParams, SessionLoad,
    SessionLoadParams, SessionMode, SessionPlanApprove, SessionPlanApproveParams, SessionPrompt,
    SessionRename, SessionRenameParams, SessionRequestAsk, SessionRequestPermission, SessionRetry,
    SessionRetryParams, SessionSetMode, SessionSetModeParams, SessionUnload, SessionUnloadParams,
    SessionUnlock, SessionUnlockParams, SessionUpdate, SessionUpdateMethod, ToolStatus,
};
use moc_protocol::{
    ClientBuilder, FinishReason, PROTOCOL_VERSION, ProtocolError, RpcCallError, RpcErrorCode,
};
use support::{
    PromptMode, ScriptedService, UpdateLog, client_info, local_client, permission_handler,
    prompt_params, update_handler, wait_until,
};
use tokio::sync::Notify;

/// 带更新日志与权限处理器的标准前端构造器。
fn builder(log: UpdateLog, option_id: &'static str) -> ClientBuilder {
    ClientBuilder::new(client_info())
        .capabilities(ClientCapabilities {
            permission_requests: true,
            ask_requests: true,
        })
        .on_notification::<SessionUpdateMethod, _, _>(update_handler(log))
        .on_request::<SessionRequestPermission, _, _>(permission_handler(option_id))
}

#[tokio::test]
async fn handshake_and_config() {
    let service = ScriptedService::new();
    let (_server, _connection, handle, handshake) =
        local_client(service.clone(), builder(UpdateLog::default(), "allow_once")).await;

    assert_eq!(handshake.protocol_version, PROTOCOL_VERSION);
    assert_eq!(handshake.server.name, "scripted");
    assert_eq!(
        handle.server().capabilities,
        ServerCapabilities {
            permission_requests: true,
            session_locking: true,
            ask_requests: true,
        }
    );
    assert!(
        service
            .state()
            .initialize
            .as_ref()
            .expect("收到握手参数")
            .capabilities
            .permission_requests
    );

    let config = handle
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .expect("config/get 成功");
    assert_eq!(config.models.len(), 2);
    assert_eq!(config.default_model.as_deref(), Some("deepseek-flash"));
    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.default_agent.as_deref(), Some("build"));
    assert_eq!(config.cwd, "/home/soeur/project/moc");

    let list = handle
        .call::<SessionList>(SessionListParams {})
        .await
        .expect("session/list 成功");
    let ids: Vec<&str> = list
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect();
    assert_eq!(ids, ["ses_1", "ses_2", "ses_3"], "按 updated_at_ms 降序");
    assert!(list.sessions.iter().all(|session| session.lock.is_none()));
}

#[tokio::test]
async fn new_session_methods_are_dispatched_to_core() {
    let service = ScriptedService::new();
    let (_server, _connection, handle, _) =
        local_client(service, builder(UpdateLog::default(), "allow_once")).await;

    let error = handle
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: "ses_1".to_owned(),
            mode: SessionMode::Ptc,
        })
        .await
        .expect_err("脚本 core 不实现模式切换");
    assert_eq!(error.code(), Some(RpcErrorCode::NotSupported.code()));

    let error = handle
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: "ses_1".to_owned(),
            plan_id: "plan_12345678".to_owned(),
            action: PlanAction::Execute,
            execution_model: None,
            execution_mode: None,
        })
        .await
        .expect_err("脚本 core 不实现计划批准");
    assert_eq!(error.code(), Some(RpcErrorCode::NotSupported.code()));

    let error = handle
        .call::<SessionCompact>(SessionCompactParams {
            session_id: "ses_1".to_owned(),
            instructions: None,
        })
        .await
        .expect_err("脚本 core 不实现压缩");
    assert_eq!(error.code(), Some(RpcErrorCode::NotSupported.code()));
}

#[tokio::test]
async fn request_ask_round_trips_through_frontend_handler() {
    let service = ScriptedService::new();
    service.prompt_mode(PromptMode::Ask);
    let observed = Arc::new(std::sync::Mutex::new(None));
    let observed_by_handler = Arc::clone(&observed);
    let frontend = builder(UpdateLog::default(), "allow_once")
        .on_request::<SessionRequestAsk, _, _>(move |params, _ctx| {
            let observed = Arc::clone(&observed_by_handler);
            async move {
                *observed.lock().expect("ask 请求锁可用") = Some(params);
                Ok(AskResult {
                    answers: vec![AskAnswer {
                        id: "choice".to_owned(),
                        selected: vec!["Rust".to_owned()],
                        custom: None,
                    }],
                })
            }
        });
    let (_server, _connection, handle, _) = local_client(service.clone(), frontend).await;

    handle
        .call::<SessionPrompt>(prompt_params("ses_1", "选择实现语言"))
        .await
        .expect("提示往返与成功");

    let request = observed
        .lock()
        .expect("ask 请求锁可用")
        .clone()
        .expect("前端收到 ask 请求");
    assert_eq!(request.session_id, "ses_1");
    assert_eq!(request.prompt_id, "prompt_1");
    assert_eq!(request.questions[0].id, "choice");
    assert_eq!(request.questions[0].recommended, [0]);
    assert_eq!(service.state().ask_results[0].answers[0].selected, ["Rust"]);
}

#[tokio::test]
async fn prompt_streams_updates_and_matches_result() {
    let service = ScriptedService::new();
    let log = UpdateLog::default();
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(log.clone(), "allow_once")).await;

    let result = handle
        .call::<SessionPrompt>(prompt_params("ses_1", "看一下 README"))
        .await
        .expect("提示成功");

    // 通知按线上顺序原地落地：响应到达时更新序列已经完整
    assert_eq!(
        log.tags(),
        [
            "prompt_started",
            "assistant_delta",
            "assistant_delta",
            "tool_started",
            "permission_requested",
            "permission_resolved",
            "tool_finished",
            "assistant_delta",
            "prompt_finished",
        ]
    );

    // 拿到这次提示的全部更新后统一检查：先看顺序，再看内容
    let updates = log.take();

    // 工具参数随 `tool_started` 一次性给出，内容就是执行时用的那一份
    let arguments = updates
        .iter()
        .find_map(|update| match update {
            SessionUpdate::ToolStarted { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .expect("收到 tool_started");
    assert_eq!(arguments, "{\"cmd\":\"cargo test\"}");

    let finished = updates
        .iter()
        .find_map(|update| match update {
            update @ SessionUpdate::PromptFinished { .. } => Some(update.clone()),
            _ => None,
        })
        .expect("收到提示_finished");
    match finished {
        SessionUpdate::PromptFinished {
            message,
            finish_reason,
            usage,
            prompt_id,
        } => {
            assert_eq!(message, result.message, "通知与响应必须是同一份权威内容");
            assert_eq!(finish_reason, result.finish_reason);
            assert_eq!(usage, result.usage);
            assert_eq!(prompt_id, result.prompt_id);
        }
        other => panic!("期待提示_finished，收到 {other:?}"),
    }
    assert_eq!(result.finish_reason, FinishReason::ToolCalls);
    assert_eq!(service.state().asks, vec![Ok("allow_once".to_owned())]);
}

#[tokio::test]
async fn permission_denied_yields_tool_status_denied() {
    let service = ScriptedService::new();
    let log = UpdateLog::default();
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(log.clone(), "reject_once")).await;

    let result = handle
        .call::<SessionPrompt>(prompt_params("ses_1", "跑测试"))
        .await
        .expect("被拒授权不该让整次提示失败");

    let denied = log
        .take()
        .into_iter()
        .find_map(|update| match update {
            SessionUpdate::ToolFinished { status, .. } => Some(status),
            _ => None,
        })
        .expect("收到 tool_finished");
    assert_eq!(denied, ToolStatus::Denied);
    assert_eq!(result.finish_reason, FinishReason::ToolCalls);
    assert_eq!(service.state().asks, vec![Ok("reject_once".to_owned())]);
}

#[tokio::test]
async fn cancel_from_second_task_ends_turn_cancelled() {
    let service = ScriptedService::new();
    service.prompt_mode(PromptMode::UntilCancelled);
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(UpdateLog::default(), "allow_once")).await;

    // 一条任务在跑提示，另一条任务发取消：并发调用靠同一条连接的 ID 空间区分
    let prompt_handle = handle.clone();
    let prompt = tokio::spawn(async move {
        prompt_handle
            .call::<SessionPrompt>(prompt_params("ses_1", "长任务"))
            .await
    });
    wait_until(|| service.state().running_prompts == 1).await;

    let cancelled = handle
        .call::<SessionCancel>(SessionCancelParams {
            session_id: "ses_1".to_owned(),
        })
        .await
        .expect("取消成功");
    assert!(cancelled.cancelled);

    let result = prompt.await.expect("任务正常结束").expect("提示正常收尾");
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert_eq!(service.state().cancel_seen, 1);

    // 幂等：没有进行中的提示时也是成功，只是什么都没取消
    let again = handle
        .call::<SessionCancel>(SessionCancelParams {
            session_id: "ses_1".to_owned(),
        })
        .await
        .expect("重复取消成功");
    assert!(!again.cancelled);
}

#[tokio::test]
async fn same_session_second_prompt_is_busy_and_other_sessions_run_concurrently() {
    let service = ScriptedService::new();
    let gate = Arc::new(Notify::new());
    service.prompt_mode(PromptMode::Gated(Arc::clone(&gate)));
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(UpdateLog::default(), "allow_once")).await;

    let first_handle = handle.clone();
    let first = tokio::spawn(async move {
        first_handle
            .call::<SessionPrompt>(prompt_params("ses_1", "第一轮"))
            .await
    });
    wait_until(|| service.state().running_prompts == 1).await;

    // 同一会话第二次：协议层直接拒绝，不进 core
    let busy = handle
        .call::<SessionPrompt>(prompt_params("ses_1", "抢同一会话"))
        .await
        .expect_err("同一会话应当忙碌");
    assert_eq!(busy.code(), Some(RpcErrorCode::SessionBusy.code()));
    let compact_busy = handle
        .call::<SessionCompact>(SessionCompactParams {
            session_id: "ses_1".to_owned(),
            instructions: None,
        })
        .await
        .expect_err("活动提示期间拒绝压缩");
    assert_eq!(compact_busy.code(), Some(RpcErrorCode::SessionBusy.code()));
    // 重试与提示共用同一份提示注册表：活动提示期间不允许在旁边再起一次。
    let retry_busy = handle
        .call::<SessionRetry>(SessionRetryParams {
            session_id: "ses_1".to_owned(),
        })
        .await
        .expect_err("活动提示期间拒绝重试");
    assert_eq!(retry_busy.code(), Some(RpcErrorCode::SessionBusy.code()));

    // 另一个会话并发跑
    let second_handle = handle.clone();
    let second = tokio::spawn(async move {
        second_handle
            .call::<SessionPrompt>(prompt_params("ses_2", "第二轮"))
            .await
    });
    wait_until(|| service.state().running_prompts == 2).await;
    gate.notify_waiters();

    first.await.expect("任务正常结束").expect("第一次提示成功");
    second.await.expect("任务正常结束").expect("第二次提示成功");
    assert_eq!(service.state().peak_prompts, 2, "两会话确实重叠执行");
}

#[tokio::test]
async fn connection_loss_cancels_running_prompt() {
    let service = ScriptedService::new();
    service.prompt_mode(PromptMode::Hang);
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(UpdateLog::default(), "allow_once")).await;

    let prompt_handle = handle.clone();
    let prompt = tokio::spawn(async move {
        prompt_handle
            .call::<SessionPrompt>(prompt_params("ses_1", "永不结束"))
            .await
    });
    wait_until(|| service.state().running_prompts == 1).await;

    // 前端关闭连接：服务端必须被 drop 掉这一次提示的 future，而不是留个孤儿任务
    handle.close().await;
    let outcome = prompt.await.expect("任务正常结束");
    assert!(
        matches!(outcome, Err(RpcCallError::Closed)),
        "连接已死，调用应当收到 Closed：{outcome:?}"
    );

    wait_until(|| {
        service
            .state()
            .prompt_dropped
            .as_ref()
            .is_some_and(|dropped| dropped.load(Ordering::SeqCst))
    })
    .await;
    assert!(
        service
            .state()
            .cancel_at_drop
            .as_ref()
            .expect("记录了 drop 标记")
            .load(Ordering::SeqCst),
        "drop 服务 future 时取消令牌应当已触发"
    );
}

#[tokio::test]
async fn dropping_the_last_handle_closes_the_connection() {
    let (_server, connection, handle, _) =
        local_client(ScriptedService::new(), ClientBuilder::new(client_info())).await;
    let observer = handle.clone();
    assert!(!connection.is_closed());

    drop(handle);
    assert!(!observer.is_closed(), "还有句柄存活时连接不该关");

    drop(observer);
    tokio::time::timeout(Duration::from_secs(3), connection.wait())
        .await
        .expect("最后一个句柄丢弃后连接应当收尾");
    assert!(connection.is_closed());
}

#[tokio::test]
async fn unknown_update_type_is_dropped_without_breaking_the_connection() {
    // 这一侧扮演「版本更新的 core」：前端认不出新更新类型时，必须只丢那一条
    let log = UpdateLog::default();
    let (client_side, server_side) = moc_protocol::transport::duplex(64 * 1024);
    let mut peer = support::RawPeer::new(
        server_side,
        moc_protocol::Framing::ContentLength,
        moc_protocol::DEFAULT_MAX_FRAME_BYTES,
    );
    let builder = builder(log.clone(), "allow_once");
    let connecting = tokio::spawn(async move { builder.connect(client_side).await });

    // 握手：前端会自动发 initialize
    let handshake_request = match peer
        .recv_until(|message| matches!(message, moc_protocol::RpcMessage::Request(_)))
        .await
    {
        moc_protocol::RpcMessage::Request(request) => request,
        other => panic!("期待 initialize 请求，收到 {other:?}"),
    };
    assert_eq!(handshake_request.method, "initialize");
    peer.send(&moc_protocol::RpcMessage::Response(
        moc_protocol::Response::result(handshake_request.id, support::initialize_result()),
    ))
    .await;
    let (handle, handshake) = connecting.await.expect("任务正常结束").expect("握手成功");
    assert_eq!(handshake.server.name, "raw-core");

    // 一条未来版本的更新 + 一条当前版本的更新
    peer.send(&support::notification(
        "session/update",
        serde_json::json!({
            "session_id": "ses_1",
            "update": { "type": "future_thing", "prompt_id": "prompt_1", "payload": { "x": 1 } }
        }),
    ))
    .await;
    peer.send(&support::notification(
        "session/update",
        serde_json::json!({
            "session_id": "ses_1",
            "update": { "type": "title_changed", "title": "新标题" }
        }),
    ))
    .await;
    wait_until(|| log.tags() == ["title_changed"]).await;

    // 连接照常可用
    let ping_handle = handle.clone();
    let ping = tokio::spawn(async move {
        ping_handle
            .call::<moc_protocol::Ping>(moc_protocol::EmptyParams {})
            .await
    });
    let ping_request = match peer
        .recv_until(|message| matches!(message, moc_protocol::RpcMessage::Request(_)))
        .await
    {
        moc_protocol::RpcMessage::Request(request) => request,
        other => panic!("期待 ping 请求，收到 {other:?}"),
    };
    assert_eq!(ping_request.method, "ping");
    peer.send(&moc_protocol::RpcMessage::Response(
        moc_protocol::Response::result(ping_request.id, serde_json::json!({ "server_time_ms": 7 })),
    ))
    .await;
    let result = ping
        .await
        .expect("任务正常结束")
        .expect("未知更新不该影响连接");
    assert_eq!(result.server_time_ms, 7);
}

#[tokio::test]
async fn duplicate_handler_registration_is_rejected() {
    let builder = ClientBuilder::new(client_info())
        .on_request::<SessionRequestPermission, _, _>(permission_handler("allow_once"))
        .on_request::<SessionRequestPermission, _, _>(permission_handler("reject_once"));
    let (client_side, server_side) = moc_protocol::transport::duplex(8 * 1024);
    let server = moc_protocol::Server::new(ScriptedService::new());
    let _connection = server.spawn(server_side);
    let error = match builder.connect(client_side).await {
        Ok(_) => panic!("重复注册必须失败"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        moc_protocol::ConnectError::Protocol(ProtocolError::DuplicateHandler(
            "session/request_permission"
        ))
    ));
}

#[tokio::test]
async fn session_lock_blocks_writes_but_allows_reads() {
    let service = ScriptedService::new();
    service.lock_session("ses_1", 4242, "web-tui");
    let log = UpdateLog::default();
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(log.clone(), "allow_once")).await;

    // 写操作全部被拒
    for error in [
        handle
            .call::<SessionRename>(SessionRenameParams {
                session_id: "ses_1".to_owned(),
                title: "改名".to_owned(),
            })
            .await
            .expect_err("被锁时禁止改名")
            .code(),
        handle
            .call::<SessionDelete>(SessionDeleteParams {
                session_id: "ses_1".to_owned(),
            })
            .await
            .expect_err("被锁时禁止删除")
            .code(),
        handle
            .call::<SessionPrompt>(prompt_params("ses_1", "跑一次提示"))
            .await
            .expect_err("被锁时禁止跑提示")
            .code(),
    ] {
        assert_eq!(error, Some(RpcErrorCode::SessionLocked.code()));
    }

    // 读操作放行，锁投影可见
    let loaded = handle
        .call::<SessionLoad>(SessionLoadParams {
            session_id: "ses_1".to_owned(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("被锁也能读");
    let lock = loaded.session.lock.expect("锁投影可见");
    assert_eq!(lock.pid, 4242);
    assert_eq!(lock.frontend, "web-tui");
    assert_eq!(lock.process_name.as_deref(), Some("other-core"));

    // 力不足的解锁解不掉别人的锁
    let refused = handle
        .call::<SessionUnlock>(SessionUnlockParams {
            session_id: "ses_1".to_owned(),
            force: false,
        })
        .await
        .expect_err("force=false 解不掉别人的锁");
    assert_eq!(refused.code(), Some(RpcErrorCode::SessionLocked.code()));

    // 强夺成功后 core 推送 lock_changed
    handle
        .call::<SessionUnlock>(SessionUnlockParams {
            session_id: "ses_1".to_owned(),
            force: true,
        })
        .await
        .expect("强夺成功");
    assert!(service.lock_of("ses_1").is_none());
    assert!(
        matches!(
            log.find("lock_changed"),
            Some(SessionUpdate::LockChanged { lock: None })
        ),
        "前端应当收到解锁推送：{:?}",
        log.tags()
    );

    // 解锁后写操作恢复：被拒的那次提示没有留下占位
    handle
        .call::<SessionPrompt>(prompt_params("ses_1", "现在可以跑了"))
        .await
        .expect("解锁后可以跑提示");
}

#[tokio::test]
async fn rename_emits_title_changed_and_returns_session_info() {
    let service = ScriptedService::new();
    let log = UpdateLog::default();
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(log.clone(), "allow_once")).await;

    let info = handle
        .call::<SessionRename>(SessionRenameParams {
            session_id: "ses_2".to_owned(),
            title: "重构协议层".to_owned(),
        })
        .await
        .expect("改名成功");
    assert_eq!(info.id, "ses_2");
    assert_eq!(info.title, "重构协议层");
    assert_eq!(info.updated_at_ms, 500);
    assert!(
        service
            .calls()
            .contains(&"session_rename(ses_2)".to_owned())
    );

    match log.find("title_changed") {
        Some(SessionUpdate::TitleChanged { title }) => assert_eq!(title, "重构协议层"),
        other => panic!("期待 title_changed，收到 {other:?}"),
    }
}

#[tokio::test]
async fn load_honors_max_entries() {
    let service = ScriptedService::new();
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(UpdateLog::default(), "allow_once")).await;

    let full = handle
        .call::<SessionLoad>(SessionLoadParams {
            session_id: "ses_1".to_owned(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("全量读取");
    assert_eq!(full.entries.len(), 5);
    assert!(!full.truncated);
    assert_eq!(
        full.entries.last().map(|entry| entry.id.as_str()),
        Some("ses_1_e4")
    );

    let tail = handle
        .call::<SessionLoad>(SessionLoadParams {
            session_id: "ses_1".to_owned(),
            max_entries: Some(2),
            read_only: false,
        })
        .await
        .expect("截断读取");
    assert_eq!(tail.entries.len(), 2);
    assert!(tail.truncated);
    assert_eq!(
        tail.entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["ses_1_e3", "ses_1_e4"],
        "截断保留最新条目"
    );
    // 截断只影响返回值，会话本身不变
    assert_eq!(tail.session.id, "ses_1");
    assert_eq!(service.state().sessions["ses_1"].entries.len(), 5);

    let missing = handle
        .call::<SessionLoad>(SessionLoadParams {
            session_id: "ses_不存在".to_owned(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect_err("未知会话");
    assert_eq!(missing.code(), Some(RpcErrorCode::SessionNotFound.code()));
}

#[tokio::test]
async fn unload_invokes_service_and_session_stays_listed() {
    let service = ScriptedService::new();
    let (_server, _connection, handle, _) =
        local_client(service.clone(), builder(UpdateLog::default(), "allow_once")).await;

    let empty = handle
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: "ses_3".to_owned(),
        })
        .await
        .expect("卸载成功");
    assert_eq!(empty, moc_protocol::methods::Empty {});
    assert!(
        service
            .calls()
            .contains(&"session_unload(ses_3)".to_owned())
    );

    let list = handle
        .call::<SessionList>(SessionListParams {})
        .await
        .expect("session/list 成功");
    assert!(
        list.sessions.iter().any(|session| session.id == "ses_3"),
        "卸载只释放常驻资源，不删除会话"
    );

    let missing = handle
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: "ses_不存在".to_owned(),
        })
        .await
        .expect_err("未知会话");
    assert_eq!(missing.code(), Some(RpcErrorCode::SessionNotFound.code()));
}
