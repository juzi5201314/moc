//! 有界日志输出的前台、后台与 PTY 命令。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use grep_matcher::Matcher;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};

mod policy;
pub(crate) use policy::{allowed_in_sandbox, reject_unscoped_scan};
mod group;
pub(crate) use group::ProcessGroup;
mod process;
pub(crate) use process::{Process, ProcessExit};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    timeout: Option<u64>,
    pty: Option<bool>,
    stdin: Option<String>,
    #[serde(default, rename = "async")]
    asynchronous: bool,
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run builds, tests, formatters, version-control commands, and other commands that require a shell. Do not use it to read, search, or edit workspace files; use read, grep/find, and edit/write. Runs in the foreground by default; async=true returns a job_id, and pty=true enables an interactive terminal. Dangerous commands are rejected even with yolo."
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "bash",
            schema(
                serde_json::json!({
                    "command":{"type":"string","description":"Shell command to run; do not use cat, grep, rg, find, sed, awk, or interpreters here to read, search, or edit workspace files"},
                    "cwd":{"type":"string"},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "timeout":{"type":"integer","minimum":1,"maximum":3600},
                    "pty":{"type":"boolean"},
                    "stdin":{"type":"string"},
                    "async":{"type":"boolean"}
                }),
                &["command"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: BashArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if args.command.trim().is_empty() {
                return Err(ToolError::new("E_BAD_PAYLOAD", "command must not be empty"));
            }
            let cwd = resolve_cwd(&ctx.cwd, args.cwd.as_deref())?;
            crate::tools::bash::reject_unscoped_scan(&args.command, &cwd, &ctx.cwd)?;
            let timeout = args
                .timeout
                .unwrap_or(ctx.config.tools.bash.default_timeout_secs)
                .min(ctx.config.tools.bash.max_timeout_secs);
            if args.asynchronous && !ctx.config.tools.job.enabled {
                return Err(ToolError::new(
                    "E_TOOL_DISABLED",
                    "job is disabled; cannot create a background task",
                ));
            }
            let asynchronous = args.asynchronous;
            let command = args.command.clone();
            let job_id = next_job_id(&ctx.paths, &ctx.state).await?;
            let output_path = ctx.paths.jobs().join(format!("{job_id}.log"));
            // 显式后台任务在启动子进程之前预留名额，超限不会先执行命令。
            let permit = if asynchronous {
                Some(ctx.jobs.reserve(&ctx.config.tools.job, &ctx.cancel).await?)
            } else {
                None
            };
            let process = Process::spawn(
                args,
                cwd,
                ctx.cwd.clone(),
                timeout,
                ctx.config.tools.bash.max_output_bytes,
                output_path.clone(),
                ctx.permission,
            )
            .await?;
            if asynchronous {
                ctx.jobs.start(
                    ctx.clone(),
                    job_id.clone(),
                    command,
                    output_path.clone(),
                    process,
                    permit.expect("后台任务已预留名额"),
                )?;
                let uri = ctx
                    .artifact_store()
                    .uri_for_path(&output_path)
                    .map_err(|error| ToolError::new(error.code, error.message))?;
                return Ok(ToolOutcome::success(format!(
                    "Background job {job_id} started; log: {uri}"
                )));
            }
            let result =
                wait_foreground(ctx, process, job_id.clone(), command, output_path.clone()).await?;
            let Some(result) = result else {
                let uri = ctx
                    .artifact_store()
                    .uri_for_path(&output_path)
                    .map_err(|error| ToolError::new(error.code, error.message))?;
                return Ok(ToolOutcome::success(format!(
                    "Command exceeded the foreground threshold; job {job_id} continues in the background; log: {uri}"
                )));
            };
            Ok(exit_outcome(output_path, result, timeout))
        })
    }
}

