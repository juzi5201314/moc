//! bash 工具：前台、后台和 PTY 命令，输出写入有大小上限的日志。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::methods::{JobLifecycle, JobOrigin};
use serde::Deserialize;
use serde_json::Value;

use super::job::ForegroundEnd;
use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};

mod builtin_policy;
pub(crate) use builtin_policy::{check as prefer_builtin_check, render as render_prefer_builtin};
mod deny;
pub(crate) use deny::denied_rule;
mod group;
pub(crate) use super::job_log::JobLog;
pub(crate) use group::ProcessGroup;
mod process;
pub(crate) use process::{Process, ProcessExit};
pub(crate) mod syntax;
pub(crate) mod urlfs;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum BashLifecycle {
    #[default]
    Monitored,
    Persistent,
}

/// `description` 参数的模型可见说明；与 dsh 的 bash 工具保持同一前端卡片契约。
const DESCRIPTION_PARAM: &str = "Clear, concise description of what this command does in active voice, 5-10 words (shown in the UI). Examples: \"ls\" → \"List files in current directory\"; \"git status\" → \"Show working tree status\"; \"npm install\" → \"Install package dependencies\".";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    /// 前端卡片用的一句话说明；模型调用必须提供，用户手动执行的命令没有。
    description: Option<String>,
    command: String,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    timeout: Option<u64>,
    lifecycle: Option<BashLifecycle>,
    pty: Option<bool>,
    stdin: Option<String>,
    #[serde(default, rename = "async")]
    asynchronous: bool,
    /// 模型申请更宽的执行模式；必须与 `justification` 同时出现。
    sandbox_permissions: Option<moc_protocol::SessionPermission>,
    justification: Option<String>,
    /// 只在 service/prompt/tools.rs 的 prefer_builtin 策略检查里通过 JSON 读取。
    #[serde(default)]
    #[allow(dead_code)]
    force_bash: bool,
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
    if let Some(timeout) = args.timeout
        && !(1..=3600).contains(&timeout)
    {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "timeout must be between 1 and 3600 seconds",
        ));
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

fn validate_escalation(
    args: &BashArgs,
    current: moc_protocol::SessionPermission,
) -> Result<(), ToolError> {
    match (&args.sandbox_permissions, &args.justification) {
        (None, None) => Ok(()),
        (Some(_), None) | (None, Some(_)) => Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "sandbox_permissions and justification must be provided together",
        )),
        (Some(target), Some(justification)) => {
            if *target == moc_protocol::SessionPermission::WorkspaceOnly
                || target.width() < current.width()
            {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "sandbox_permissions must name workspace or full, never a narrower mode",
                ));
            }
            let justification = justification.trim();
            if justification.is_empty() {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "justification must explain why the wider mode is needed",
                ));
            }
            if justification.len() > 500 {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "justification must be at most 500 bytes",
                ));
            }
            Ok(())
        }
    }
}

