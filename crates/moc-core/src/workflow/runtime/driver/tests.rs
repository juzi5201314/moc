use super::*;
use crate::workflow::runtime::driver::world::capture_stream;
use crate::workflow::runtime::driver::world::glob_files;
use crate::workflow::runtime::driver::world::grep_files;
use crate::workflow::runtime::driver::world::parse_porcelain;
use crate::workflow::runtime::driver::world::run_capped;
use moc_ptc_protocol::wire::WorldReadKind;
use tokio::io::AsyncWriteExt;

/// WorldReadKind 的 serde 名必须被 world_read_impl 识别（防 Debug 小写化回归）。
#[tokio::test]
async fn world_read_wire_names_are_recognized() {
    let dir = tempfile::tempdir().unwrap();
    let kinds = [
        WorldReadKind::FilesGlob,
        WorldReadKind::FilesRead,
        WorldReadKind::FilesGrep,
        WorldReadKind::GitChangedFiles,
        WorldReadKind::GitDiff,
        WorldReadKind::GitStatus,
        WorldReadKind::GitLog,
    ];
    for kind in kinds {
        let op = serde_op_name(&kind);
        let outcome = world_read_impl(dir.path(), 1024, &op, &json!([])).await;
        if let DriverOutcome::Failed { message, .. } = &outcome {
            assert!(!message.contains("unknown world.read op"), "{op} 未被识别");
        }
    }
    assert_eq!(serde_op_name(&WorldReadKind::FilesGlob), "files_glob");
    assert_eq!(serde_op_name(&WorldReadKind::GitStatus), "git_status");
}

#[test]
fn workspace_file_rejects_escapes() {
    let dir = tempfile::tempdir().unwrap();
    assert!(workspace_file(dir.path(), "../x").is_err(), "../ 逃逸应拒");
    let outside = tempfile::tempdir().unwrap();
    let abs = outside.path().join("f.txt");
    std::fs::write(&abs, "x").unwrap();
    assert!(
        workspace_file(dir.path(), abs.to_str().unwrap()).is_err(),
        "绝对路径出界应拒"
    );
}

#[test]
fn glob_files_sorted_relative() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.txt"), "b").unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/c.txt"), "c").unwrap();
    let out = glob_files(dir.path(), "*.txt").unwrap();
    // `*` 跨分隔符（literal_separator=false），递归匹配且排序。
    assert_eq!(out, vec!["a.txt", "b.txt", "sub/c.txt"]);
    let out = glob_files(dir.path(), "b.*").unwrap();
    assert_eq!(out, vec!["b.txt"]);
}

#[test]
fn grep_files_glob_filter_and_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.ts"), "needle here\n").unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "needle too\n").unwrap();
    let out = grep_files(dir.path(), "needle", Some("*.ts")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["path"], "src/a.ts", "glob 过滤且相对路径");
    let out = grep_files(dir.path(), "needle", None).unwrap();
    assert_eq!(out.len(), 2);
}

#[test]
fn porcelain_parses_into_facade_status() {
    let text = "## main...origin/main\n M src/a.rs\nA  src/b.rs\n?? src/new.rs\nR  old.rs -> src/moved.rs\n";
    let status = parse_porcelain(text, "");
    assert_eq!(status["branch"], "main");
    assert_eq!(status["clean"], false);
    assert_eq!(status["staged"], json!(["src/b.rs", "src/moved.rs"]));
    assert_eq!(status["unstaged"], json!(["src/a.rs"]));
    assert_eq!(status["untracked"], json!(["src/new.rs"]));
    // 子目录工作区：prefix 去前缀。
    let status = parse_porcelain("## main\n?? sub/f.rs\n", "sub/");
    assert_eq!(status["untracked"], json!(["f.rs"]));
}

#[tokio::test]
async fn capture_stream_tail_and_discard() {
    // tail：按字节保留尾部。
    let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    let mut child = tokio::process::Command::new("cat")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&data).await.unwrap();
    let buf = capture_stream(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        OutputMode::Tail(64),
        usize::MAX,
    )
    .await
    .unwrap();
    assert_eq!(buf.len(), 64);
    assert_eq!(buf.as_slice(), &data[data.len() - 64..]);
    // discard：无管道 → 空。
    let buf = capture_stream(None, OutputMode::Discard, usize::MAX)
        .await
        .unwrap();
    assert!(buf.is_empty());
    assert!(matches!(
        OutputMode::of(&json!({"tail": 7})),
        OutputMode::Tail(7)
    ));
    assert!(matches!(
        OutputMode::of(&json!("discard")),
        OutputMode::Discard
    ));
    assert!(matches!(OutputMode::of(&Value::Null), OutputMode::Full));
}

