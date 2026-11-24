use super::RetryTrim;
use super::tools::NOT_EXECUTED;
use super::tools::persist_tool_result;
use crate::service::CoreInner;
use crate::service::to_rpc;
use crate::session::entries::Entry;
use crate::tools::ToolOutput;
use moc_llm::FinishReason;
use moc_llm::Role;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::methods::ToolStatus;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

/// 重新作答：把 `tool_call_id` 标识的 ask 调用截回未回答状态后原样重放，
/// 新答案落库为该调用的结果。
///
/// 调用方须先持提示锁——重放期间会话对外表现为「进行中的操作」。会话停在
/// 「等模型续跑」的状态，前端随后用 `session/retry` 让模型带着新答案继续。
pub(crate) async fn run_reask(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session_id: &str,
    record: &Arc<crate::session::SessionRecord>,
    tool_call_id: &str,
) -> Result<(), RpcError> {
    let call = inner
        .sessions
        .prepare_reask(session_id, tool_call_id)
        .await
        .map_err(to_rpc)?;
    // 参数原样取自落库调用，反向请求走与首次提问完全相同的管线。`prompt_id`
    // 只给前端标记请求归属；重放不在提示轮次里，用独立前缀避免误并入某个
    // live turn。
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let started = Instant::now();
    let outcome = match crate::tools::ask::run_ask(
        &record.state,
        &ctx.requester(),
        session_id,
        &format!("reask_{}", crate::ids::entry_id()),
        args,
        true,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => error.into_outcome(),
    };
    let ToolOutput::Text(content) = outcome.content else {
        unreachable!("ask 只产生文本结果");
    };
    persist_tool_result(
        inner,
        session_id,
        call.id,
        content,
        outcome.is_error,
        outcome.status.unwrap_or(if outcome.is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Completed
        }),
        started.elapsed().as_millis() as u64,
    )
    .await
    .map_err(to_rpc)
}

/// transcript 尾部是否标记着「上一轮已正常结束」。
///
/// 只认正向标记，缺标记一律视为可重试：正常结束的判定必须能区分「EndTurn 工具收尾」
/// 与「工具跑完但下一次请求失败」——两者在 transcript 上都是 assistant(调用)+工具结果，
/// 区别只在 EndTurn 的结果带 [`FinishReason::Stop`]（`commit_prepared` 写入）。拿注册表
/// 或「看起来结束了」来猜都会误伤：请求失败时尾部只有一条用户消息或半截工具批次。
///
/// 判定只看尾部连续的可见消息段，记账/状态条目（Usage、ChildTool、PtcState、
/// ModelChange、Compaction、Todos、Plan、Reminder、Bash 等）一律跳过：
///
/// - 尾部是连续的 Tool 角色消息：任一条 `finish_reason == Some(Stop)` 即为真——EndTurn
///   后同批剩余调用可能补了「未执行」占位，所以要扫整段而不是只看最后一条。
/// - 尾部是 assistant 消息：`Stop`/`StopSequence` 且不带工具调用为真；Length、
///   ContentFilter、Cancelled、Interrupted、LoopDetected 或带调用都为假。
/// - 其它（user / custom / 代理消息 / 提醒 / 没有消息）都为假。
pub(crate) fn turn_completed(entries: &[Entry]) -> bool {
    for entry in entries.iter().rev() {
        let Entry::Message {
            message,
            finish_reason,
            ..
        } = entry
        else {
            continue;
        };
        return match message.role {
            Role::Tool => {
                // 整段连续 Tool 结果：任一条带 Stop 标记即为正常结束
                //（EndTurn 后同批的「未执行」占位也在这段里）。
                let mut completed = false;
                for entry in entries.iter().rev() {
                    match entry {
                        Entry::Message {
                            message,
                            finish_reason,
                            ..
                        } if message.role == Role::Tool => {
                            if *finish_reason == Some(FinishReason::Stop) {
                                completed = true;
                            }
                        }
                        Entry::Message { .. } => break,
                        _ => continue,
                    }
                }
                completed
            }
            Role::Assistant => {
                matches!(
                    finish_reason,
                    Some(FinishReason::Stop | FinishReason::StopSequence)
                ) && message.tool_calls().next().is_none()
            }
            _ => false,
        };
    }
    false
}