/// 模型为前端卡片提供的一句话说明；目前只校验非空，展示留给后续 UI 消费。
fn validate_description(args: &BashArgs) -> Result<(), ToolError> {
    if args
        .description
        .as_deref()
        .is_none_or(|description| description.trim().is_empty())
    {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "description must be a non-empty string",
        ));
    }
    Ok(())
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command in the platform shell; see the environment section for the active shell and dialect."
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        let description = match crate::shell::detect() {
            Some(shell) => format!(
                "Run a shell command. The active shell is {} ({}); write {} commands. {BACKGROUND_NOTE}",
                shell.label(),
                shell.program.display(),
                crate::shell::dialect(shell),
            ),
            None => format!(
                "Run a shell command. No supported shell was found on this platform. {BACKGROUND_NOTE}"
            ),
        };
        ToolDef::new(
            "bash",
            schema(
                serde_json::json!({
                    "description":{"type":"string","description":DESCRIPTION_PARAM},
                    "command":{"type":"string"},
                    "cwd":{"type":"string"},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "timeout":{"type":"integer","minimum":1,"maximum":3600,"description":"Seconds before the process group is killed"},
                    "lifecycle":{"type":"string","enum":["monitored","persistent"]},
                    "pty":{"type":"boolean"},
                    "stdin":{"type":"string"},
                    "async":{"type":"boolean"},
                    "sandbox_permissions":{
                        "type":"string",
                        "enum":["workspace","full"],
                        "description":"Request a wider sandbox mode after a denial. Must be paired with justification and is only honored with user approval."
                    },
                    "justification":{
                        "type":"string",
                        "maxLength":500,
                        "description":"Why this command needs the wider mode; shown to the user in the approval prompt."
                    },
                    "force_bash":{"type":"boolean","description":"Skip the E_PREFER_BUILTIN check for this call. Default false; only when no built-in tool can do the job."}
                }),
                &["description", "command"],
            ),
        )
        .with_description(description)
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: BashArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            validate_escalation(&args, ctx.permission)?;
            if args.command.trim().is_empty() {
                return Err(ToolError::new("E_BAD_PAYLOAD", "command must not be empty"));
            }
            validate_description(&args)?;
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
            let job_id = next_job_id(&ctx.paths, &ctx.state).await?;
            let output_path = ctx.paths.jobs().join(format!("{job_id}.log"));
            let log_capacity = match lifecycle {
                BashLifecycle::Monitored => ctx.config.tools.bash.max_output_bytes,
                BashLifecycle::Persistent => ctx.config.tools.bash.persistent_log_bytes,
            };
            let log = JobLog::create(output_path.clone(), log_capacity)
                .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
            let permit = if asynchronous {
                Some(ctx.jobs.reserve(&ctx.config.tools.job, &ctx.cancel).await?)
            } else {
                None
            };
            let spec = super::job::JobSpec {
                id: job_id.clone(),
                command: args.command.clone(),
                description: args.description.clone(),
                log: log.clone(),
                lifecycle: match lifecycle {
                    BashLifecycle::Monitored => JobLifecycle::Monitored,
                    BashLifecycle::Persistent => JobLifecycle::Persistent,
                },
            };
            let limits = ctx.config.tools.bash.limits.resolve();
            let process = Process::spawn(
                args,
                cwd,
                timeout,
                log.clone(),
                ctx.sandbox_policy(),
                limits,
            )
            .await?;
            if let Some(permit) = permit {
                ctx.jobs.start_background(ctx, spec, process, permit)?;
                let uri = ctx
                    .artifact_store()
                    .uri_for_path(&output_path)
                    .map_err(|error| ToolError::new(error.code, error.message))?;
                return Ok(ToolOutcome::success(format!(
                    "Background {lifecycle:?} job {job_id} started; completion is reported automatically; log: {uri}"
                )));
            }
            let after = ctx.config.tools.bash.foreground_after_secs;
            let max_running = ctx.config.tools.job.max_running;
            // job 工具关闭时模型无法管理后台任务，命令只能在前台跑完。
            let background_after = ctx
                .config
                .tools
                .job
                .enabled
                .then(|| (Duration::from_secs(after), max_running));
            let mut foreground = ctx.jobs.start_foreground(ctx, spec, process)?;
            match foreground.wait(&ctx.cancel, background_after).await {
                ForegroundEnd::Backgrounded => Ok(backgrounded_outcome(ctx, &job_id, &log, after)),
                ForegroundEnd::Finished(result) => {
                    let mut outcome = exit_outcome(
                        output_path,
                        result?,
                        timeout.map_or(0, |value| value.as_secs()),
                        ctx.permission,
                        ctx.config.sandbox.on_denied,
                        ctx.config.sandbox.allow_unsandboxed,
                        limits,
                    );
                    if foreground.slots_full
                        && let ToolOutput::File { suffix, .. } = &mut outcome.content
                    {
                        suffix.push_str(&format!(
                            "\n\n[jobs: still running after {after}s but all {max_running} background slots in this session were busy, so it stayed in the foreground; kill or wait for jobs you no longer need]"
                        ));
                    }
                    Ok(outcome)
                }
            }
        })
    }
}

/// 工具描述里关于自动转后台的说明：模型应当预先知道长命令不会一直阻塞。
const BACKGROUND_NOTE: &str = "A monitored command still running after the foreground threshold (tools.bash.foreground_after_secs) moves to the background as a job and the call returns early; its completion is reported to you automatically, so do not poll it. To watch a running job, use `job wait`: it returns as soon as the job finishes or produces output you have not seen, so prefer it over repeated `job output` polls. Prefer fast, narrow commands (one package, one test, a filtered path) and non-interactive flags; start known-long builds or test suites with async=true. Use async=true with lifecycle=persistent only for long-lived services; persistent jobs never complete on their own.";

