//! 一次提示内的循环保护状态，以及跨轮的无进展检测；压缩、续写和 PTC 都不会重置它们。

use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use xxhash_rust::xxh3::Xxh3;

use crate::tools::{ToolCtx, ToolOutcome, ToolOutput};
use moc_llm::{FinishReason, LoopViolation, ToolCall};

pub(crate) struct PromptGuard {
    violation: Mutex<Option<LoopViolation>>,
    progress: AtomicU64,
    pub(crate) cancel: CancellationToken,
    /// 只在循环保护叫停时取消。用户主动取消不会触发它，已有后台任务照常运行。
    pub(crate) stopped: CancellationToken,
    parent: Option<Arc<Self>>,
}

impl PromptGuard {
    pub(crate) fn new(cancel: CancellationToken, parent: Option<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self {
            violation: Mutex::new(None),
            progress: AtomicU64::new(0),
            stopped: parent
                .as_ref()
                .map_or_else(CancellationToken::new, |parent| {
                    parent.stopped.child_token()
                }),
            cancel,
            parent,
        })
    }

    pub(crate) fn stop(&self, violation: LoopViolation) {
        {
            let mut reason = self.violation.lock();
            if reason.is_none() {
                *reason = Some(violation);
            }
        }
        self.stopped.cancel();
        self.cancel.cancel();
    }

    pub(crate) fn violation(&self) -> Option<LoopViolation> {
        self.violation
            .lock()
            .clone()
            .or_else(|| self.parent.as_ref().and_then(|parent| parent.violation()))
    }

    pub(crate) fn reason(&self) -> FinishReason {
        self.violation()
            .map_or(FinishReason::Cancelled, |_| FinishReason::LoopDetected)
    }

    pub(crate) fn progress(&self) -> u64 {
        self.progress.load(Ordering::Relaxed)
    }

    pub(crate) fn record_change(&self) {
        self.progress.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) enum CycleAction {
    Continue,
    Redirect(String),
    Stop(String),
}

/// 只保留最近 32 个定长指纹，检测长度为 1～4 轮的周期；同一无进展周期重复 5 次才纠偏。
#[derive(Default)]
pub(crate) struct ToolCycles {
    history: [u128; 32],
    len: usize,
    next: usize,
    redirected: bool,
}

impl ToolCycles {
    pub(crate) fn record(&mut self, observations: &[(u128, bool, bool)]) -> CycleAction {
        if observations.is_empty() {
            return CycleAction::Continue;
        }
        // 有已提交的改动，或者全都是还没结束的正常等待，就不算重复动作。
        if observations.iter().any(|(_, changed, _)| *changed)
            || observations.iter().all(|(_, _, waiting)| *waiting)
        {
            self.len = 0;
            self.next = 0;
            return CycleAction::Continue;
        }
        let mut hash = Xxh3::new();
        for (fingerprint, _, _) in observations {
            hash.update(&fingerprint.to_le_bytes());
        }
        self.history[self.next] = hash.digest128();
        self.next = (self.next + 1) % self.history.len();
        self.len = (self.len + 1).min(self.history.len());
        debug_assert!(self.len <= 32);
        for period in 1..=4 {
            if self.len < period * 5 {
                continue;
            }
            let repeated =
                (period..period * 5).all(|offset| self.back(offset) == self.back(offset % period));
            if repeated {
                // detail 也会进入模型可见的 <system-interrupt>，所以按提示词规范用英文。
                let detail = format!(
                    "Tool calls and full results repeated a {period}-turn cycle 5 times without committed changes"
                );
                if self.redirected {
                    return CycleAction::Stop(detail);
                }
                self.redirected = true;
                return CycleAction::Redirect(detail);
            }
        }
        CycleAction::Continue
    }

    fn back(&self, offset: usize) -> u128 {
        self.history[(self.next + self.history.len() - 1 - offset) % self.history.len()]
    }
}

struct HashWriter<'a>(&'a mut Xxh3);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hash_json(hash: &mut Xxh3, value: &serde_json::Value) {
    // serde_json 默认的对象表示按键排序；数组和调用批次的顺序有含义，保持原样。
    serde_json::to_writer(HashWriter(hash), value).expect("JSON Value 可序列化");
}