/// 用户输入即显式授权：不经过模型工具开关、模式限制或自动后台化。
pub(crate) async fn run_manual(
    command: String,
    cwd: PathBuf,
    paths: &crate::session::store::SessionPaths,
    state: &tokio::sync::Mutex<crate::session::state::SessionState>,
    config: &crate::config::BashConfig,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ToolOutcome, ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError::new("E_CANCELLED", "命令已取消"));
    }
    let job_id = next_job_id(paths, state).await?;
    let output_path = paths.jobs().join(format!("{job_id}.log"));
    let timeout = config.default_timeout_secs.min(config.max_timeout_secs);
    let args = BashArgs {
        command,
        cwd: None,
        env: None,
        timeout: Some(timeout),
        pty: Some(false),
        stdin: None,
        asynchronous: false,
    };
    let mut process = Process::spawn(
        args,
        cwd.clone(),
        cwd,
        timeout,
        config.max_output_bytes,
        output_path.clone(),
        moc_protocol::SessionPermission::Full,
    )
    .await?;
    tokio::select! {
        biased;
        result = &mut process.completion => Ok(exit_outcome(output_path, result?, timeout)),
        _ = cancel.cancelled() => {
            process.group.kill()?;
            process.completion.await?;
            let mut outcome = ToolOutcome::success(ToolOutput::File {
                path: output_path,
                suffix: "\n（命令已取消）".into(),
            });
            outcome.is_error = true;
            outcome.status = Some(moc_protocol::ToolStatus::Cancelled);
            outcome.error_code = Some("E_CANCELLED".into());
            Ok(outcome)
        }
    }
}

fn exit_outcome(output_path: PathBuf, result: ProcessExit, timeout: u64) -> ToolOutcome {
    let mut outcome = ToolOutcome::success(ToolOutput::File {
        path: output_path,
        suffix: render_exit(
            result.code.unwrap_or(-1),
            result.timed_out,
            result.output_truncated,
            timeout,
        ),
    });
    outcome.is_error = result.timed_out || result.output_truncated || result.code != Some(0);
    outcome.error_code = if result.timed_out {
        Some("E_TIMEOUT".into())
    } else if result.output_truncated {
        Some("E_OUTPUT_LIMIT".into())
    } else if result.code != Some(0) {
        Some("E_BASH_EXIT".into())
    } else {
        None
    };
    outcome
}

async fn wait_foreground(
    ctx: &ToolCtx,
    mut process: Process,
    job_id: String,
    command: String,
    output_path: PathBuf,
) -> Result<Option<ProcessExit>, ToolError> {
    let auto = ctx.config.tools.bash.auto_background_ms;
    enum Selection {
        Finished(Result<ProcessExit, ToolError>),
        Background,
        Cancelled,
    }
    let selection = tokio::select! {
        biased;
        result = &mut process.completion => Selection::Finished(result),
        _ = ctx.cancel.cancelled() => Selection::Cancelled,
        _ = async {
            if auto > 0 && ctx.config.tools.job.enabled { tokio::time::sleep(Duration::from_millis(auto)).await; }
            else { std::future::pending::<()>().await; }
        } => Selection::Background,
    };
    match selection {
        Selection::Finished(result) => result.map(Some),
        Selection::Background => {
            if let Some(permit) = ctx.jobs.slots.try_acquire(ctx.config.tools.job.max_running) {
                ctx.jobs
                    .start(ctx.clone(), job_id, command, output_path, process, permit)?;
                Ok(None)
            } else {
                // 自动后台化已开始执行；名额用尽时继续前台等待，不额外创建后台任务。
                tokio::select! {
                    result = &mut process.completion => result.map(Some),
                    _ = ctx.cancel.cancelled() => {
                        process.group.kill()?;
                        let _ = process.completion.await;
                        Err(ToolError::new("E_CANCELLED", "command cancelled"))
                    }
                }
            }
        }
        Selection::Cancelled => {
            process.group.kill()?;
            if let Err(error) = process.completion.await {
                tracing::warn!(%error, "取消后回收命令失败");
            }
            Err(ToolError::new("E_CANCELLED", "command cancelled"))
        }
    }
}

