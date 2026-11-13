//! 会话用量聚合。

use super::Core;
use super::usage_descendants;
use crate::service::to_rpc;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::methods::ModelUsageSummary;
use moc_protocol::methods::SessionUsageParams;
use moc_protocol::methods::SessionUsageResult;
use std::collections::BTreeMap;

pub(super) async fn session_usage(
    core: &Core,
    _ctx: RequestContext,
    params: SessionUsageParams,
) -> Result<SessionUsageResult, RpcError> {
    // 聚合范围：会话自身 + 未被回退孤立的全部后代（Fusion sidekick 与普通
    // 子代理都算）。行按 (来源, 模型) 分组——sidekick 与主会话共用模型时
    // 也能分辨用量花在谁身上。
    let mut groups: BTreeMap<
        (moc_protocol::methods::UsageSource, String),
        crate::session::state::ModelUsage,
    > = BTreeMap::new();
    let mut total = crate::session::state::ModelUsage::default();
    let mut tool_ms = 0u64;
    let mut members = vec![(
        moc_protocol::methods::UsageSource::Session,
        params.session_id.clone(),
    )];
    members.extend(usage_descendants(&core.inner, &params.session_id).await?);
    for (source, session_id) in members {
        let record = core.inner.sessions.get(&session_id).await.map_err(to_rpc)?;
        let state = record.state.lock().await;
        tool_ms = tool_ms.saturating_add(state.tool_ms);
        for (model, usage) in &state.model_usage {
            groups
                .entry((source, model.clone()))
                .or_default()
                .merge(usage);
            total.merge(usage);
        }
    }
    let summary = |model: String,
                   source: moc_protocol::methods::UsageSource,
                   usage: &crate::session::state::ModelUsage| {
        ModelUsageSummary {
            model,
            source,
            calls: usage.calls,
            input_tokens: usage.usage.input_tokens,
            output_tokens: usage.usage.output_tokens,
            cached_input_tokens: usage.usage.cached_input_tokens,
            cache_creation_input_tokens: usage.usage.cache_creation_input_tokens,
            reasoning_tokens: usage.usage.reasoning_tokens,
            active_ms: usage.active_ms,
            ttft_ms: usage.ttft_ms,
            ttft_calls: usage.ttft_calls,
            gen_ms: usage.gen_ms,
        }
    };
    let models = groups
        .iter()
        .map(|((source, model), usage)| summary(model.clone(), *source, usage))
        .collect();
    Ok(SessionUsageResult {
        models,
        total: summary("total".into(), Default::default(), &total),
        tool_ms,
    })
}