/// 剪掉上一轮没跑完的尾巴，让重试「像什么都没发生」。
///
/// 上一轮已正常结束（[`turn_completed`] 有正向标记）时直接拒绝——没有可重试的内容，
/// 重发上下文只会花一次模型请求重复上一轮的产出。
/// 只认 [`failed_turn_reason`] 认定的失败回合：模型请求直接失败时没有落库内容，这里
/// 什么也剪不到。
///
/// 三种收场分别处理：
///
/// - 半截正文（没有工具调用）：整条丢掉，否则模型会对着自己没说完的半句话继续编。
/// - 一批工具调用全部停在「未执行」占位：删掉占位，把 assistant 消息原样留着，调用方
///   直接重放这批调用，省掉一次模型请求。
/// - 批次里已经有真实结果（跑到一半被打断）：一条都不动。那些结果记着副作用已经发生，
///   删掉只会让模型重复执行；占位结果本身写着「没执行」，模型看得懂。
pub(super) async fn trim_failed_tail(
    inner: &Arc<CoreInner>,
    session_id: &str,
) -> Result<RetryTrim, RpcError> {
    let mut entries = inner
        .sessions
        .read_entries(session_id)
        .await
        .map_err(to_rpc)?;
    // 上一轮正常结束就没有可重试的东西；这一步先于任何模型请求与 transcript 改写。
    if turn_completed(&entries) {
        return Err(crate::service::invalid_params(
            moc_i18n::t!("core.err.retry_turn_completed").into_owned(),
        ));
    }
    // 尾部先剥掉不属于模型上下文、但同属这一轮的东西：取消路径给「模型请求过、一个都
    // 没跑」的工具补的占位结果，以及用量、PTC 子结果这类记账条目。整段一起剪掉，留下
    // 的前缀 parent 链才连续。
    let mut placeholders = Vec::new();
    let mut tail_start = entries.len();
    while tail_start > 0 {
        match &entries[tail_start - 1] {
            Entry::Usage { .. } | Entry::ChildTool { .. } | Entry::PtcState { .. } => {
                tail_start -= 1
            }
            Entry::Message {
                message,
                tool: Some(tool),
                ..
            } if tool.status == ToolStatus::Cancelled => {
                placeholders.extend(
                    message
                        .tool_results()
                        .map(|result| result.content.clone())
                        .collect::<Vec<_>>(),
                );
                tail_start -= 1;
            }
            _ => break,
        }
    }
    let Some(Entry::Message {
        message,
        finish_reason,
        ..
    }) = tail_start
        .checked_sub(1)
        .and_then(|index| entries.get(index))
    else {
        return Ok(RetryTrim::Nothing);
    };
    if !finish_reason.as_ref().is_some_and(failed_turn_reason) {
        return Ok(RetryTrim::Nothing);
    }
    let calls: Vec<moc_llm::ToolCall> = message.tool_calls().cloned().collect();
    if calls.is_empty() {
        entries.truncate(tail_start - 1);
        inner
            .sessions
            .rewrite(session_id, entries)
            .await
            .map_err(to_rpc)?;
        return Ok(RetryTrim::Dropped);
    }
    if placeholders.len() != calls.len() || placeholders.iter().any(|text| text != NOT_EXECUTED) {
        return Ok(RetryTrim::Nothing);
    }
    entries.truncate(tail_start);
    inner
        .sessions
        .rewrite(session_id, entries)
        .await
        .map_err(to_rpc)?;
    Ok(RetryTrim::Replay(calls))
}

/// 这个结束原因是否代表「这一轮没跑完、可以整轮重来」。
pub(super) fn failed_turn_reason(reason: &FinishReason) -> bool {
    matches!(
        reason,
        FinishReason::Cancelled | FinishReason::Interrupted | FinishReason::LoopDetected
    )
}
