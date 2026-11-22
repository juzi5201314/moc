use super::OutputMode;
use crate::workflow::engine::DriverOutcome;
use crate::workflow::facade;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// `world.run` 的共享实现：eval 与 run driver 同一份（策略快照 + argv + 超时/取消 + 输出视图）。
pub(crate) async fn world_run_impl(
    policy: &crate::sandbox::SandboxPolicy,
    command: &str,
    args: &[String],
    timeout_ms: u64,
    output: &Value,
    cancel: &CancellationToken,
) -> DriverOutcome {
    let wrapped = match crate::sandbox::wrap_command_argv(command, args, &policy.workspace, policy)
    {
        Ok(w) => w,
        Err(e) => {
            return DriverOutcome::Failed {
                code: "E_SANDBOX".into(),
                message: e.to_string(),
            };
        }
    };
    run_capped(
        &wrapped.program,
        &wrapped.args,
        &wrapped.envs,
        &policy.workspace,
        timeout_ms,
        output,
        facade::WORLD_RUN_STREAM_MAX_BYTES,
        cancel,
    )
    .await
}

/// world.run 进程执行：固定 argv 直跑。cap 注入供测试（生产路径 = facade 上限）。
/// 超时覆盖「等待退出 + 两路流读完」整体：后台孙进程持有管道也不会挂死；
/// 单流超限立即 kill 子进程；超时或取消时终止读取任务并回收子进程。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_capped(
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
    envs: &[(std::ffi::OsString, std::ffi::OsString)],
    cwd: &std::path::Path,
    timeout_ms: u64,
    output: &Value,
    cap: usize,
    cancel: &CancellationToken,
) -> DriverOutcome {
    let cancelled = || DriverOutcome::Failed {
        code: "E_WORLD_CANCELLED".into(),
        message: "world.run cancelled".into(),
    };
    // 派发后、实际启动前就停止的 run 不应再产生进程副作用。
    if cancel.is_cancelled() {
        return cancelled();
    }
    // opts.output："full"（默认，单流 cap 封顶）/ "discard" / {tail:n}（每流保留尾 n 字节）。
    let mode = OutputMode::of(output);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .envs(envs.iter().cloned())
        .current_dir(cwd)
        .kill_on_drop(true);
    if matches!(mode, OutputMode::Discard) {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    } else {
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("failed to start command: {e}"),
            };
        }
    };
    let read_stream = |stream: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
                       mode: OutputMode| {
        tokio::spawn(async move { capture_stream(stream, mode, cap).await })
    };
    let mut out_task = read_stream(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        mode,
    );
    let mut err_task = read_stream(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        mode,
    );
    let mut status = None;
    let mut out_res = None;
    let mut err_res = None;
    let collect = async {
        loop {
            if status.is_some() && out_res.is_some() && err_res.is_some() {
                break;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(cancelled()),
                s = child.wait(), if status.is_none() => status = Some(s),
                r = &mut out_task, if out_res.is_none() => out_res = Some(r),
                r = &mut err_task, if err_res.is_none() => err_res = Some(r),
            }
            // 任一流超限 → 立即 kill：管道写端不会阻塞，其余读取收尾；
            // 退出状态仍由 wait 臂补取（kill 不保证已回收）。
            let overflowed =
                matches!(out_res, Some(Ok(Err(_)))) || matches!(err_res, Some(Ok(Err(_))));
            if overflowed && status.is_none() {
                let _ = child.start_kill();
            }
        }
        Ok(())
    };
    let failure =
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), collect).await {
            Ok(Ok(())) => None,
            Ok(Err(outcome)) => Some(outcome),
            Err(_) => Some(DriverOutcome::Failed {
                code: "E_WORLD_TIMEOUT".into(),
                message: format!("world.run timed out after {timeout_ms}ms"),
            }),
        };
    if let Some(outcome) = failure {
        // 孙进程可能仍持有管道写端：先终止两路读取，不能靠 kill 子进程等 EOF。
        out_task.abort();
        err_task.abort();
        let _ = child.start_kill();
        // 已从 select! 取得结果的 JoinHandle 不能再次 await。
        if out_res.is_none() {
            let _ = out_task.await;
        }
        if err_res.is_none() {
            let _ = err_task.await;
        }
        // kill_on_drop 只作兜底；显式 wait 确保返回前已回收子进程。
        let _ = child.wait().await;
        return outcome;
    }
    let status = match status {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("failed waiting for command: {e}"),
            };
        }
        None => unreachable!("collect 循环退出时三项必齐"),
    };
    let (stdout, stderr) = match (out_res, err_res) {
        (Some(Ok(Ok(o))), Some(Ok(Ok(e)))) => (o, e),
        _ => {
            return DriverOutcome::Failed {
                code: "E_WORLD".into(),
                message: format!("world.run output exceeded {cap} bytes"),
            };
        }
    };
    DriverOutcome::Completed(json!({
        "exitCode": status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    }))
}

