//! Grok Build 的两遍压缩：在触发阈值之前先摘要前缀，真正压缩时只补尾部。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use moc_llm::{Message, Role};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::ResolvedModel;
use crate::config::{CompactionMethod, CompactionThreshold, ResolvedConfig, SoftMethod};
use crate::service::{CoreInner, SessionRuntime};

use super::{PromptSkeleton, Sampler, prompt};

/// 第一遍覆盖对话的权重比例。
pub(crate) const TWO_PASS_DEFAULT_SPLIT_FRACTION: f64 = 0.95;

/// 提前量占上下文窗口的百分比；可用环境变量覆盖。
const DEFAULT_PREFIRE_LEAD_PERCENT: u64 = 10;

const PREFIRE_LEAD_ENV: &str = "GROK_PREFIRE_LEAD_PERCENT";

/// 提前量占窗口的百分比。
pub(crate) fn prefire_lead_percent() -> u64 {
    std::env::var(PREFIRE_LEAD_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PREFIRE_LEAD_PERCENT)
}

/// 预火启动点：压缩触发阈值往前推一个提前量。
pub(crate) fn prefire_start(config: &ResolvedConfig, context_window: u64) -> u64 {
    let lead = prefire_lead_percent();
    match config.compaction.threshold {
        CompactionThreshold::Percent(percent) => {
            context_window.saturating_mul(percent.saturating_sub(lead)) / 100
        }
        CompactionThreshold::Tokens(tokens) => {
            tokens.saturating_sub(context_window.saturating_mul(lead) / 100)
        }
    }
}

/// 第一遍的产物：交给第二遍的 NOTE₁，以及它覆盖的前缀长度。
#[derive(Debug, Clone)]
pub(crate) struct PrefireCache {
    pub note1: String,
    pub prefix_len: usize,
    pub fingerprint: u128,
    pub model: String,
}

/// 会话级的预火状态。
///
/// 全部用同步原语：压缩路径和提示循环会并发访问它，异步锁一旦跨任务持有，就会把提示
/// 卡在预火上。
#[derive(Default)]
pub(crate) struct PrefireState {
    in_flight: AtomicBool,
    /// 每次 `try_begin` / `clear` 都会递增；旧任务只能写入自己那一代的结果。
    generation: AtomicU64,
    cache: Mutex<Option<PrefireCache>>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cancel: Mutex<CancellationToken>,
}

impl PrefireState {
    /// 抢占本次预火；已经有一遍在跑时返回 `None`。
    pub(crate) fn try_begin(&self) -> Option<(u64, CancellationToken)> {
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let token = CancellationToken::new();
        *self.cancel.lock() = token.clone();
        Some((generation, token))
    }

    /// 结束本次预火；代际对不上说明已被 `clear` 作废，不再改状态。
    pub(crate) fn finish(&self, generation: u64) {
        if self.generation.load(Ordering::Acquire) == generation {
            self.in_flight.store(false, Ordering::Release);
        }
    }

    /// 保存第一遍的结果；只接受当前代际的结果。
    pub(crate) fn store(&self, generation: u64, cache: PrefireCache) {
        if self.generation.load(Ordering::Acquire) == generation {
            *self.cache.lock() = Some(cache);
        }
    }

    /// 缓存的结果是否仍然覆盖当前切点之前的那段前缀。
    ///
    /// 对话是追加式的：只要缓存的前缀长度不超过当前切点，它就还是同一条前缀的开头，
    /// 第二遍可以直接续写，不必在频带内每轮都重跑第一遍。
    pub(crate) fn keeps_valid_prefix(&self, split: usize, model: &str) -> bool {
        self.cache
            .lock()
            .as_ref()
            .is_some_and(|cache| cache.model == model && cache.prefix_len <= split)
    }

    pub(crate) fn set_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.handle.lock() = Some(handle);
    }

    /// 取出第一遍的结果给第二遍用；如果预火还在跑，先等它结束。
    pub(crate) async fn take_for_pass2(
        &self,
        cancel: &CancellationToken,
        model: &ResolvedModel,
        messages: &[Message],
    ) -> Option<(String, usize)> {
        // 先取出 handle 再 await：parking_lot 的守卫不能跨 await 存活，否则 future 不是 Send。
        let handle = { self.handle.lock().take() };
        if let Some(handle) = handle {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                _ = handle => {}
            }
        }
        let cache = { self.cache.lock().take() }?;
        if cache.model != model.selector || cache.prefix_len > messages.len() {
            return None;
        }
        if conversation_fingerprint(&messages[..cache.prefix_len]) != cache.fingerprint {
            return None;
        }
        Some((cache.note1, cache.prefix_len))
    }

    /// 作废预火：取消在跑的任务、丢弃结果，并让旧任务无法再写回状态。
    pub(crate) fn clear(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cancel.lock().cancel();
        if let Some(handle) = self.handle.lock().take() {
            handle.abort();
        }
        *self.cache.lock() = None;
        self.in_flight.store(false, Ordering::Release);
    }
}

