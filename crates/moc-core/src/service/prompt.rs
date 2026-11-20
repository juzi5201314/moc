use crate::{
    compact,
    compact::replay::model_messages,
    service::{CoreInner, to_rpc},
    system_prompt::reminder::{self, Reminder},
};
use moc_llm::Message;
use moc_protocol::{RequestContext, RpcError, methods::PromptResult};
use std::sync::Arc;

pub(crate) mod guard;
mod input;
mod preview;
mod reminders;
pub(crate) mod request;
mod stream;
pub(crate) mod tools;

mod retry;
pub(crate) use retry::run_reask;
pub(crate) use retry::turn_completed;
mod lifecycle;
use lifecycle::run_prompt_entry;
pub(crate) use lifecycle::run_prompt_with_lock;
pub(crate) use lifecycle::wake_for_jobs;
mod warnings;

/// 提示运行类型，仅在 crate 内部区分输入处理语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptRun {
    /// 普通提示：解析并校验新输入，写入用户消息。
    Normal,
    /// 重试提示：剪掉失败尾部并按需重放工具，不追加用户消息。
    Retry,
    /// 后台唤醒：沿用现有上下文，不解析输入、不剪尾、不追加用户消息。
    Wake,
}

pub async fn run_prompt(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
) -> Result<PromptResult, RpcError> {
    run_prompt_entry(inner, ctx, params, PromptRun::Normal).await
}

/// 思维链重复后最多纠偏几次；再重复就按循环终止。
const MAX_THINKING_REDIRECTS: usize = 3;

/// 不追加任何用户消息，按会话现有上下文继续跑一次提示。
///
/// 内部仍走同一套提示循环：提示锁、取消、工具执行、压缩与收尾完全复用。重试类型只
/// 改变输入段：先把上一轮没跑完的尾巴剪掉（见 [`trim_failed_tail`]），然后既不解析
/// 也不落库新的用户消息，直接把当前 transcript 交给模型。
pub async fn run_retry(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::SessionRetryParams,
) -> Result<PromptResult, RpcError> {
    let params = moc_protocol::methods::PromptParams {
        session_id: params.session_id,
        input: Vec::new(),
        mode: None,
        agent_message: None,
        goal_round: None,
        max_turns: None,
    };
    run_prompt_entry(inner, ctx, params, PromptRun::Retry).await
}

/// 一轮请求需要的会话状态副本。
///
/// 只复制构建请求所需的部分，避免 `'turns` 循环每轮深拷贝整个 [`SessionState`]
/// （长会话带 base64 图片时一次可达数十 MB）。
struct TurnInputs {
    messages: Vec<Message>,
    context_tokens: u64,
}

/// 在调用方持有的会话状态锁内生成 [`TurnInputs`]；持锁期间没有 await。
fn turn_inputs(
    state: &crate::session::state::SessionState,
    system: &[Message],
    model: &crate::ResolvedModel,
    tools: &[moc_llm::ToolDef],
) -> TurnInputs {
    let messages = model_messages(state, Some(system), Some(model));
    let context_tokens = compact::context_tokens(state, &messages, tools);
    TurnInputs {
        messages,
        context_tokens,
    }
}

/// 把候选提醒里变化过的条目写进历史。
///
/// 只写变化的部分：条目正文的摘要与历史里那份相同就跳过，所以一次提示里的后续请求
/// 不再重复注入。写进去的提醒成为历史的一部分，可以吃到前缀缓存。
async fn flush_reminders(
    inner: &Arc<CoreInner>,
    session_id: &str,
    record: &Arc<crate::session::SessionRecord>,
    candidates: &[Reminder],
) -> Result<(), RpcError> {
    // `pending` 的返回值只借用 `candidates`，所以状态锁在块结束时就能释放。
    let selected = {
        let state = record.state.lock().await;
        reminder::pending(candidates, &state.reminder_digests)
    };
    if selected.is_empty() {
        return Ok(());
    }
    let (message, items) = reminder::render(&selected);
    inner
        .sessions
        .append_reminder(session_id, message, items)
        .await
        .map_err(to_rpc)?;
    Ok(())
}

mod execution;
mod skills;

/// `/retry` 对 transcript 尾部的处理结果。
enum RetryTrim {
    /// 尾部没有可剪的失败回合：按现有上下文原样重发，模型自己接着往下走。
    Nothing,
    /// 已剪掉被打断的半截回复（含它后面的记账条目）。
    Dropped,
    /// 失败回合的工具调用一个都没跑：占位结果已删，直接重放这批调用。
    Replay(Vec<moc_llm::ToolCall>),
}

/// 请求耗时警告：到时间只推一条通知，请求结束时取消计时器。
struct RequestWarning(tokio::task::JoinHandle<()>);

#[cfg(test)]
mod tests;
