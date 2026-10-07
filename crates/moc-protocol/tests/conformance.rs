//! 服务端一致性测试：用原始对端（[`support::RawPeer`]）直读直写原始帧。
//!
//! 这里刻意**不用** crate 客户端：自家客户端会把服务端的缺陷（漏回响应、回错 ID、
//! 多回一份）掩盖掉，因为它只按契约解析自己期待的东西。

mod support;

use std::sync::Arc;
use std::time::Duration;

use moc_protocol::frame::Framing;
use moc_protocol::{Id, PROTOCOL_VERSION, Response, RpcErrorCode, RpcMessage, ServerOptions};
use serde_json::json;
use support::{
    PromptMode, ScriptedService, handshake, initialize_params, notification, options_with_framing,
    raw_pair, request, wait_until,
};
use tokio::sync::Notify;

/// 两帧之间的静默窗口：用于「不该有字节」的断言。
const SILENCE: Duration = Duration::from_millis(200);

#[tokio::test]
async fn echoes_id_and_jsonrpc_version() {
    let (_server, _connection, mut peer) = raw_pair(
        ScriptedService::new(),
        options_with_framing(Framing::ContentLength),
    );
    peer.send(&request(
        7,
        "initialize",
        initialize_params(PROTOCOL_VERSION),
    ))
    .await;
    let raw = peer.recv_json().await;
    assert_eq!(raw["jsonrpc"], "2.0");
    assert_eq!(raw["id"], 7);
    assert_eq!(raw["result"]["protocol_version"], PROTOCOL_VERSION);

    // 字符串 ID 原样回显
    peer.send(&RpcMessage::Request(moc_protocol::Request::new(
        Id::String("abc".to_owned()),
        "ping",
        Some(json!({})),
    )))
    .await;
    let raw = peer.recv_json().await;
    assert_eq!(raw["jsonrpc"], "2.0");
    assert_eq!(raw["id"], "abc");
    assert!(
        raw["result"]["server_time_ms"]
            .as_u64()
            .expect("毫秒时间戳")
            > 0
    );
}

#[tokio::test]
async fn notification_is_never_answered() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    handshake(&mut peer).await;

    // 已注册方法名与未注册方法名的通知都不产生响应
    peer.send(&notification("ping", json!({}))).await;
    peer.send(&notification("no/such", json!({}))).await;
    assert!(peer.expect_silence(SILENCE).await, "通知永远不产生响应字节");

    // 连接仍然可用
    peer.send(&request(2, "ping", json!({}))).await;
    peer.recv_ok(2).await;
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    handshake(&mut peer).await;
    // 方法表按方向隔离：服务端→客户端的方法在服务端侧就是未知方法
    for (id, method) in [
        (2, "no/such"),
        (3, "session/update"),
        (4, "session/request_permission"),
    ] {
        peer.send(&request(id, method, json!({}))).await;
        assert_eq!(
            peer.recv_err(id).await,
            RpcErrorCode::MethodNotFound.code(),
            "方法 {method} 应当未知"
        );
    }
}

#[tokio::test]
async fn malformed_json_returns_parse_error_with_null_id() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    peer.send_payload(b"{oops").await;
    let raw = peer.recv_json().await;
    assert_eq!(raw["jsonrpc"], "2.0");
    assert_eq!(raw["id"], json!(null), "解析错误响应必须回 null ID");
    assert_eq!(raw["error"]["code"], RpcErrorCode::ParseError.code());
    assert!(raw.get("result").is_none());

    // 报文边界是清楚的：连接可继续使用
    handshake(&mut peer).await;
}