/// 对话前缀指纹；只有前缀完全一致时第一遍结果才可复用。
pub(crate) fn conversation_fingerprint(prefix: &[Message]) -> u128 {
    let serialized = serde_json::to_string(prefix).unwrap_or_default();
    xxhash_rust::xxh3::xxh3_128(serialized.as_bytes())
}

/// 在 [阈值 - 提前量, 阈值) 区间内启动预火。
///
/// 所有前置条件都在这里判断，调用方无条件调用即可。
pub(crate) async fn maybe_prefire(
    inner: &Arc<CoreInner>,
    session_id: &str,
    config: &ResolvedConfig,
    model: &ResolvedModel,
    context_tokens: u64,
    skeleton: PromptSkeleton<'_>,
    cancel: &CancellationToken,
) {
    if !config.compaction.enabled
        || !config.compaction.auto
        || config.compaction.soft_method != SoftMethod::GrokBuild
        || !config
            .compaction
            .method_order
            .contains(&CompactionMethod::Soft)
    {
        return;
    }
    let start = prefire_start(config, model.context_window);
    let trigger = super::super::threshold(config, model.context_window);
    if context_tokens < start || context_tokens >= trigger || cancel.is_cancelled() {
        return;
    }
    let Ok(record) = inner.sessions.get(session_id).await else {
        return;
    };
    let (messages, cwd) = {
        let state = record.state.lock().await;
        if state.mode == moc_protocol::methods::SessionMode::ClmExperimental {
            return;
        }
        (
            super::effective_messages(&state, model),
            state.session.cwd.clone(),
        )
    };
    let Ok(project_config) = inner.project_config(std::path::Path::new(&cwd)) else {
        return;
    };
    let runtime = inner.runtime(session_id).await;
    let Some((generation, prefire_cancel)) = runtime.grok_prefire.try_begin() else {
        return;
    };
    if messages.len() < 4 {
        runtime.grok_prefire.finish(generation);
        return;
    }
    let Some(split) = split_conversation_for_two_pass(&messages, TWO_PASS_DEFAULT_SPLIT_FRACTION)
    else {
        runtime.grok_prefire.finish(generation);
        return;
    };
    if runtime
        .grok_prefire
        .keeps_valid_prefix(split, &model.selector)
    {
        runtime.grok_prefire.finish(generation);
        return;
    }
    let prefix = messages[..split].to_vec();
    let inner = Arc::clone(inner);
    let session_id = session_id.to_owned();
    let model = model.clone();
    let system = skeleton.system.to_vec();
    let static_prefix = skeleton.static_prefix.to_vec();
    let tools = skeleton.tools.to_vec();
    let task_runtime = Arc::clone(&runtime);
    let handle = tokio::spawn(async move {
        let sampler = Sampler::new(
            &inner,
            &project_config,
            &session_id,
            &model,
            PromptSkeleton {
                system: &system,
                static_prefix: &static_prefix,
                tools: &tools,
            },
        );
        run_pass1(
            &task_runtime,
            &sampler,
            generation,
            &prefire_cancel,
            &prefix,
        )
        .await;
    });
    runtime.grok_prefire.set_handle(handle);
}

/// 第一遍：摘要前缀，留下 NOTE₁ 供第二遍拼接。
async fn run_pass1(
    runtime: &SessionRuntime,
    sampler: &Sampler<'_>,
    generation: u64,
    cancel: &CancellationToken,
    prefix: &[Message],
) {
    let prompt = prompt::build_summary_prompt(None);
    let sampled = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = sampler.sample(cancel, prefix, &prompt) => Some(result),
    };
    match sampled {
        Some(Ok(raw)) => {
            let note1 = prompt::note_for_two_pass_pass2(&raw);
            if !note1.trim().is_empty() {
                runtime.grok_prefire.store(
                    generation,
                    PrefireCache {
                        note1,
                        prefix_len: prefix.len(),
                        fingerprint: conversation_fingerprint(prefix),
                        model: sampler.model_selector().to_owned(),
                    },
                );
            }
        }
        // 预火是纯优化：失败只留诊断，绝不打扰用户。
        Some(Err(error)) => {
            tracing::debug!(%error, "Grok Build 预火第一遍失败");
        }
        None => tracing::debug!("Grok Build 预火第一遍被取消"),
    }
    runtime.grok_prefire.finish(generation);
}