/// `world.read` 的共享实现（files/git 白名单）。
pub(crate) async fn world_read_impl(
    cwd: &std::path::Path,
    cap: usize,
    op: &str,
    args: &Value,
) -> DriverOutcome {
    match op {
        "files_glob" => {
            let pattern = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            match glob_files(cwd, pattern) {
                Ok(paths) => DriverOutcome::Completed(json!(paths)),
                Err(e) => DriverOutcome::Failed {
                    code: "E_WORLD".into(),
                    message: e,
                },
            }
        }
        "files_read" => {
            let path = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            match workspace_file(cwd, path).and_then(|p| read_capped(&p, cap).map_err(fail_world)) {
                Ok(text) => DriverOutcome::Completed(json!(text)),
                Err(o) => o,
            }
        }
        "files_grep" => {
            // 第二参数是路径 glob 过滤（"*.ts"、"src/**"），不是路径。
            let pattern = args.get(0).and_then(|v| v.as_str()).unwrap_or("");
            let glob = args
                .get(1)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            match grep_files(cwd, pattern, glob) {
                Ok(matches) => DriverOutcome::Completed(json!(matches)),
                Err(e) => DriverOutcome::Failed {
                    code: "E_WORLD".into(),
                    message: e,
                },
            }
        }
        "git_changed_files" | "git_diff" | "git_status" | "git_log" => {
            git_impl(cwd, op, args).await
        }
        _ => DriverOutcome::Failed {
            code: "E_WORLD".into(),
            message: format!("unknown world.read op {op}"),
        },
    }
}

pub(super) fn fail_world(message: String) -> DriverOutcome {
    DriverOutcome::Failed {
        code: "E_WORLD".into(),
        message,
    }
}

/// 工作区路径围栏：一律工作区相对；逃逸/绝对路径出界 → Failed。
pub(super) fn workspace_file(
    cwd: &std::path::Path,
    value: &str,
) -> Result<std::path::PathBuf, DriverOutcome> {
    let resolved =
        crate::tools::resolve_tool_path(cwd, value).map_err(|e| fail_world(e.to_string()))?;
    if crate::tools::workspace_escape(cwd, &resolved).is_some() {
        return Err(fail_world(format!("{value} is outside the workspace")));
    }
    Ok(resolved)
}

/// git base 校验：非空、不含 ".."、不得以 '-' 开头（防 `--output=` 选项注入）。
pub(super) fn valid_base(base: &str) -> bool {
    !base.is_empty() && !base.contains("..") && !base.starts_with('-')
}