/// Full 模式单流超限：立即 kill 返回超限 Failed，而不是干等到 E_WORLD_TIMEOUT。
#[tokio::test]
async fn world_run_full_overflow_kills_fast() {
    let start = std::time::Instant::now();
    let outcome = run_capped(
        std::ffi::OsStr::new("sh"),
        &[
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("yes | head -c 1048576"),
        ],
        &[],
        std::path::Path::new("/tmp"),
        60_000,
        &Value::Null,
        64,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let elapsed = start.elapsed();
    match outcome {
        DriverOutcome::Failed { code, message } => {
            assert_eq!(code, "E_WORLD", "超限不该是超时：{message}");
            assert!(message.contains("64"), "{message}");
        }
        other => panic!("超限应 Failed，实际 {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "超限应立即返回，实际耗时 {elapsed:?}"
    );
}

#[cfg(unix)]
async fn world_run_ready(path: &std::path::Path) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("子进程应报告就绪")
}

/// 同时覆盖仍在运行和已退出的直接子进程；孙进程始终持有 stdout/stderr。
#[cfg(unix)]
#[tokio::test]
async fn world_run_interruptions_reap_child_and_readers() {
    struct Descendant(libc::pid_t);
    impl Drop for Descendant {
        fn drop(&mut self) {
            // 测试无论成功或 panic 都清理自己启动的孙进程。
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    for cancelled in [false, true] {
        for child_exits in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let policy = crate::sandbox::SandboxPolicy::full(dir.path().to_path_buf());
            let cancel = tokio_util::sync::CancellationToken::new();
            let args = vec![
                "-c".into(),
                format!(
                    "sleep 60 & printf '%s %s' \"$$\" \"$!\" > ready.tmp; mv ready.tmp ready; {}",
                    if child_exits { "exit 0" } else { "wait" }
                ),
            ];
            let metrics = tokio::runtime::Handle::current().metrics();
            let tasks_before = metrics.num_alive_tasks();
            let run = world_run_impl(&policy, "sh", &args, 60_000, &Value::Null, &cancel);
            tokio::pin!(run);
            let ready = dir.path().join("ready");
            let pids = tokio::select! {
                outcome = &mut run => panic!("中断前不应完成：{outcome:?}"),
                pids = world_run_ready(&ready) => pids,
            };
            let pids: Vec<libc::pid_t> = pids
                .split_whitespace()
                .map(|pid| pid.parse().unwrap())
                .collect();
            let _descendant = Descendant(pids[1]);
            if child_exits {
                // 让 collect 先取得退出状态，确定中断发生在仅等管道 EOF 的路径上。
                tokio::select! {
                    outcome = &mut run => panic!("孙进程持有管道时不应完成：{outcome:?}"),
                    result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        while unsafe { libc::kill(pids[0], 0) } == 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    }) => result.expect("直接子进程应先退出"),
                }
                assert_eq!(unsafe { libc::kill(pids[0], 0) }, -1, "直接子进程应已退出");
            }
            if cancelled {
                cancel.cancel();
            } else {
                // 就绪后推进虚拟时钟，不靠短超时与进程启动速度竞赛。
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(60)).await;
                tokio::time::resume();
            }
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), &mut run)
                .await
                .expect("中断不应等待孙进程释放管道");
            let expected = if cancelled {
                "E_WORLD_CANCELLED"
            } else {
                "E_WORLD_TIMEOUT"
            };
            assert!(matches!(outcome, DriverOutcome::Failed { code, .. } if code == expected));
            assert_eq!(
                metrics.num_alive_tasks(),
                tasks_before,
                "两路读取任务必须已回收"
            );
            assert_eq!(
                unsafe { libc::kill(pids[0], 0) },
                -1,
                "直接子进程不能继续运行"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            // 已被 wait 回收的子进程不应留下可再次 waitpid 的僵尸。
            assert_eq!(
                unsafe { libc::waitpid(pids[0], std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn world_run_cancel_before_spawn_and_while_waiting_without_output() {
    let dir = tempfile::tempdir().unwrap();
    let policy = crate::sandbox::SandboxPolicy::full(dir.path().to_path_buf());
    for output in [Value::Null, json!({"tail": 64}), json!("discard")] {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        // 无效命令应被取消短路，而非尝试 spawn 后返回 E_WORLD。
        let outcome = world_run_impl(
            &policy,
            "missing-world-run-command",
            &[],
            60_000,
            &output,
            &cancel,
        )
        .await;
        assert!(
            matches!(outcome, DriverOutcome::Failed { code, .. } if code == "E_WORLD_CANCELLED")
        );

        let cancel = tokio_util::sync::CancellationToken::new();
        let args = vec![
            "-c".into(),
            "exec 1>&- 2>&-; echo $$ > ready.tmp; mv ready.tmp ready; exec sleep 60".into(),
        ];
        let run = world_run_impl(&policy, "sh", &args, 60_000, &output, &cancel);
        tokio::pin!(run);
        let ready = dir.path().join("ready");
        let pid = tokio::select! {
            outcome = &mut run => panic!("取消前不应完成：{outcome:?}"),
            pid = world_run_ready(&ready) => pid.trim().parse::<libc::pid_t>().unwrap(),
        };
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), &mut run)
            .await
            .expect("已无输出的子进程也应被取消");
        assert!(
            matches!(outcome, DriverOutcome::Failed { code, .. } if code == "E_WORLD_CANCELLED")
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "子进程不能继续运行");
        std::fs::remove_file(ready).unwrap();
    }
}

fn user_entry(id: &str) -> Entry {
    Entry::Message {
        id: id.into(),
        parent_id: None,
        at_ms: 0,
        message: moc_llm::Message::user_text("q"),
        tool: None,
        finish_reason: None,
        bash: None,
        agent: None,
        goal: None,
        reminder: None,
        agent_result: None,
        job_notice: None,
    }
}

fn assistant_entry(id: &str) -> Entry {
    Entry::Message {
        id: id.into(),
        parent_id: None,
        at_ms: 0,
        message: moc_llm::Message::assistant_text("a"),
        tool: None,
        finish_reason: None,
        bash: None,
        agent: None,
        goal: None,
        reminder: None,
        agent_result: None,
        job_notice: None,
    }
}

#[test]
fn transcript_prefix_cuts_at_user_turns() {
    // transcript：header + (user, assistant) × N。
    let mut entries = vec![user_entry("h")];
    for i in 0..3 {
        entries.push(user_entry(&format!("u{i}")));
        entries.push(assistant_entry(&format!("a{i}")));
    }
    // user_turns=2 → 保留到第 2 个 user 回合结束（header 始终跳过）。
    let out = transcript_prefix(&entries, 2);
    let users = out
        .iter()
        .filter(|e| matches!(e, Entry::Message { message, .. } if message.role == Role::User))
        .count();
    assert_eq!(users, 2, "只保留前 2 个 user 回合");
    assert_eq!(out.len(), 4);
    // user_turns=0 → 空前缀。
    assert!(transcript_prefix(&entries, 0).is_empty());
}

#[test]
fn resolve_actor_model_ordering() {
    let meta = |role_models: &[(&str, &str)], ref_map: &[(&str, &str)]| super::super::RunMeta {
        run_id: "r".into(),
        status: "running".into(),
        mode: "launch".into(),
        title: String::new(),
        created_at_ms: 0,
        finished_at_ms: None,
        resumed_from: None,
        args: Value::Null,
        commands: vec![],
        role_models: role_models
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        model_ref_map: ref_map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        subagent_model: Some("sub-m".into()),
        session_model: Some("sess-m".into()),
        result: None,
        error: None,
        stop_reason: None,
        reports: vec![],
        artifacts: vec![],
        holes: vec![],
        roles: vec![],
        phases: vec![],
        usage: Value::Null,
        plan: json!({
            "roles": [
                {"role_id": "routed", "model_source": {"kind": "routed", "refs": ["model#0"]}},
                {"role_id": "lit", "model_source": {"kind": "literal", "model": "lit-m"}},
            ],
            "model_refs": [{"ref_id": "model#0", "model_id": "ref-m"}],
        }),
    };
    // site#ord > site > persona.literal > plan > subagent > session。
    let m = meta(&[("a#2", "ord-m"), ("a", "site-m")], &[]);
    assert_eq!(resolve_actor_model(&m, "a", 2, &json!({})), "ord-m");
    assert_eq!(resolve_actor_model(&m, "a", 0, &json!({})), "site-m");
    // persona.model 字面量。
    assert_eq!(
        resolve_actor_model(&m, "b", 0, &json!({"model": "per-m"})),
        "per-m"
    );
    // persona.model ref → model_ref_map 重映射。
    let m2 = meta(&[], &[("ref-x", "mapped-m")]);
    assert_eq!(
        resolve_actor_model(
            &m2,
            "b",
            0,
            &json!({"model": {"model_id": "raw-m", "ref_id": "ref-x"}})
        ),
        "mapped-m"
    );
    // plan routed → model_ref_map；未映射 → plan.model_refs。
    let m3 = meta(&[], &[("model#0", "mapped2")]);
    assert_eq!(resolve_actor_model(&m3, "routed", 0, &json!({})), "mapped2");
    assert_eq!(resolve_actor_model(&m2, "routed", 0, &json!({})), "ref-m");
    // plan literal。
    assert_eq!(resolve_actor_model(&m2, "lit", 0, &json!({})), "lit-m");
    // inherit → subagent_model → session_model。
    assert_eq!(resolve_actor_model(&m2, "unknown", 0, &json!({})), "sub-m");
    let m4 = meta(&[], &[]);
    let mut m5 = m4.clone();
    m5.subagent_model = None;
    assert_eq!(resolve_actor_model(&m5, "unknown", 0, &json!({})), "sess-m");
}
