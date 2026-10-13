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
pub(crate) use policy::allowed_in_sandbox;
mod group;
pub(crate) use super::job_log::{JobLog, JobLogMode};
pub(crate) use group::ProcessGroup;
mod process;
pub(crate) use process::{Process, ProcessExit};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BashLifecycle {
    Monitored,
    Persistent,
}

impl Default for BashLifecycle {
    fn default() -> Self {
        Self::Monitored
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    timeout: Option<u64>,
    lifecycle: Option<BashLifecycle>,
    pty: Option<bool>,
    stdin: Option<String>,
    #[serde(default, rename = "async")]
    asynchronous: bool,
}

fn resolve_timeout(
    args: &BashArgs,
    lifecycle: BashLifecycle,
    config: &crate::config::BashConfig,
) -> Result<Option<Duration>, ToolError> {
    if lifecycle == BashLifecycle::Persistent && !args.asynchronous {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "persistent lifecycle requires async=true",
        ));
    }
    if lifecycle == BashLifecycle::Persistent && args.timeout.is_some() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "persistent lifecycle forbids timeout",
        ));
    }
    if let Some(timeout) = args.timeout {
        if !(1..=3600).contains(&timeout) {
            return Err(ToolError::new(
                "E_BAD_PAYLOAD",
                "timeout must be between 1 and 3600 seconds",
            ));
        }
    }
    Ok(match lifecycle {
        BashLifecycle::Monitored => Some(Duration::from_secs(
            args.timeout
                .unwrap_or(config.monitored_timeout_secs)
                .min(config.max_timeout_secs),
        )),
        BashLifecycle::Persistent => None,
    })
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command. Default lifecycle is monitored; use async=true,lifecycle=persistent for intentionally long-lived services. Persistent jobs never auto-complete. Use grep/find for file search."
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
                    "command":{"type":"string","description":"Shell command; use grep/find for file search"},
                    "cwd":{"type":"string"},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "timeout":{"type":"integer","minimum":1,"maximum":3600},
                    "lifecycle":{"type":"string","enum":["monitored","persistent"]},
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
            let lifecycle = args.lifecycle.unwrap_or_default();
            let timeout = resolve_timeout(&args, lifecycle, &ctx.config.tools.bash)?;
            let cwd = resolve_cwd(&ctx.cwd, args.cwd.as_deref())?;
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
            let log_mode = match lifecycle {
                BashLifecycle::Monitored => JobLogMode::Monitored,
                BashLifecycle::Persistent => JobLogMode::Persistent,
            };
            let log_capacity = match lifecycle {
                BashLifecycle::Monitored => ctx.config.tools.bash.max_output_bytes,
                BashLifecycle::Persistent => ctx.config.tools.bash.persistent_log_bytes,
            };
            let log = JobLog::create(output_path.clone(), log_mode, log_capacity)
                .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
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
                log.clone(),
                ctx.permission,
            )
            .await?;
            if asynchronous {
                ctx.jobs.start(
                    ctx.clone(),
                    job_id.clone(),
                    command,
                    output_path.clone(),
                    log,
                    process,
                    permit.expect("后台任务已预留名额"),
                    match lifecycle {
                        BashLifecycle::Monitored => super::job::JobLifecycle::Monitored,
                        BashLifecycle::Persistent => super::job::JobLifecycle::Persistent,
                    },
                    super::job::JobOrigin::Explicit,
                    timeout,
                )?;
                let uri = ctx
                    .artifact_store()
                    .uri_for_path(&output_path)
                    .map_err(|error| ToolError::new(error.code, error.message))?;
                return Ok(ToolOutcome::success(format!(
                    "Background {lifecycle:?} job {job_id} started; log: {uri}"
                )));
            }
            let result = wait_foreground(
                ctx,
                process,
                job_id,
                command,
                output_path.clone(),
                log,
                timeout,
            )
            .await?;
            let Some(result) = result else {
                let uri = ctx
                    .artifact_store()
                    .uri_for_path(&output_path)
                    .map_err(|error| ToolError::new(error.code, error.message))?;
                return Ok(ToolOutcome::success(format!(
                    "Command exceeded the foreground threshold; monitored job continues in the background; inspect output or kill it; log: {uri}"
                )));
            };
            Ok(exit_outcome(
                output_path,
                result,
                timeout.map_or(0, |value| value.as_secs()),
            ))
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
    let timeout = Duration::from_secs(config.monitored_timeout_secs.min(config.max_timeout_secs));
    let log = JobLog::create(
        output_path.clone(),
        JobLogMode::Monitored,
        config.max_output_bytes,
    )
    .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
    let args = BashArgs {
        command,
        cwd: None,
        env: None,
        timeout: Some(timeout.as_secs()),
        lifecycle: Some(BashLifecycle::Monitored),
        pty: Some(false),
        stdin: None,
        asynchronous: false,
    };
    let mut process = Process::spawn(
        args,
        cwd.clone(),
        cwd,
        Some(timeout),
        log,
        moc_protocol::SessionPermission::Full,
    )
    .await?;
    tokio::select! {
        biased;
        result = &mut process.completion => Ok(exit_outcome(output_path, result?, timeout.as_secs())),
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
    log: JobLog,
    timeout: Option<Duration>,
) -> Result<Option<ProcessExit>, ToolError> {
    enum Selection {
        Finished(Result<ProcessExit, ToolError>),
        Background,
        Cancelled,
    }
    let selection = tokio::select! {
        biased;
        result = &mut process.completion => Selection::Finished(result),
        _ = ctx.cancel.cancelled() => Selection::Cancelled,
        _ = tokio::time::sleep(Duration::from_secs(ctx.config.tools.bash.foreground_after_secs)) => Selection::Background,
    };
    match selection {
        Selection::Finished(result) => result.map(Some),
        Selection::Background => {
            if ctx.config.tools.job.enabled
                && let Some(permit) = ctx.jobs.slots.try_acquire(ctx.config.tools.job.max_running)
            {
                ctx.jobs.start(
                    ctx.clone(),
                    job_id,
                    command,
                    output_path,
                    log,
                    process,
                    permit,
                    super::job::JobLifecycle::Monitored,
                    super::job::JobOrigin::AutoBackground,
                    timeout,
                )?;
                Ok(None)
            } else {
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn lifecycle_defaults_to_monitored_and_parses_persistent() {
        let args: BashArgs = serde_json::from_value(serde_json::json!({"command":"echo ok"}))
            .expect("解析默认生命周期");
        assert_eq!(args.lifecycle.unwrap_or_default(), BashLifecycle::Monitored);
        let args: BashArgs = serde_json::from_value(
            serde_json::json!({"command":"echo ok","lifecycle":"persistent","async":true}),
        )
        .expect("解析 persistent 参数");
        assert_eq!(args.lifecycle, Some(BashLifecycle::Persistent));
        assert!(args.asynchronous);
    }

    #[test]
    fn resolve_timeout_bounds_monitored_and_rejects_invalid_combinations() {
        let config = crate::config::BashConfig::default();
        let resolve = |value: serde_json::Value| {
            let args: BashArgs = serde_json::from_value(value).expect("解析参数");
            resolve_timeout(&args, args.lifecycle.unwrap_or_default(), &config)
        };
        assert_eq!(
            resolve(serde_json::json!({"command":"echo ok"})).unwrap(),
            Some(Duration::from_secs(config.monitored_timeout_secs))
        );
        assert_eq!(
            resolve(serde_json::json!({"command":"echo ok","timeout":1})).unwrap(),
            Some(Duration::from_secs(1))
        );
        assert!(resolve(serde_json::json!({"command":"echo ok","timeout":0})).is_err());
        assert!(resolve(serde_json::json!({"command":"echo ok","timeout":3601})).is_err());
        assert_eq!(
            resolve(serde_json::json!({"command":"echo ok","async":true,"lifecycle":"persistent"}))
                .unwrap(),
            None
        );
        assert!(
            resolve(
                serde_json::json!({"command":"echo ok","timeout":60,"async":true,"lifecycle":"persistent"})
            )
            .is_err()
        );
        assert!(
            resolve(serde_json::json!({"command":"echo ok","lifecycle":"persistent"})).is_err()
        );
    }
}
