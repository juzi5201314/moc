//! 手动 Bash 的执行、隐私、持久化和取消边界。

mod support;

use moc_protocol::{BashCommand, BashParams, SessionBash, methods::*};
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;
use support::Harness;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

async fn bash(harness: &Harness, id: &str, input: &str) -> moc_protocol::BashResult {
    harness
        .client
        .call::<SessionBash>(BashParams {
            session_id: id.into(),
            command: BashCommand::parse(input).unwrap(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn manual_bash_context_survives_resume_and_compaction_without_private_output() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("已读取命令结果"), "text/event-stream")
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(support::complete("保留公开命令及失败退出码"))
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "[compaction]\nmethod_order=['soft']\nkeep_recent_tokens=1\n[tools.bash]\nallow_patterns=[]\n").await;
    let id = harness.session(SessionMode::Plan).await;
    let before = harness
        .client
        .call::<SessionContext>(SessionContextParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    let private = bash(
        &harness,
        &id,
        "!!printf PRIVATE_STDOUT; printf PRIVATE_STDERR >&2; printf done > private-marker",
    )
    .await;
    assert_eq!(private.status, ToolStatus::Completed);
    assert!(private.result.content.contains("PRIVATE_STDOUT"));
    assert!(private.result.content.contains("PRIVATE_STDERR"));
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("work/private-marker")).unwrap(),
        "done"
    );
    let after = harness
        .client
        .call::<SessionContext>(SessionContextParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(before, after, "!! 不消耗模型上下文预算");
    let public = bash(
        &harness,
        &id,
        "!pwd; printf PUBLIC_STDOUT; printf PUBLIC_STDERR >&2; exit 7",
    )
    .await;
    assert_eq!(public.status, ToolStatus::Error);
    assert!(public.result.content.contains("PUBLIC_STDOUT"));
    assert!(public.result.content.contains("PUBLIC_STDERR"));
    assert!(public.result.content.contains("exit 7"));
    assert!(
        public
            .result
            .content
            .contains(&harness.root.path().join("work").display().to_string())
    );
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "手动命令不得触发模型请求"
    );

    harness
        .client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    let loaded = harness.load(&id).await;
    assert!(loaded.entries.iter().any(|entry| {
        entry
            .bash
            .as_ref()
            .is_some_and(|command| command.exclude_from_context)
            && entry.message.text().contains("PRIVATE_STDOUT")
    }));
    assert!(loaded.entries.iter().any(|entry| {
        entry
            .bash
            .as_ref()
            .is_some_and(|command| !command.exclude_from_context)
            && entry
                .tool
                .as_ref()
                .is_some_and(|tool| tool.status == ToolStatus::Error)
    }));
    harness.prompt(&id, "说明公开结果").await;
    harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    harness.prompt(&id, "压缩后继续").await;
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            !body.contains("PRIVATE_STDOUT"),
            "!! 正文不得进入生成或压缩请求"
        );
        assert!(
            !body.contains("PRIVATE_STDERR"),
            "!! 错误输出不得进入生成或压缩请求"
        );
        assert!(
            !body.contains("private-marker"),
            "!! 命令本身也不得进入上下文"
        );
    }
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(first["messages"].to_string().contains("PUBLIC_STDOUT"));
    assert!(first["messages"].to_string().contains("PUBLIC_STDERR"));
    assert!(first["messages"].to_string().contains("exit 7"));
    assert!(
        !first["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"),
        "不得伪造 agent 的工具调用"
    );
    harness.close().await;
}

#[tokio::test]
async fn empty_manual_commands_release_the_session_for_next_execution() {
    let harness = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let id = harness.session(SessionMode::Ask).await;
    for input in ["!", "!!", "!  \n  "] {
        let error = harness
            .client
            .call::<SessionBash>(BashParams {
                session_id: id.clone(),
                command: BashCommand::parse(input).unwrap(),
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            Some(moc_protocol::RpcErrorCode::InvalidParams.code())
        );
    }
    let result = bash(&harness, &id, "!printf enabled-for-user").await;
    assert_eq!(result.result.content, "enabled-for-user");
    assert_eq!(result.status, ToolStatus::Completed);
    harness.close().await;
}

#[tokio::test]
async fn disabled_jobs_keep_slow_commands_in_the_foreground() {
    let upstream = MockServer::start().await;
    let streams = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = Arc::clone(&streams);
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] != true {
                return ResponseTemplate::new(200).set_body_json(support::complete("摘要"));
            }
            let mut streams = recorder.lock();
            let first = streams.is_empty();
            streams.push(body.to_string());
            drop(streams);
            let response = if first {
                support::tool_stream(vec![(
                    "call_1",
                    "bash",
                    serde_json::json!({"command": "printf foreground-hi"}),
                )])
            } else {
                support::text_stream("完成")
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[tools.bash]\nforeground_after_secs=0\n[tools.job]\nenabled=false\n",
    )
    .await;
    let id = harness.session(SessionMode::Normal).await;
    harness.prompt(&id, "运行命令").await;
    let follow_up = {
        let streams = streams.lock();
        streams.last().cloned().expect("工具结果会回灌给模型")
    };
    assert!(
        follow_up.contains("foreground-hi"),
        "命令输出应留在前台结果里：{follow_up}"
    );
    assert!(
        !follow_up.contains("exceeded the foreground threshold"),
        "job 关闭时超时命令不得自动后台化：{follow_up}"
    );
    harness.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_manual_bash_reaps_process_and_unlocks_session() {
    let harness = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let id = harness.session(SessionMode::Normal).await;
    let client = harness.client.clone();
    let session_id = id.clone();
    let mut events = harness.events.subscribe();
    let gate = harness.root.path().join("work/gate");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    let execution = tokio::spawn(async move {
        client
            .call::<SessionBash>(BashParams {
                session_id,
                command: BashCommand::parse("!!printf '%s' $$ > manual.pid.pending; mv manual.pid.pending manual.pid; read -r input < gate").unwrap(),
            })
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                SessionUpdate::BashStarted { .. }
            ) {
                break;
            }
        }
        while !harness.root.path().join("work/manual.pid").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pid: i32 = std::fs::read_to_string(harness.root.path().join("work/manual.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let rejected = harness
        .client
        .call::<SessionBash>(BashParams {
            session_id: id.clone(),
            command: BashCommand::parse("!printf should-not-run").unwrap(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        rejected.code(),
        Some(moc_protocol::RpcErrorCode::SessionBusy.code())
    );
    assert!(
        harness
            .client
            .call::<SessionCancel>(SessionCancelParams {
                session_id: id.clone()
            })
            .await
            .unwrap()
            .cancelled
    );
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, ToolStatus::Cancelled);
    // SAFETY: 信号 0 只检查本测试刚取消的 shell 进程。
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        bash(&harness, &id, "!printf ready-again")
            .await
            .result
            .content,
        "ready-again"
    );
    harness.close().await;
}
