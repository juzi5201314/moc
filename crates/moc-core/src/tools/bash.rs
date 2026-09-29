//! 有界日志输出的前台、后台与 PTY 命令。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use grep_matcher::Matcher;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

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
        "执行 shell 命令；默认前台，async=true 时返回 job_id。需要交互终端时设置 pty=true。危险命令即使 yolo 也会被拒绝。"
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
                    "command":{"type":"string","description":"stderr 合并到 stdout 的 shell 命令"},
                    "cwd":{"type":"string"},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "timeout":{"type":"integer","minimum":0,"maximum":3600},
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
                return Err(ToolError::new("E_BAD_PAYLOAD", "command 不能为空"));
            }
            reject_denied(&ctx.config.tools.bash.deny_patterns, &args.command)?;
            let cwd = resolve_cwd(&ctx.cwd, args.cwd.as_deref())?;
            let timeout = args
                .timeout
                .unwrap_or(ctx.config.tools.bash.default_timeout_secs)
                .min(ctx.config.tools.bash.max_timeout_secs);
            if args.asynchronous && !ctx.config.tools.job.enabled {
                return Err(ToolError::new(
                    "E_TOOL_DISABLED",
                    "job 工具关闭时不能创建后台任务",
                ));
            }
            let asynchronous = args.asynchronous;
            let command = args.command.clone();
            let job_id = next_job_id(ctx).await?;
            let output_path = ctx.paths.jobs().join(format!("{job_id}.log"));
            // 显式后台任务在启动子进程之前预留名额，超限不会先执行命令。
            let permit = if asynchronous {
                Some(ctx.jobs.reserve(&ctx.config.tools.job, &ctx.cancel).await?)
            } else {
                None
            };
            let process = Process::spawn(args, cwd, timeout, output_path.clone()).await?;
            if asynchronous {
                ctx.jobs.start(
                    ctx.clone(),
                    job_id.clone(),
                    command,
                    output_path.clone(),
                    process,
                    permit.expect("后台任务已预留名额"),
                )?;
                return Ok(ToolOutcome::success(format!(
                    "后台任务已启动：{job_id}，日志：{}",
                    output_path.display()
                )));
            }
            let result =
                wait_foreground(ctx, process, job_id.clone(), command, output_path.clone()).await?;
            let Some(result) = result else {
                return Ok(ToolOutcome::success(format!(
                    "命令运行超过自动转后台阈值，任务 {job_id} 继续执行，日志：{}",
                    output_path.display()
                )));
            };
            let output = read_bounded_file(
                &output_path,
                ctx.config.tools.output.head_bytes,
                ctx.config.tools.output.tail_bytes,
            )
            .await
            .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
            Ok(ToolOutcome {
                content: render_exit(
                    &output,
                    result.code.unwrap_or(-1),
                    result.timed_out,
                    (timeout > 0).then_some(Duration::from_secs(timeout)),
                ),
                is_error: result.timed_out || result.code != Some(0),
                images: Vec::new(),
                summary: None,
                control: super::ToolControl::Continue,
                status: None,
                diff: None,
            })
        })
    }
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
                        Err(ToolError::new("E_CANCELLED", "命令已取消"))
                    }
                }
            }
        }
        Selection::Cancelled => {
            process.group.kill()?;
            if let Err(error) = process.completion.await {
                tracing::warn!(%error, "取消后回收命令失败");
            }
            Err(ToolError::new("E_CANCELLED", "命令已取消"))
        }
    }
}

fn reject_denied(patterns: &[String], command: &str) -> Result<(), ToolError> {
    for pattern in patterns {
        let matcher = grep_regex::RegexMatcher::new(pattern)
            .map_err(|error| ToolError::new("E_CONFIG", format!("危险命令规则无效：{error}")))?;
        if matcher.is_match(command.as_bytes()).unwrap_or(false) {
            return Err(ToolError::new("E_BASH_DENIED", "命令匹配禁止执行规则"));
        }
    }
    Ok(())
}

fn resolve_cwd(base: &Path, requested: Option<&str>) -> Result<PathBuf, ToolError> {
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
        return Err(ToolError::new("E_BASH_CWD", "cwd 不是目录"));
    }
    Ok(canonical)
}

async fn next_job_id(ctx: &ToolCtx) -> Result<String, ToolError> {
    let jobs = ctx.paths.jobs();
    tokio::fs::create_dir_all(&jobs)
        .await
        .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
    if ctx.state.lock().await.job_sequence == 0 {
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
        let mut state = ctx.state.lock().await;
        state.job_sequence = state.job_sequence.max(sequence);
    }
    loop {
        let id = {
            let mut state = ctx.state.lock().await;
            state.job_sequence = state
                .job_sequence
                .checked_add(1)
                .ok_or_else(|| ToolError::new("E_BASH_LOG", "任务编号空间已耗尽"))?;
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

async fn read_bounded_file(
    path: &Path,
    head_bytes: usize,
    tail_bytes: usize,
) -> std::io::Result<String> {
    let metadata = tokio::fs::metadata(path).await?;
    let mut file = tokio::fs::File::open(path).await?;
    let head_len = metadata.len().min(head_bytes as u64) as usize;
    let mut head = vec![0; head_len];
    file.read_exact(&mut head).await?;
    let tail_len = metadata
        .len()
        .saturating_sub(head_len as u64)
        .min(tail_bytes as u64) as usize;
    let tail_start = metadata.len().saturating_sub(tail_len as u64);
    if tail_start > head_len as u64 {
        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(tail_start)).await?;
    }
    let mut tail = vec![0; tail_len];
    if tail_len > 0 {
        file.read_exact(&mut tail).await?;
    }
    let text = if tail_start > head_len as u64 {
        format!(
            "{}\n[… {} 字节已省略；完整输出：{}]\n{}",
            String::from_utf8_lossy(&head),
            tail_start - head_len as u64,
            path.display(),
            String::from_utf8_lossy(&tail)
        )
    } else {
        format!(
            "{}{}",
            String::from_utf8_lossy(&head),
            String::from_utf8_lossy(&tail)
        )
    };
    Ok(text)
}

fn render_exit(output: &str, status: i32, timed_out: bool, timeout: Option<Duration>) -> String {
    let mut output = if output.is_empty() {
        "(no output)".to_owned()
    } else {
        output.to_owned()
    };
    if timed_out {
        output.push_str(&format!(
            "\n(超时 {} 秒后被终止)",
            timeout.map_or(0, |duration| duration.as_secs())
        ));
    } else if status != 0 {
        output.push_str(&format!("\n(exit {status})"));
    }
    output
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
