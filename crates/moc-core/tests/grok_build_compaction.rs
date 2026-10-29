//! Grok Build soft 压缩的端到端行为：9 段摘要、full-replace 回放、输入阶梯与两遍预火。

mod support;

use moc_core::session::entries::Entry;
use moc_protocol::methods::{
    SessionCompact, SessionCompactParams, SessionMode, SessionNew, SessionNewParams, SessionUpdate,
};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::Harness;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

/// 旧助手文本：压缩后必须从模型视图消失。
const OLD_ANSWER: &str = "OLD-ASSISTANT-MARKER";

/// 第一遍摘要的独有标记：只能出现在第二遍的输入里。
const PASS1_MARKER: &str = "PASS1-NOTE-MARKER";

/// 第二遍摘要的独有标记：只有它能成为最终 checkpoint。
const PASS2_MARKER: &str = "PASS2-SUMMARY-MARKER";

/// Grok Build 的 9 段摘要；正文远超 500 字符阈值与 1000 字符的 NOTE₁ 阈值。
fn grok_summary(marker: &str) -> String {
    let sections = [
        "1. Primary Request and Intent",
        "2. Key Technical Concepts",
        "3. Files and Code Sections",
        "4. Errors and Fixes",
        "5. Problem Solving",
        "6. All User Messages",
        "7. Pending Tasks",
        "8. Current Work",
        "9. Optional Next Step",
    ];
    let mut body = format!("<summary>\n{marker}\n");
    for section in sections {
        body.push_str(section);
        body.push_str(": 保留这一段的工程细节，包括路径、命令与决策理由。\n");
    }
    // 补足长度：退化判定与 NOTE₁ 提取都按字符数设阈值。每行都不同，避免触发正文循环保护。
    for index in 0..25 {
        body.push_str(&format!(
            "补充第 {index} 条细节：涉及 crates/moc-core/src/compact/grok_build/mod.rs 的第 {index} 段逻辑，命令为 cargo test -p moc-core，原因是保留完整上下文。\n"
        ));
    }
    body.push_str("</summary>");
    body
}

/// 请求体里的对话原文（含系统提示），用于断言模型看到什么。
///
/// 不能直接对 `Value` 取 `to_string()`：那会转义引号，标签匹配会失效。
fn messages_text(body: &Value) -> String {
    let mut out = String::new();
    let Some(messages) = body["messages"].as_array() else {
        return out;
    };
    for message in messages {
        match &message["content"] {
            Value::String(text) => {
                out.push_str(text);
                out.push('\n');
            }
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        out.push_str(text);
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 递归收集会话目录下的所有文件名。
fn file_names(root: &std::path::Path, output: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            file_names(&path, output);
        } else if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            output.push(name.to_owned());
        }
    }
}

#[tokio::test]
async fn grok_build_full_replace_and_replay() {
    let upstream = MockServer::builder().start().await;
    let summary = grok_summary("GROK-SUMMARY-MARKER");
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream(OLD_ANSWER), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(support::complete(&summary))
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='grok_build'\nmethod_order=['soft']\nauto=false\nkeep_recent_tokens=100\n",
    )
    .await;
    let id = harness.session(SessionMode::Normal).await;
    harness.prompt(&id, "第一轮：实现可切换 soft 压缩").await;

    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "grok_build");

    let requests = upstream.received_requests().await.unwrap();
    let compaction = requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .find(|body| body["stream"] != true)
        .expect("发生过一次摘要请求");
    let prompt_text = messages_text(&compaction);
    for section in [
        "1. Primary Request and Intent",
        "6. All User Messages",
        "9. Optional Next Step",
    ] {
        assert!(prompt_text.contains(section), "缺少 {section}");
    }
    assert!(
        !prompt_text.contains("## Primary Request and Intent"),
        "不应再使用 moc 的 8 段提示词"
    );

    harness.prompt(&id, "第二轮：继续").await;
    let requests = upstream.received_requests().await.unwrap();
    let replay = requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .rfind(|body| body["stream"] == true)
        .expect("第二轮请求存在");
    let replay_text = messages_text(&replay);
    assert!(
        replay_text.contains("第一轮：实现可切换 soft 压缩"),
        "最后一条用户查询必须被重注入"
    );
    assert!(
        replay_text.contains(
            "This session is being continued from a previous conversation that ran out of context."
        ),
        "缺少续接前言"
    );
    assert!(replay_text.contains("Summary:"), "缺少清洗后的摘要");
    assert!(
        !replay_text.contains(OLD_ANSWER),
        "压缩前的旧消息必须从模型视图消失"
    );
    assert!(
        !replay_text.contains("<compaction-history>"),
        "严格 Grok 不写历史指针"
    );
    assert!(
        !replay_text.contains("<compaction-runtime>"),
        "严格 Grok 不注入运行时快照"
    );

    assert!(
        harness.updates.lock().iter().any(|update| matches!(
            update,
            SessionUpdate::ContextCompacted { method, .. } if method == "grok_build"
        )),
        "必须广播 grok_build checkpoint"
    );

    let mut names = Vec::new();
    file_names(&harness.root.path().join("sessions"), &mut names);
    assert!(
        !names
            .iter()
            .any(|name| name == "INDEX.md" || name.starts_with("segment-")),
        "严格 Grok 模式不写压缩归档：{names:?}"
    );
}