/// 在输出归档或截断之前对完整结果计算指纹，避免把每次都不同的 artifact ID 当成进展。
pub(super) async fn observe_tool(
    ctx: &ToolCtx,
    call: &ToolCall,
    outcome: &ToolOutcome,
) -> std::io::Result<(u128, bool, bool)> {
    use tokio::io::AsyncReadExt;
    let mut arguments = serde_json::from_str::<serde_json::Value>(&call.arguments);
    if let Ok(serde_json::Value::Object(arguments)) = &mut arguments {
        // `i` 是描述调用意图的元数据，先剔除；其余参数全部参与比较。
        arguments.remove("i");
    }
    let mut hash = Xxh3::new();
    hash.update(call.name.as_bytes());
    if let Ok(arguments) = &arguments {
        hash_json(&mut hash, arguments);
    } else {
        hash.update(call.arguments.as_bytes());
    }
    hash.update(&[u8::from(outcome.is_error)]);
    if let Some(code) = &outcome.error_code {
        hash.update(code.as_bytes());
    }
    match &outcome.content {
        ToolOutput::Text(text) => hash.update(text.as_bytes()),
        ToolOutput::File { path, .. } | ToolOutput::Excerpt { path, .. } => {
            let mut file = tokio::fs::File::open(path).await?;
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let count = file.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            if let ToolOutput::File { suffix, .. } = &outcome.content {
                hash.update(suffix.as_bytes());
            }
        }
    }
    if let Some(structured) = &outcome.structured_content {
        if call.name == "job" {
            // elapsed_secs、warning_count 的变化不算进展；只取状态和实际日志内容。
            if let Some(status) = structured.get("status") {
                hash_json(&mut hash, status);
            }
        } else {
            hash_json(&mut hash, structured);
        }
    }
    for image in &outcome.images {
        serde_json::to_writer(HashWriter(&mut hash), image).expect("图片可序列化");
    }
    let waiting = if !outcome.is_error && call.name == "job" {
        arguments
            .as_ref()
            .ok()
            .and_then(|args| args.get("op"))
            .and_then(serde_json::Value::as_str)
            == Some("wait")
            && outcome
                .structured_content
                .as_ref()
                .and_then(|value| value.get("status"))
                .and_then(serde_json::Value::as_str)
                == Some("Running")
    } else if !outcome.is_error && call.name == "wait_agent" {
        // 「等待中」指既没进入终态也没有新消息；读结构化结果判断，不解析文本。
        outcome.structured_content.as_ref().is_some_and(|value| {
            value.get("settled") == Some(&serde_json::Value::Bool(false))
                && value.get("messages_pending") != Some(&serde_json::Value::Bool(true))
        })
    } else {
        false
    };
    // 有 diff 说明写入确实改了文件。读取时的文件版本也计入指纹：即使读到的分页没变，外部改动也不会被漏掉。
    if call.name == "read"
        && let Ok(arguments) = &arguments
        && let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str)
        && !path.contains("://")
        && let Ok(metadata) = tokio::fs::metadata(ctx.cwd.join(path)).await
    {
        hash.update(&metadata.len().to_le_bytes());
        if let Ok(modified) = metadata.modified()
            && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            hash.update(&duration.as_nanos().to_le_bytes());
        }
    }
    Ok((hash.digest128(), outcome.diff.is_some(), waiting))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_and_alternating_cycles_redirect_once_then_stop() {
        for period in 1..=4 {
            let mut detector = ToolCycles::default();
            for turn in 0..period * 5 - 1 {
                assert!(matches!(
                    detector.record(&[((turn % period) as u128, false, false)]),
                    CycleAction::Continue
                ));
            }
            assert!(matches!(
                detector.record(&[((period - 1) as u128, false, false)]),
                CycleAction::Redirect(_)
            ));
            assert!(matches!(
                detector.record(&[(0, false, false)]),
                CycleAction::Stop(_)
            ));
        }
    }

    #[test]
    fn real_changes_and_live_waits_break_cycle_evidence() {
        let mut detector = ToolCycles::default();
        for _ in 0..20 {
            assert!(matches!(
                detector.record(&[(1, false, true)]),
                CycleAction::Continue
            ));
        }
        for i in 0..20 {
            assert!(matches!(
                detector.record(&[(i, false, false)]),
                CycleAction::Continue
            ));
        }
        for _ in 0..20 {
            assert!(matches!(
                detector.record(&[(1, true, false)]),
                CycleAction::Continue
            ));
        }
    }

    #[test]
    fn first_violation_wins_and_stop_cancels_both_tokens() {
        let guard = PromptGuard::new(CancellationToken::new(), None);
        assert_eq!(guard.reason(), FinishReason::Cancelled);
        guard.stop(LoopViolation {
            channel: "工具",
            detail: "重复".into(),
        });
        assert!(guard.cancel.is_cancelled());
        assert!(guard.stopped.is_cancelled());
        assert_eq!(guard.reason(), FinishReason::LoopDetected);
        guard.stop(LoopViolation {
            channel: "正文",
            detail: "后到".into(),
        });
        assert_eq!(
            guard.violation().map(|violation| violation.channel),
            Some("工具")
        );
    }

    #[test]
    fn parent_stop_reaches_descendants_and_supplies_their_reason() {
        let parent = PromptGuard::new(CancellationToken::new(), None);
        let child = PromptGuard::new(parent.cancel.child_token(), Some(Arc::clone(&parent)));
        parent.stop(LoopViolation {
            channel: "工具",
            detail: "重复".into(),
        });
        assert!(child.cancel.is_cancelled());
        assert!(child.stopped.is_cancelled());
        assert_eq!(child.reason(), FinishReason::LoopDetected);
    }

    #[test]
    fn recorded_changes_advance_progress() {
        let guard = PromptGuard::new(CancellationToken::new(), None);
        assert_eq!(guard.progress(), 0);
        guard.record_change();
        guard.record_change();
        assert_eq!(guard.progress(), 2);
    }
}
