use super::support;

use moc_protocol::{
    InputPart, PromptParams, SessionMode, SessionPrompt, SessionSteer, SteerParams,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{Harness, complete, text_stream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn request(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::with_capacity(8192);
    loop {
        assert!(socket.read_buf(&mut bytes).await.unwrap() > 0);
        let Some(end) = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        if bytes.len() >= end + length {
            return serde_json::from_slice(&bytes[end..end + length]).unwrap();
        }
    }
}
async fn chunk(socket: &mut tokio::net::TcpStream, body: &str) {
    socket
        .write_all(format!("{:x}\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
    socket.flush().await.unwrap();
}

#[tokio::test]
async fn steering_is_delivered_after_the_current_response_and_before_next_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (ready_tx, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        request(&mut first).await;
        first.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
        chunk(&mut first,"data: {\"id\":\"first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"原回答\"}}]}\n\n").await;
        ready_tx.send(()).unwrap();
        let _ = gate.await;
        chunk(&mut first,"data: {\"id\":\"first\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
        first.write_all(b"0\r\n\r\n").await.unwrap();
        drop(first);
        let (mut second, _) = listener.accept().await.unwrap();
        let body = request(&mut second).await;
        let response = text_stream("按补充要求继续");
        second.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
        body
    });
    let harness = Harness::new(&url, "openai_chat", "[compaction]\nenabled=false").await;
    let id = harness.session(SessionMode::Normal).await;
    let client = harness.client.clone();
    let request_id = id.clone();
    let running = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: request_id,
                input: vec![InputPart::Text {
                    text: "开始任务".into(),
                }],
            })
            .await
            .unwrap()
    });
    ready.await.unwrap();
    let accepted = harness
        .client
        .call::<SessionSteer>(SteerParams {
            session_id: id.clone(),
            input: vec![InputPart::Text {
                text: "先保留原文件".into(),
            }],
        })
        .await
        .unwrap();
    assert!(accepted.accepted);
    release.send(()).unwrap();
    assert_eq!(running.await.unwrap().message.text(), "按补充要求继续");
    let request = server.await.unwrap();
    let messages = request["messages"].as_array().unwrap();
    let original = messages
        .iter()
        .position(|message| message["role"] == "assistant" && message["content"] == "原回答")
        .unwrap();
    let steering = messages
        .iter()
        .position(|message| message["role"] == "user" && message["content"] == "先保留原文件")
        .unwrap();
    assert!(steering > original);
    let idle = harness
        .client
        .call::<SessionSteer>(SteerParams {
            session_id: id,
            input: vec![InputPart::Text {
                text: "下一项".into(),
            }],
        })
        .await
        .unwrap();
    assert!(
        !idle.accepted,
        "关闭边界之后必须让前端按新轮次投递，不吞掉消息"
    );
    harness.close().await;
}

#[tokio::test]
async fn context_overflow_inside_sse_compacts_and_continues() {
    let upstream = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = counter.clone();
    Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(move|request:&wiremock::Request|{
        let body:Value=serde_json::from_slice(&request.body).unwrap();
        if body["stream"]!=true{return ResponseTemplate::new(200).set_body_json(complete("已保留任务目标与用户纠正"));}
        let response=match seen.fetch_add(1,Ordering::SeqCst){
            0=>text_stream("已记录"),
            1=>format!("data: {}\n\n",json!({"error":{"message":"maximum context length exceeded","code":"context_length_exceeded","type":"invalid_request_error"}})),
            _=>text_stream("恢复成功"),
        };
        ResponseTemplate::new(200).set_body_raw(response,"text/event-stream")
    }).mount(&upstream).await;
    let harness=Harness::new(&upstream.uri(),"openai_chat","[compaction]\nmethod_order=['soft']\nkeep_recent_tokens=100\nthreshold={mode='tokens',value=900000}").await;
    let id = harness.session(SessionMode::Normal).await;
    harness.prompt(&id, &"保留历史任务要求。".repeat(300)).await;
    assert_eq!(
        harness.prompt(&id, "继续实现").await.message.text(),
        "恢复成功"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    let requests = upstream.received_requests().await.unwrap();
    let retried: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert!(retried.to_string().contains("compaction-summary"));
    assert!(retried.to_string().contains("继续实现"));
    harness.close().await;
}