#[tokio::test]
async fn grok_build_retries_degenerate_summary() {
    // 集成测试不等待真实的 3 秒退避。
    // SAFETY: 本用例串行执行且先于压缩请求设置变量；集成测试二进制内的其他用例
    // 不读取该变量，唯一的并发读者是自己发起的压缩任务。
    unsafe { std::env::set_var("GROK_COMPACT_RETRY_MS", "1") };
    let upstream = MockServer::builder().start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let summary = grok_summary("GROK-SUMMARY-MARKER");
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream(OLD_ANSWER), "text/event-stream");
            }
            // 第一次只给一句退化输出，第二次给合法摘要。
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200)
                    .set_body_json(support::complete("<summary>太短</summary>"))
            } else {
                ResponseTemplate::new(200).set_body_json(support::complete(&summary))
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='grok_build'\nmethod_order=['soft']\nauto=false\n",
    )
    .await;
    let id = harness.session(SessionMode::Normal).await;
    harness.prompt(&id, "写点东西").await;
    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "grok_build");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "退化输出必须触发一次重试"
    );
    // SAFETY: 压缩请求已经结束，本用例不再有并发读者；清除变量避免影响同进程的
    // 其他用例。
    unsafe { std::env::remove_var("GROK_COMPACT_RETRY_MS") };
}

#[tokio::test]
async fn grok_build_ladder_shrinks_after_context_length() {
    let upstream = MockServer::builder().start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let summary = grok_summary("GROK-SUMMARY-MARKER");
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream(OLD_ANSWER), "text/event-stream");
            }
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {
                        "message": "This model's maximum context length is exceeded",
                        "type": "invalid_request_error",
                        "code": "context_length_exceeded"
                    }
                }));
            }
            ResponseTemplate::new(200).set_body_json(support::complete(&summary))
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[providers.test.models.small]\ncontext_window=130000\nmax_output_tokens=4096\n\
         [compaction]\nsoft_method='grok_build'\nmethod_order=['soft']\nauto=false\n",
    )
    .await;
    let id = harness
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: Some("test/small".into()),
            title: None,
            mode: SessionMode::Normal,
            plan_active: false,
        })
        .await
        .unwrap()
        .id;
    // 100k token 的单轮对话：Fitted 档先裁剪，上游报超窗后 Lossy 档进一步收紧。
    harness.prompt(&id, &"a".repeat(400_000)).await;

    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "grok_build");
    assert_eq!(requests.load(Ordering::SeqCst), 2, "一次超窗后换档重试");

    let bodies: Vec<Value> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .filter(|body| body["stream"] != true)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[1]["messages"].to_string().len() < bodies[0]["messages"].to_string().len(),
        "换到有损档后请求必须更小"
    );
}

#[tokio::test]
async fn grok_build_prefire_feeds_note1_into_second_pass() {
    let upstream = MockServer::builder().start().await;
    let summary = grok_summary("GROK-SUMMARY-MARKER");
    let pass1 = grok_summary(PASS1_MARKER);
    let pass2 = grok_summary(PASS2_MARKER);
    // 预火的第一遍请求体与单遍压缩无法从内容区分，按到达顺序认第一个非流式请求。
    let sampled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sampled);
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream(OLD_ANSWER), "text/event-stream");
            }
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            if first {
                return ResponseTemplate::new(200).set_body_json(support::complete(&pass1));
            }
            if messages_text(&body).contains("This session is being continued") {
                ResponseTemplate::new(200).set_body_json(support::complete(&pass2))
            } else {
                ResponseTemplate::new(200).set_body_json(support::complete(&summary))
            }
        })
        .mount(&upstream)
        .await;
    // 阈值 60k、窗口 1M：预火区间被压到 [0, 60k)，短对话也能落在区间内。
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='grok_build'\nmethod_order=['soft']\n\
         threshold={mode='tokens',value=60000}\n",
    )
    .await;
    let id = harness.session(SessionMode::Normal).await;
    for round in 0..3 {
        harness
            .prompt(&id, &format!("第 {round} 轮：继续推进"))
            .await;
    }
    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "grok_build");

    let bodies: Vec<Value> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .filter(|body| body["stream"] != true)
        .collect();
    assert_eq!(
        bodies.len(),
        2,
        "预火一遍 + 压缩一遍；实际 {} 次摘要请求",
        bodies.len()
    );
    assert!(
        messages_text(&bodies[1]).contains(PASS1_MARKER),
        "第二遍必须带着 NOTE₁ 前缀"
    );
    assert!(
        messages_text(&bodies[1]).contains("This session is being continued"),
        "第二遍必须是 full-replace 形状"
    );

    let entries = support::transcript_entries(&harness, &id).await;
    let summary = entries
        .iter()
        .find_map(|entry| match entry {
            Entry::Compaction { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("落库了 checkpoint");
    assert!(summary.contains(PASS2_MARKER), "{summary}");
    assert!(
        !summary.contains(PASS1_MARKER),
        "NOTE₁ 只进输入，不进 checkpoint"
    );
}

#[tokio::test]
async fn moc_soft_default_unchanged() {
    let upstream = MockServer::builder().start().await;
    let summary = grok_summary("GROK-SUMMARY-MARKER");
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream(OLD_ANSWER), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(support::complete(&summary))
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nauto=false\n",
    )
    .await;
    let id = harness.session(SessionMode::Normal).await;
    harness.prompt(&id, "第一轮").await;
    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "soft", "默认仍是 moc 自有 soft 实现");

    let bodies: Vec<Value> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect();
    let compaction = bodies
        .iter()
        .find(|body| body["stream"] != true)
        .expect("发生过一次摘要请求");
    assert!(
        messages_text(compaction).contains("## Primary Request and Intent"),
        "默认走 moc 的 8 段提示词"
    );

    harness.prompt(&id, "第二轮").await;
    let bodies: Vec<Value> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect();
    let replay = bodies
        .iter()
        .rfind(|body| body["stream"] == true)
        .expect("第二轮请求存在");
    assert!(
        messages_text(replay).contains("<compaction-summary method=\"soft\">"),
        "默认回放保持 moc 的摘要包装"
    );
}