#[tokio::test]
async fn invalid_envelope_returns_invalid_request() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    // 顺序与断言的 ID 回收策略一致：能读到合法 ID 就回显，读不到回 null
    let cases: [(&str, serde_json::Value); 7] = [
        (r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#, json!(1)),
        (r#"{"id":1,"method":"ping"}"#, json!(1)),
        (r#"{"jsonrpc":"2.0","id":1,"method":""}"#, json!(1)),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":"x"}"#,
            json!(1),
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","extra":1}"#,
            json!(1),
        ),
        (r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#, json!(null)),
        (r#"{"jsonrpc":"2.0","id":1}"#, json!(1)),
    ];
    for (payload, expected_id) in cases {
        peer.send_payload(payload.as_bytes()).await;
        let raw = peer.recv_json().await;
        assert_eq!(
            raw["error"]["code"],
            RpcErrorCode::InvalidRequest.code(),
            "报文应当被拒绝：{payload}"
        );
        assert_eq!(raw["id"], expected_id, "ID 回显不符：{payload}");
    }
    // 连接未受影响
    handshake(&mut peer).await;
}

#[tokio::test]
async fn batch_array_is_rejected() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    peer.send_payload(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#)
        .await;
    let raw = peer.recv_json().await;
    assert_eq!(raw["id"], json!(null));
    assert_eq!(raw["error"]["code"], RpcErrorCode::InvalidRequest.code());
    assert!(
        peer.expect_silence(SILENCE).await,
        "batch 只产生一个错误响应"
    );
}

#[tokio::test]
async fn uninitialized_request_is_rejected_and_duplicate_initialize_fails() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    peer.send(&request(1, "session/list", json!({}))).await;
    assert_eq!(
        peer.recv_err(1).await,
        RpcErrorCode::ServerNotInitialized.code()
    );

    peer.send(&request(
        2,
        "initialize",
        initialize_params(PROTOCOL_VERSION),
    ))
    .await;
    peer.recv_ok(2).await;

    peer.send(&request(
        3,
        "initialize",
        initialize_params(PROTOCOL_VERSION),
    ))
    .await;
    assert_eq!(peer.recv_err(3).await, RpcErrorCode::InvalidRequest.code());
}

#[tokio::test]
async fn protocol_version_mismatch_reports_server_version() {
    let (_server, _connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    peer.send(&request(1, "initialize", initialize_params("2.0")))
        .await;
    let raw = peer.recv_json().await;
    assert_eq!(
        raw["error"]["code"],
        RpcErrorCode::UnsupportedProtocolVersion.code()
    );
    assert_eq!(raw["error"]["data"]["server_version"], PROTOCOL_VERSION);
    assert_eq!(raw["error"]["data"]["server_version"], "1.2");

    // 版本不兼容不占用握手状态：改正后仍能握手
    peer.send(&request(
        2,
        "initialize",
        initialize_params(PROTOCOL_VERSION),
    ))
    .await;
    peer.recv_ok(2).await;
}

#[tokio::test]
async fn invalid_params_returns_invalid_params() {
    let service = ScriptedService::new();
    let (_server, _connection, mut peer) = raw_pair(service.clone(), ServerOptions::default());
    handshake(&mut peer).await;

    peer.send(&request(2, "session/new", json!({ "cwdly": "/tmp" })))
        .await;
    assert_eq!(peer.recv_err(2).await, RpcErrorCode::InvalidParams.code());

    peer.send(&request(
        3,
        "session/prompt",
        json!({ "input": [{ "type": "text", "text": "hi" }] }),
    ))
    .await;
    assert_eq!(peer.recv_err(3).await, RpcErrorCode::InvalidParams.code());

    assert!(
        service.calls().is_empty(),
        "参数非法时不得调用 core：{:?}",
        service.calls()
    );
    // 被拒的请求不该占住会话提示
    peer.send(&request(
        4,
        "session/cancel",
        json!({ "session_id": "ses_1" }),
    ))
    .await;
    let raw = peer.recv_ok(4).await;
    assert_eq!(raw["cancelled"], false);
}

#[tokio::test]
async fn ping_does_not_touch_service() {
    let service = ScriptedService::new();
    let (_server, _connection, mut peer) = raw_pair(service.clone(), ServerOptions::default());
    handshake(&mut peer).await;
    peer.send(&request(2, "ping", json!({}))).await;
    let raw = peer.recv_ok(2).await;
    assert!(raw["server_time_ms"].as_u64().expect("毫秒时间戳") > 0);
    assert!(
        service.calls().is_empty(),
        "ping 由协议层自答，不进 core：{:?}",
        service.calls()
    );
}

#[tokio::test]
async fn in_flight_limit_returns_server_busy() {
    let service = ScriptedService::new();
    let gate = Arc::new(Notify::new());
    service.prompt_mode(PromptMode::Gated(Arc::clone(&gate)));
    let options = ServerOptions {
        max_in_flight: 1,
        ..ServerOptions::default()
    };
    let (_server, _connection, mut peer) = raw_pair(service.clone(), options);
    handshake(&mut peer).await;

    // 先挂住一次提示
    peer.send(&request(
        2,
        "session/prompt",
        json!({ "session_id": "ses_1", "input": [{ "type": "text", "text": "hi" }] }),
    ))
    .await;
    wait_until(|| service.state().running_prompts == 1).await;

    // 许可用尽：不排队，直接拒绝
    peer.send(&request(3, "session/list", json!({}))).await;
    assert_eq!(peer.recv_err(3).await, RpcErrorCode::ServerBusy.code());

    // 释放后连接照常可用（这一次提示要过权限询问，对端自动放行）
    gate.notify_waiters();
    let (response, _) = support::recv_with_auto_permission(&mut peer, 2, "allow_once").await;
    assert!(response.payload.is_ok(), "挂住的提示应当正常完成");
    peer.send(&request(4, "session/list", json!({}))).await;
    peer.recv_ok(4).await;
}

#[tokio::test]
async fn server_request_without_handler_returns_method_not_found() {
    let service = ScriptedService::new();
    let (_server, _connection, mut peer) = raw_pair(service.clone(), ServerOptions::default());
    handshake(&mut peer).await;
    peer.send(&request(
        2,
        "session/prompt",
        json!({ "session_id": "ses_1", "input": [{ "type": "text", "text": "hi" }] }),
    ))
    .await;

    // 核心侧发起的反向请求：用负 ID，前端「没有处理器」时应当回 -32601
    let permission = match peer
        .recv_until(|message| matches!(message, RpcMessage::Request(_)))
        .await
    {
        RpcMessage::Request(request) => request,
        other => panic!("期待反向请求，收到 {other:?}"),
    };
    assert_eq!(permission.method, "session/request_permission");
    assert!(
        matches!(permission.id, Id::Number(number) if number < 0),
        "反向请求用负 ID：{:?}",
        permission.id
    );
    peer.send(&RpcMessage::Response(Response::error(
        Some(permission.id.clone()),
        moc_protocol::RpcError::of(RpcErrorCode::MethodNotFound),
    )))
    .await;

    // 提示照常收尾：工具状态记为拒绝
    let (response, updates) = support::recv_response_with_updates(&mut peer, 2).await;
    assert!(response.payload.is_ok(), "缺处理器不该让整次提示失败");
    assert_eq!(
        service.state().asks,
        vec![Err(RpcErrorCode::MethodNotFound.code())]
    );
    let finished = updates
        .iter()
        .find_map(|update| match update {
            moc_protocol::methods::SessionUpdate::ToolFinished { status, .. } => Some(*status),
            _ => None,
        })
        .expect("收到 tool_finished");
    assert_eq!(finished, moc_protocol::methods::ToolStatus::Denied);
    let resolved = updates
        .iter()
        .find_map(|update| match update {
            moc_protocol::methods::SessionUpdate::PermissionResolved { option_id, .. } => {
                Some(option_id.clone())
            }
            _ => None,
        })
        .expect("收到 permission_resolved");
    assert_eq!(resolved, None, "没人应答时按未给选择处理");
}

#[tokio::test]
async fn oversized_response_becomes_internal_error() {
    let service = ScriptedService::new();
    service.with(|state| state.config_padding = 8192);
    let options = ServerOptions {
        max_frame_bytes: 1024,
        ..ServerOptions::default()
    };
    let (_server, _connection, mut peer) = raw_pair(service, options);
    handshake(&mut peer).await;

    peer.send(&request(2, "config/get", json!({}))).await;
    assert_eq!(peer.recv_err(2).await, RpcErrorCode::InternalError.code());

    // 连接还活着：下一帧照常处理
    peer.send(&request(3, "ping", json!({}))).await;
    peer.recv_ok(3).await;
}

#[tokio::test]
async fn fatal_frame_error_closes_connection() {
    let (_server, connection, mut peer) =
        raw_pair(ScriptedService::new(), ServerOptions::default());
    peer.send_bytes(b"Content-Length: abc\r\n\r\n").await;
    assert!(
        peer.expect_eof(Duration::from_secs(2)).await,
        "分帧错位必须关闭连接"
    );
    tokio::time::timeout(Duration::from_secs(2), connection.wait())
        .await
        .expect("连接应当在收尾后结束");
    assert!(connection.is_closed());
}