/// 执行 git（固定 argv，不经 shell）；非零退出 → Failed（catchable）。
pub(super) async fn git_stdout(
    cwd: &std::path::Path,
    argv: &[&str],
) -> Result<String, DriverOutcome> {
    let out = tokio::process::Command::new("git")
        .args(argv)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| DriverOutcome::Failed {
            code: "E_GIT".into(),
            message: format!("failed to run git: {e}"),
        })?;
    if !out.status.success() {
        return Err(DriverOutcome::Failed {
            code: "E_GIT".into(),
            message: format!(
                "git {} exited {}: {}",
                argv.first().unwrap_or(&""),
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// git 白名单子命令（固定参数，不经 shell）。
pub(super) async fn git_impl(cwd: &std::path::Path, op: &str, args: &Value) -> DriverOutcome {
    let base = args.get(0).and_then(|a| a.as_str());
    if let Some(b) = base
        && !valid_base(b)
    {
        return fail_world(format!("invalid git base \"{b}\""));
    }
    match op {
        "git_changed_files" => {
            let mut files: Vec<String> = Vec::new();
            match base {
                Some(b) => {
                    match git_stdout(cwd, &["diff", "--name-only", "--relative", b, "--", "."])
                        .await
                    {
                        Ok(out) => files.extend(out.lines().map(str::to_string)),
                        Err(o) => return o,
                    }
                }
                None => {
                    match git_stdout(
                        cwd,
                        &["diff", "--name-only", "--relative", "HEAD", "--", "."],
                    )
                    .await
                    {
                        Ok(out) => files.extend(out.lines().map(str::to_string)),
                        Err(o) => return o,
                    }
                    match git_stdout(
                        cwd,
                        &["ls-files", "--others", "--exclude-standard", "--", "."],
                    )
                    .await
                    {
                        Ok(out) => files.extend(out.lines().map(str::to_string)),
                        Err(o) => return o,
                    }
                }
            }
            files.retain(|f| !f.is_empty());
            files.sort();
            files.dedup();
            DriverOutcome::Completed(json!(files))
        }
        "git_diff" => {
            let root = match std::fs::canonicalize(cwd) {
                Ok(r) => r,
                Err(e) => return fail_world(e.to_string()),
            };
            let mut argv = vec![
                "diff",
                "--no-ext-diff",
                "--relative",
                base.unwrap_or("HEAD"),
            ];
            let mut rel = None;
            if let Some(p) = args
                .get(1)
                .and_then(|a| a.as_str())
                .filter(|s| !s.is_empty())
            {
                match workspace_file(cwd, p) {
                    Ok(resolved) => {
                        rel = Some(
                            resolved
                                .strip_prefix(&root)
                                .map(|r| r.display().to_string())
                                .unwrap_or_else(|_| p.to_string()),
                        );
                    }
                    Err(o) => return o,
                }
            }
            if let Some(rel) = &rel {
                argv.push("--");
                argv.push(rel);
            }
            match git_stdout(cwd, &argv).await {
                Ok(out) if out.len() > facade::GIT_DIFF_MAX_BYTES => fail_world(format!(
                    "git diff exceeds {} bytes",
                    facade::GIT_DIFF_MAX_BYTES
                )),
                Ok(out) => DriverOutcome::Completed(json!(out)),
                Err(o) => o,
            }
        }
        "git_status" => {
            let status = match git_stdout(cwd, &["status", "--porcelain=v1", "-b", "--", "."]).await
            {
                Ok(s) => s,
                Err(o) => return o,
            };
            let prefix = match git_stdout(cwd, &["rev-parse", "--show-prefix"]).await {
                Ok(p) => p.trim_end().to_string(),
                Err(o) => return o,
            };
            DriverOutcome::Completed(parse_porcelain(&status, &prefix))
        }
        "git_log" => {
            let n = args
                .get(0)
                .and_then(|a| a.as_u64())
                .unwrap_or(facade::GIT_LOG_DEFAULT as u64);
            if n > facade::GIT_LOG_MAX as u64 {
                return fail_world(format!("git log count exceeds {}", facade::GIT_LOG_MAX));
            }
            match git_stdout(
                cwd,
                &[
                    "log",
                    "-n",
                    &n.to_string(),
                    "--format=%H%x1f%s%x1f%an%x1f%aI",
                ],
            )
            .await
            {
                Ok(out) => {
                    let commits: Vec<Value> = out
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| {
                            let mut parts = l.split('\x1f');
                            json!({
                                "hash": parts.next().unwrap_or_default(),
                                "subject": parts.next().unwrap_or_default(),
                                "author": parts.next().unwrap_or_default(),
                                "date": parts.next().unwrap_or_default(),
                            })
                        })
                        .collect();
                    DriverOutcome::Completed(json!(commits))
                }
                Err(o) => o,
            }
        }
        _ => unreachable!(),
    }
}

/// `git status --porcelain=v1 -b` 输出 → facade GitStatus。
/// porcelain 路径是仓库根相对，`prefix`（rev-parse --show-prefix）去前缀成工作区相对。
pub(super) fn parse_porcelain(text: &str, prefix: &str) -> Value {
    let mut branch = None;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    for line in text.lines() {
        if let Some(b) = line.strip_prefix("## ") {
            let name = b.split("...").next().unwrap_or(b).trim();
            // 未生成分支（尚无提交）仍是分支；仅 detached（HEAD 行）缺省。
            let name = name.strip_prefix("No commits yet on ").unwrap_or(name);
            if !name.is_empty() && !name.starts_with("HEAD") {
                branch = Some(name.to_string());
            }
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let (x, y) = (line.as_bytes()[0], line.as_bytes()[1]);
        let raw = &line[3..];
        // rename "a -> b" 取 b；路径去工作区前缀，不在工作区内的跳过。
        let path = raw.rsplit(" -> ").next().unwrap_or(raw).trim_matches('"');
        let Some(path) = path
            .strip_prefix(prefix)
            .or(prefix.is_empty().then_some(path))
        else {
            continue;
        };
        let path = path.trim_matches('"').to_string();
        if x == b'?' && y == b'?' {
            untracked.push(json!(path));
        } else {
            if x != b' ' && x != b'?' {
                staged.push(json!(path));
            }
            if y != b' ' && y != b'?' {
                unstaged.push(json!(path));
            }
        }
    }
    let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();
    let mut status = json!({
        "clean": clean,
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
    });
    if let Some(b) = branch {
        status["branch"] = json!(b);
    }
    status
}

/// 流式捕获一条输出流；Full 超 cap → Err（调用方 kill + Failed）。
pub(super) async fn capture_stream(
    stream: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    mode: OutputMode,
    cap: usize,
) -> Result<Vec<u8>, ()> {
    use tokio::io::AsyncReadExt;
    let Some(mut stream) = stream else {
        return Ok(Vec::new());
    };
    let mut buf: std::collections::VecDeque<u8> = Default::default();
    let mut chunk = [0u8; 8192];
    let mut len = 0usize;
    loop {
        let n = stream.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            break;
        }
        match mode {
            OutputMode::Full => {
                len += n;
                if len > cap {
                    return Err(());
                }
                buf.extend(&chunk[..n]);
            }
            OutputMode::Tail(limit) => {
                buf.extend(&chunk[..n]);
                let excess = buf.len().saturating_sub(limit);
                buf.drain(..excess);
            }
            OutputMode::Discard => {}
        }
    }
    Ok(buf.into_iter().collect())
}

/// files.glob：工作区相对路径，排序；超 FILES_GLOB_MAX 拒绝（不截断）。
pub(super) fn glob_files(cwd: &std::path::Path, pattern: &str) -> Result<Vec<String>, String> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| e.to_string())?
        .compile_matcher();
    let mut out = Vec::new();
    for entry in ignore::Walk::new(cwd).filter_map(|e| e.ok()) {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(cwd)
            && glob.is_match(rel)
        {
            out.push(rel.display().to_string());
        }
    }
    if out.len() > facade::FILES_GLOB_MAX {
        return Err(format!(
            "files.glob matched more than {} files",
            facade::FILES_GLOB_MAX
        ));
    }
    out.sort();
    Ok(out)
}

/// files.read：只读前 cap 字节。
pub(super) fn read_capped(path: &std::path::Path, cap: usize) -> Result<String, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.take(cap as u64)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// files.grep：遍历工作区，glob 过滤相对路径；匹配/字节超限拒绝。
pub(super) fn grep_files(
    cwd: &std::path::Path,
    pattern: &str,
    glob: Option<&str>,
) -> Result<Vec<Value>, String> {
    use grep_matcher::Matcher;
    let re = grep_regex::RegexMatcher::new(pattern).map_err(|e| e.to_string())?;
    let filter = glob
        .map(|g| {
            globset::GlobBuilder::new(g)
                .literal_separator(false)
                .build()
                .map(|b| b.compile_matcher())
                .map_err(|e| e.to_string())
        })
        .transpose()?;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for entry in ignore::Walk::new(cwd).filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        if let Some(f) = &filter
            && !f.is_match(rel)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line.as_bytes()).unwrap_or(false) {
                let item = json!({
                    "path": rel.display().to_string(),
                    "line": i + 1,
                    "text": line,
                });
                bytes += serde_json::to_string(&item).map(|s| s.len()).unwrap_or(0);
                out.push(item);
                if out.len() > facade::FILES_GREP_MAX_MATCHES
                    || bytes > facade::FILES_GREP_MAX_BYTES
                {
                    return Err(format!(
                        "files.grep exceeded the cap ({} matches / {} bytes)",
                        facade::FILES_GREP_MAX_MATCHES,
                        facade::FILES_GREP_MAX_BYTES
                    ));
                }
            }
        }
    }
    Ok(out)
}