/// 转入后台时附在结果里的输出尾部行数。
const BACKGROUNDED_TAIL_LINES: usize = 20;

/// 前台命令转入后台时的结果：交代原因、已有输出和下一步怎样避免同样慢。
fn backgrounded_outcome(ctx: &ToolCtx, job_id: &str, log: &JobLog, after: u64) -> ToolOutcome {
    let info = ctx.jobs.info(job_id);
    let lead = match info.as_ref().map(|info| info.origin) {
        Some(JobOrigin::UserBackground) => {
            format!("The user moved this command to the background as monitored job {job_id}")
        }
        _ => format!(
            "Command was still running after {after}s and moved to the background as monitored job {job_id}"
        ),
    };
    let uri = info.map_or_else(|| log.path().display().to_string(), |info| info.log_uri);
    let (tail, elided, seen) = log
        .read_tail_and_seen(BACKGROUNDED_TAIL_LINES)
        .unwrap_or_else(|error| (format!("[E_JOB_LOG] failed to read log: {error}"), false, 0));
    // 预览尾部即「模型已见」：游标推进到 seen，之后 job wait/output 只报新输出。
    ctx.jobs.mark_output_read(job_id, seen);
    let tail = if tail.trim().is_empty() {
        "(no output yet)".to_owned()
    } else if elided {
        format!("[the middle of the output was dropped]\n{tail}")
    } else {
        tail
    };
    ToolOutcome::success(format!(
        "{lead}. Its completion is reported automatically; continue with other work instead of polling with job wait.\nOutput so far:\n{tail}\nLog: {uri}\nIf this command is routinely slow, make the next one cheaper: narrow its scope, avoid rebuilding or refetching what is already done, use non-interactive flags, or start it with async=true from the outset."
    ))
}

