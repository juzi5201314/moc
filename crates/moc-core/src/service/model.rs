//! 切换会话模型：切换前按目标模型的窗口检查上下文是否装得下。

use std::path::Path;

use moc_protocol::RequestContext;
use moc_protocol::methods::{
    ModelSwitchOverflow, SessionEngine, SessionInfo, SessionModelOverflowAction, SessionUpdate,
};

use super::CoreInner;
use crate::ResolvedModel;
use crate::compact::{self, CompactReason};
use crate::error::CoreError;

/// 一次模型切换的结果。
pub(crate) struct SwitchOutcome {
    pub session: SessionInfo,
    pub overflow: Option<ModelSwitchOverflow>,
    pub compaction_error: Option<String>,
}

/// 切换会话模型；调用方必须已经持有该会话的提示锁和文件锁。
///
/// 目标模型装不下当前上下文时按 `action` 处理：`Ask` 不切换，只返回缺口；
/// `Switch` 直接切换；`Compact` 先切换，再用新模型压缩。
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
    let overflow = if config.compaction.enabled && state.engine != SessionEngine::ClmExperimental {
        let tokens = compact::model_context(inner, ctx, &state, &config, model)
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
    // 模型变了，旧模型提前生成的摘要（含 NOTE₁）不能再用。
    inner.runtime(session_id).await.grok_prefire.clear();
    // 必须先切换再压缩：ModelChange 会让旧模型的原生压缩数据和远程摘要失效，
    // checkpoint 只能由新模型生成。
    let compaction_error = match action {
        SessionModelOverflowAction::Compact if overflow.is_some() => compact::compact(
            inner,
            ctx,
            session_id,
            CompactReason::Switch,
            None,
            None,
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
