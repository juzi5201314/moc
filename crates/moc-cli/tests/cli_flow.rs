//! 通过真实 moc 二进制与本地固定上游验证终端行为。
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::AsyncWriteExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

struct Project {
    root: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}
impl Project {
    fn new(url: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("config.toml"), format!("[models.roles]\ndefault='p/gpt-test:off'\nsmol='p/gpt-test:off'\nslow='p/gpt-test:off'\nplan='p/gpt-test:off'\n[providers.p]\napi='openai_chat'\nbase_url='{url}/v1'\napi_key=''\nretry={{max_attempts=1}}\n[providers.p.models.gpt-test]\ncontext_window=1000000\n[context_files]\nenabled=false\n[workspace]\nwatch=false\n[compaction]\nmethod_order=['soft']\n")).unwrap();
        Self { root, home, cwd }
    }
    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_moc"));
        command
            .env("MOC_HOME", &self.home)
            .args(["--cwd"])
            .arg(&self.cwd)
            .arg("--session-dir")
            .arg(self.root.path().join("sessions"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
    async fn run(&self, args: &[&str], input: &str) -> std::process::Output {
        let mut child = self.command().args(args).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }
}
fn text(value: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"r","choices":[{"index":0,"delta":{"role":"assistant","content":value},"finish_reason":"stop"}]})
    )
}
fn call(id: &str, name: &str, args: Value) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"id":"r","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
    )
}

#[tokio::test]
async fn print_runs_write_grep_read_edit_and_resume_without_notification_noise() {
    let upstream = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = counter.clone();
    Mock::given(method("POST")).respond_with(move |request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(!body["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "ask"));
        let response = match seen.fetch_add(1, Ordering::SeqCst) {
            0 => call("write", "write", json!({"path":"hello.py","content":"print('hello')\n# pending\n"})),
            1 => call("grep", "grep", json!({"pattern":"hello","glob":["*.py"]})),
            2 => call("read", "read", json!({"path":"hello.py"})),
            3 => {
                let result = body["messages"].as_array().unwrap().iter().find(|message| message["tool_call_id"] == "read").unwrap();
                let output = result["content"][0]["text"].as_str().unwrap();
                let anchors: Vec<_> = output.lines().filter_map(|line| line.split_once('│').map(|(anchor, _)| anchor)).collect();
                call("edit", "edit", json!({"path":"hello.py","edits":[[anchors[0],anchors[1],"print('hello moc')"]]}))
            }
            _ => text("任务完成"),
        };
        ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
    }).mount(&upstream).await;
    let project = Project::new(&upstream.uri());
    let output = project.run(&["--print"], "创建并修改 hello.py").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "任务完成\n");
    assert_eq!(
        std::fs::read_to_string(project.cwd.join("hello.py")).unwrap(),
        "print('hello moc')\n"
    );
    let sessions = project.run(&["--list"], "").await;
    assert!(sessions.status.success());
    let listing = String::from_utf8(sessions.stdout).unwrap();
    let id = listing.split_whitespace().next().unwrap();
    let resumed = project.run(&["--resume", id, "--print", "继续"], "").await;
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let requests = upstream.received_requests().await.unwrap();
    let last: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert!(last.to_string().contains("创建并修改 hello.py"));
}

#[tokio::test]
async fn interactive_plan_approval_and_invalid_command_preserve_the_repl() {
    let upstream = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = counter.clone();
    Mock::given(method("POST")).respond_with(move |request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["stream"] != true { return ResponseTemplate::new(200).set_body_json(json!({"id":"s","choices":[{"index":0,"message":{"role":"assistant","content":"保留已批准计划"},"finish_reason":"stop"}]})); }
        let response = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            assert!(!body["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "edit"));
            call("plan", "submit_plan", json!({"title":"实施方案","markdown":"# 可审阅计划\n保留公开接口。"}))
        } else {
            assert!(body.to_string().contains("active-plan"));
            text("执行完成")
        };
        ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
    }).mount(&upstream).await;
    let project = Project::new(&upstream.uri());
    let output = project
        .run(
            &[],
            "/mode invalid\n/plan 设计接口\n/approve new\n/mode ask\n/compact\n/sessions\n/quit\n",
        )
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("# 可审阅计划"));
    assert!(stdout.contains("执行完成"));
    assert!(stdout.contains("上下文压缩"));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("模式必须是")
    );
}

async fn wait_file(path: &Path) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("等待子进程明确写出启动标记");
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_cancels_a_running_shell_and_repl_still_accepts_quit() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            call(
                "shell",
                "bash",
                json!({"command":"printf '%s' $$ > shell.pid; exec sleep 300","timeout":0}),
            ),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    let project = Project::new(&upstream.uri());
    let mut child = project.command().arg("开始执行").spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let pid_file = project.cwd.join("shell.pid");
    wait_file(&pid_file).await;
    let shell_pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    let pid = child.id().unwrap() as i32;
    // SAFETY: PID 来自仍由本测试持有的 Child；SIGINT 仅发给该 CLI。
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    input.write_all(b"/quit\n").await.unwrap();
    drop(input);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // SAFETY: 信号 0 仅查询刚刚退出的子进程是否仍存在。
    assert_eq!(unsafe { libc::kill(shell_pid, 0) }, -1);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Cancelled")
    );
}