/// 用户手动执行命令：用户输入本身就是授权，不受模型工具开关、模式限制和自动转后台影响。
pub(crate) async fn run_manual(
    command: String,
    cwd: PathBuf,
    paths: &crate::session::store::SessionPaths,
    state: &tokio::sync::Mutex<crate::session::state::SessionState>,
    skills: &crate::skills::SkillIndex,
    config: &crate::config::BashConfig,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ToolOutcome, ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError::new("E_CANCELLED", "命令已取消"));
    }
    let policy = crate::sandbox::SandboxPolicy::full(cwd.clone());
    let command = match urlfs::prepare(paths, skills, &command)? {
        Some(prepared) => {
            if prepared.history_view {
                urlfs::refresh_history(paths, &policy)?;
            }
            prepared.command
        }
        None => command,
    };
    let job_id = next_job_id(paths, state).await?;
    let output_path = paths.jobs().join(format!("{job_id}.log"));
    let timeout = Duration::from_secs(config.monitored_timeout_secs.min(config.max_timeout_secs));
    let log = JobLog::create(output_path.clone(), config.max_output_bytes)
        .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
    let args = BashArgs {
        description: None,
        command,
        cwd: None,
        env: None,
        timeout: Some(timeout.as_secs()),
        lifecycle: Some(BashLifecycle::Monitored),
        pty: Some(false),
        stdin: None,
        asynchronous: false,
        sandbox_permissions: None,
        justification: None,
        force_bash: false,
    };
    // 用户手打的命令不受资源限制：输入本身即授权。
    let mut process = Process::spawn(args, cwd.clone(), Some(timeout), log, policy, None).await?;
    tokio::select! {
        biased;
        result = &mut process.completion => Ok(exit_outcome(
            output_path,
            result?,
            timeout.as_secs(),
            moc_protocol::SessionPermission::Full,
            crate::config::OnDenied::Ask,
            true,
            None,
        )),
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

fn exit_outcome(
    output_path: PathBuf,
    result: ProcessExit,
    timeout: u64,
    permission: moc_protocol::SessionPermission,
    on_denied: crate::config::OnDenied,
    allow_unsandboxed: bool,
    limits: Option<crate::limits::BashLimits>,
) -> ToolOutcome {
    let mut suffix = render_exit(
        result.code.unwrap_or(-1),
        result.timed_out,
        result.killed,
        timeout,
    );
    if result.sandbox_denied
        && result.code != Some(0)
        && permission != moc_protocol::SessionPermission::Full
    {
        suffix.push_str(&format!(
            "\n\n[sandbox: file access denied under {} mode]",
            permission.as_str()
        ));
        if allow_unsandboxed && on_denied == crate::config::OnDenied::Ask {
            suffix.push_str(
                "\n[sandbox: escalation available — retry this exact command once with sandbox_permissions (the narrowest wider mode that suffices) + justification; the user is asked]",
            );
        } else {
            suffix.push_str(
                "\n[sandbox: escalation disabled by policy; adjust sandbox settings or use a permitted path]",
            );
        }
    }
    if result.limits_failed {
        suffix.push_str(
            "\n\n[limits: the resource scope could not be created, so the command did not run; disable tools.bash.limits or check `systemd --user`]",
        );
    } else if result.code == Some(137)
        && cfg!(unix)
        && limits.is_some_and(|limits| limits.memory_max_bytes > 0)
    {
        suffix.push_str(
            "\n\n[limits: killed by the memory cap; raise tools.bash.limits.memory_max_bytes or lower build parallelism]",
        );
    }
    let mut outcome = ToolOutcome::success(ToolOutput::File {
        path: output_path,
        suffix,
    });
    outcome.is_error = result.timed_out || result.killed || result.code != Some(0);
    outcome.error_code = if result.timed_out {
        Some("E_TIMEOUT".into())
    } else if result.killed {
        Some("E_KILLED".into())
    } else if result.code != Some(0) {
        Some("E_BASH_EXIT".into())
    } else {
        None
    };
    outcome
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

fn render_exit(status: i32, timed_out: bool, killed: bool, timeout: u64) -> String {
    if timed_out {
        format!("\n(killed after {timeout}s timeout)")
    } else if killed {
        "\n(killed)".into()
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
    fn tool_definition_names_the_active_shell() {
        let definition = BashTool.def();
        let description = definition.description.expect("bash 工具必须有描述");
        let shell = crate::shell::detect().expect("测试环境必须有 shell");
        assert!(description.contains("The active shell is"));
        assert!(
            description.contains(shell.label()),
            "描述必须点名实际 shell：{description}"
        );
    }

    #[test]
    fn tool_definition_requires_a_ui_description() {
        let parameters = BashTool.def().parameters;
        assert_eq!(
            parameters["required"],
            serde_json::json!(["description", "command"])
        );
        let description = parameters["properties"]["description"]["description"]
            .as_str()
            .expect("description 参数必须有说明");
        assert!(description.contains("shown in the UI"));
    }

    #[test]
    fn description_must_be_non_empty() {
        let args: BashArgs =
            serde_json::from_value(serde_json::json!({"description":"List files","command":"ls"}))
                .unwrap();
        assert!(validate_description(&args).is_ok());
        let args: BashArgs =
            serde_json::from_value(serde_json::json!({"description":"  ","command":"ls"})).unwrap();
        assert!(validate_description(&args).is_err());
        let args: BashArgs = serde_json::from_value(serde_json::json!({"command":"ls"})).unwrap();
        assert!(validate_description(&args).is_err());
    }

    #[test]
    fn escalation_validates_target_against_the_execution_mode() {
        let mut args = BashArgs {
            description: Some("Build the project".into()),
            command: "cargo build".into(),
            cwd: None,
            env: None,
            timeout: None,
            lifecycle: None,
            pty: None,
            stdin: None,
            asynchronous: false,
            sandbox_permissions: Some(moc_protocol::SessionPermission::Full),
            justification: Some("cargo needs the network and host toolchain".into()),
            force_bash: false,
        };
        // 授权通过后执行上下文已经是目标模式；同宽必须放行，否则升级成功也会被工具拒绝。
        assert!(validate_escalation(&args, moc_protocol::SessionPermission::Full).is_ok());
        assert!(validate_escalation(&args, moc_protocol::SessionPermission::Workspace).is_ok());
        assert!(validate_escalation(&args, moc_protocol::SessionPermission::WorkspaceOnly).is_ok());
        args.sandbox_permissions = Some(moc_protocol::SessionPermission::WorkspaceOnly);
        assert!(validate_escalation(&args, moc_protocol::SessionPermission::Workspace).is_err());
        args.sandbox_permissions = Some(moc_protocol::SessionPermission::Workspace);
        assert!(validate_escalation(&args, moc_protocol::SessionPermission::WorkspaceOnly).is_ok());
        args.justification = None;
        assert!(validate_escalation(&args, moc_protocol::SessionPermission::Workspace).is_err());
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