pub(crate) fn reject_denied(patterns: &[String], command: &str) -> Result<(), ToolError> {
    for pattern in patterns {
        let matcher = grep_regex::RegexMatcher::new(pattern).map_err(|error| {
            ToolError::new(
                "E_CONFIG",
                format!("dangerous-command rule is invalid: {error}"),
            )
        })?;
        if matcher.is_match(command.as_bytes()).unwrap_or(false) {
            return Err(ToolError::new(
                "E_BASH_DENIED",
                "command matches a blocked execution rule",
            ));
        }
    }
    Ok(())
}

pub(crate) fn resolve_cwd(base: &Path, requested: Option<&str>) -> Result<PathBuf, ToolError> {
    let cwd = requested
        .map(Path::new)
        .map(|path| {
            if path.is_absolute() {
                path.to_owned()
            } else {
                base.join(path)
            }
        })
        .unwrap_or_else(|| base.to_owned());
    let canonical = std::fs::canonicalize(&cwd)
        .map_err(|error| ToolError::new("E_BASH_CWD", error.to_string()))?;
    if !canonical.is_dir() {
        return Err(ToolError::new("E_BASH_CWD", "cwd is not a directory"));
    }
    Ok(canonical)
}

async fn next_job_id(
    paths: &crate::session::store::SessionPaths,
    state: &tokio::sync::Mutex<crate::session::state::SessionState>,
) -> Result<String, ToolError> {
    let jobs = paths.jobs();
    tokio::fs::create_dir_all(&jobs)
        .await
        .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
    if state.lock().await.job_sequence == 0 {
        let mut files = tokio::fs::read_dir(&jobs)
            .await
            .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
        let mut sequence = 0;
        while let Some(file) = files
            .next_entry()
            .await
            .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?
        {
            let name = file.file_name();
            if let Some(number) = name
                .to_str()
                .and_then(|name| name.strip_prefix("job_"))
                .and_then(|name| name.strip_suffix(".log"))
                .and_then(|number| number.parse::<u64>().ok())
            {
                sequence = sequence.max(number);
            }
        }
        let mut state = state.lock().await;
        state.job_sequence = state.job_sequence.max(sequence);
    }
    loop {
        let id = {
            let mut state = state.lock().await;
            state.job_sequence = state
                .job_sequence
                .checked_add(1)
                .ok_or_else(|| ToolError::new("E_BASH_LOG", "job ID space is exhausted"))?;
            crate::ids::job_id(state.job_sequence)
        };
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(jobs.join(format!("{id}.log")))
            .await
        {
            Ok(_) => return Ok(id),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ToolError::new("E_BASH_LOG", error.to_string())),
        }
    }
}

fn render_exit(status: i32, timed_out: bool, output_truncated: bool, timeout: u64) -> String {
    if timed_out {
        format!("\n(killed after {timeout}s timeout)")
    } else if output_truncated {
        "\n(killed after output limit)".into()
    } else if status != 0 {
        format!("\n(exit {status})")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_patterns_preempt_all_execution_modes() {
        let patterns = vec![r"rm\s+-rf\s+/(\s|$)".into()];
        assert!(reject_denied(&patterns, "rm -rf /").is_err());
        assert!(reject_denied(&patterns, "printf safe").is_ok());
    }

    #[test]
    fn default_deny_patterns_are_valid_and_block_forkbombs() {
        let patterns = crate::config::BashConfig::default().deny_patterns;
        assert!(reject_denied(&patterns, ":(){ :|:& };:").is_err());
        assert!(reject_denied(&patterns, "SHUTDOWN /S").is_err());
        assert!(reject_denied(&patterns, "FORMAT C:").is_err());
        assert!(reject_denied(&patterns, "Remove-Item C:\\data -Recurse").is_err());
        assert!(reject_denied(&patterns, "printf safe").is_ok());
    }

    #[test]
    fn shell_cwd_must_exist_and_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_cwd(dir.path(), None).unwrap(), dir.path());
        assert!(resolve_cwd(dir.path(), Some("missing")).is_err());
    }
}