/// 按权重找到第一遍与第二遍的切点；切不动时返回 `None`。
///
/// 切点不能落在工具结果或带工具调用的助手消息上，否则前缀或尾部会出现孤立结果。
pub(crate) fn split_conversation_for_two_pass(
    messages: &[Message],
    fraction: f64,
) -> Option<usize> {
    let len = messages.len();
    if len < 2 {
        return None;
    }
    let weight = super::super::messages_tokens(messages);
    if weight == 0 {
        return None;
    }
    // 权重估算是整数；f64 只用来表达比例，结果不会超过总权重。
    let clamped = fraction.clamp(0.0, 1.0);
    let target = (weight as f64 * clamped) as u64;
    let mut accumulated = 0u64;
    let mut split = len;
    for (index, message) in messages.iter().enumerate() {
        accumulated = accumulated.saturating_add(super::super::message_tokens(message));
        if accumulated >= target {
            split = index + 1;
            break;
        }
    }
    split = split.clamp(1, len - 1);
    while split < len && breaks_pairing(messages, split) {
        split += 1;
    }
    if split >= len {
        split = len - 1;
        while split > 1 && messages[split].role == Role::Tool {
            split -= 1;
        }
    }
    if split == 0 || split >= len || messages[split].role == Role::Tool {
        return None;
    }
    Some(split)
}

/// 切在工具调用或工具结果上会拆开配对。
fn breaks_pairing(messages: &[Message], split: usize) -> bool {
    let message = &messages[split];
    message.role == Role::Tool
        || (message.role == Role::Assistant && message.tool_calls().next().is_some())
}

#[cfg(test)]
mod tests {
    use moc_llm::{ContentPart, ToolCall};
    use moc_protocol::methods::{SessionMode, SessionPermission};

    use super::*;
    use crate::session::entries::{Entry, EntryPayload, SessionOrigin};
    use crate::session::state::SessionState;

    fn big(chars: usize) -> String {
        "a".repeat(chars * 4)
    }

    fn assistant_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall(ToolCall::new(id, "read", "{}"))],
        }
    }

    fn test_config() -> ResolvedConfig {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("home")).unwrap();
        std::fs::write(
            directory.path().join("home/config.toml"),
            "[models.roles]\ndefault='p/m'\nexplore='p/m'\nplan='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n",
        )
        .unwrap();
        ResolvedConfig::load(&crate::CoreConfig {
            home: directory.path().join("home"),
            cwd: directory.path().to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
            ptc_worker: None,
        })
        .unwrap()
    }

    fn test_model() -> ResolvedModel {
        test_config().resolve_model("p/m").unwrap()
    }

    #[test]
    fn prefire_state_gates_single_flight_and_clear() {
        let state = PrefireState::default();
        let (generation, _token) = state.try_begin().expect("首次抢占成功");
        assert!(state.try_begin().is_none(), "在跑时不再抢占");
        state.store(
            generation,
            PrefireCache {
                note1: "笔记".into(),
                prefix_len: 1,
                fingerprint: 0,
                model: "p/m".into(),
            },
        );
        assert!(state.keeps_valid_prefix(1, "p/m"));
        state.finish(generation);
        assert!(state.try_begin().is_some(), "结束后可以再次抢占");

        state.clear();
        assert!(!state.keeps_valid_prefix(1, "p/m"), "clear 丢弃结果");
        assert!(state.try_begin().is_some(), "clear 之后可以重新抢占");
    }

    #[test]
    fn stale_generation_cannot_store_or_finish() {
        let state = PrefireState::default();
        let (generation, _token) = state.try_begin().unwrap();
        state.clear();
        state.store(
            generation,
            PrefireCache {
                note1: "过期".into(),
                prefix_len: 1,
                fingerprint: 0,
                model: "p/m".into(),
            },
        );
        assert!(!state.keeps_valid_prefix(1, "p/m"), "过期任务的结果被丢弃");
        assert!(
            state.try_begin().is_some(),
            "过期任务的 finish 不影响新一轮"
        );
    }

    #[test]
    fn split_moves_off_tool_pairs() {
        let messages = vec![
            Message::user_text(big(100)),
            assistant_call("c1"),
            Message::tool_result("c1", big(100), false),
            Message::user_text(big(10)),
        ];
        let split = split_conversation_for_two_pass(&messages, 0.5).expect("切点越过工具结果");
        assert_eq!(split, 3, "切点不能落在工具结果上");
        assert_ne!(messages[split].role, Role::Tool);
    }

    #[test]
    fn effective_messages_expands_raw_history_after_remote_checkpoint() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            1,
        );
        let first = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("第一句"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            header.id().to_owned(),
            2,
        );
        let second = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("第二句"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            first.id().to_owned(),
            3,
        );
        let compacted = Entry::from_payload(
            EntryPayload::Compaction {
                method: "remote".into(),
                summary: "占位摘要".into(),
                first_kept_id: None,
                tokens_before: 10,
                tokens_after: 5,
                instructions: None,
                native: Some(Box::new(serde_json::json!([{ "type": "message" }]))),
                model: Some("p/m".into()),
                reminder: None,
            },
            second.id().to_owned(),
            4,
        );
        let state = SessionState::from_entries(vec![header, first, second, compacted]).unwrap();
        assert_eq!(state.native_input.as_ref().map(Vec::len), Some(1));
        // 原生载荷无法在文本视图里还原早期历史，Grok 必须展开原始消息。
        let messages = super::super::effective_messages(&state, &test_model());
        assert_eq!(messages.len(), 2, "{messages:?}");
    }
}
