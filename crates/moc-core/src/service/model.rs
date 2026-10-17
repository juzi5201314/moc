//! 会话模型切换：切换前按目标模型窗口检查上下文容量。

use std::path::Path;

use moc_protocol::RequestContext;
use moc_protocol::methods::{
    ModelSwitchOverflow, SessionInfo, SessionMode, SessionModelOverflowAction, SessionUpdate,
};

use super::CoreInner;
use crate::ResolvedModel;
use crate::compact::{self, CompactReason, ContextBudget};
use crate::error::CoreError;

/// 一次模型切换的完整结果。
pub(crate) struct SwitchOutcome {
    pub session: SessionInfo,
    pub overflow: Option<ModelSwitchOverflow>,
    pub compaction_error: Option<String>,
}

/// 切换会话模型；调用方必须已持有该会话的提示锁与文件锁。
///
/// 目标模型容不下当前上下文时按 `action` 处理：`Ask` 只回报缺口不切换，
/// `Switch` 直接切换，`Compact` 先切换再用新模型压缩。
pub(crate) async fn switch(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    model: &ResolvedModel,
    action: SessionModelOverflowAction,
) -> Result<SwitchOutcome, CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let state = record.state.lock().await.clone();
    if state.model == model.selector {
        return Ok(SwitchOutcome {
            session: record.state.lock().await.session_info(),
            overflow: None,
            compaction_error: None,
        });
    }
    let config = inner.project_config(Path::new(&state.session.cwd))?;
    let overflow = if config.compaction.enabled && state.mode != SessionMode::ClmExperimental {
        let tokens = compact::model_context(inner, ctx, &state, &config, model, 0)
            .await?
            .tokens;
        let context_window = model.context_window;
        let threshold = compact::threshold(&config, context_window);
        (tokens > context_window || (config.compaction.auto && tokens > threshold)).then(|| {
            ModelSwitchOverflow {
                tokens,
                context_window,
                threshold,
                target_tokens: compact::recovery_band(&config, context_window),
            }
        })
    } else {
        None
    };
    if overflow.is_some() && action == SessionModelOverflowAction::Ask {
        return Ok(SwitchOutcome {
            session: record.state.lock().await.session_info(),
            overflow,
            compaction_error: None,
        });
    }
    let session = inner
        .sessions
        .set_model(session_id, model.selector.clone())
        .await?;
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::ModelChanged {
                model: model.selector.clone(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    // 模型变了，旧模型产出的预火摘要（含 NOTE₁）不再可用。
    inner.runtime(session_id).await.grok_prefire.clear();
    // 先切换再压缩：ModelChange 会作废旧模型的 native 载荷与 remote 摘要，
    // checkpoint 必须由新模型产出。
    let compaction_error = match action {
        SessionModelOverflowAction::Compact if overflow.is_some() => compact::compact(
            inner,
            ctx,
            session_id,
            CompactReason::Switch,
            None,
            ContextBudget::default(),
            None,
        )
        .await
        .err()
        .map(|error| error.to_string()),
        _ => None,
    };
    Ok(SwitchOutcome {
        session,
        overflow,
        compaction_error,
    })
}
