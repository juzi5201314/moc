//! 前台技能摘要维护：隔离模型请求，完整验证后一次性发布，不写会话历史。

use moc_llm::{FinishReason, Message};
use moc_protocol::{
    RequestContext, SkillsCompactAction, SkillsCompactParams, SkillsCompactResult, SkillsProgress,
    SkillsProgressParams, SkillsProgressPhase,
};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

use super::{
    SkillIndex,
    cache::{Cache, Entry, PROMPT, Writer, fingerprint, valid_hint},
};
use crate::{
    CoreError,
    compact::estimate_tokens,
    service::{
        CoreInner,
        prompt::request::{CacheAnchors, build_request},
    },
};

pub(crate) async fn run(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: SkillsCompactParams,
) -> Result<SkillsCompactResult, CoreError> {
    let _epoch = inner.config_epoch.read().await;
    let record = match params.session_id.as_deref() {
        Some(id) => Some(inner.sessions.get(id).await?),
        None => None,
    };
    let _foreground = record
        .as_ref()
        .map(|record| record.try_prompt_lock())
        .transpose()?;
    struct Working<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Working<'_> {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let _working = record.as_ref().map(|record| {
        record
            .skills_compacting
            .store(true, std::sync::atomic::Ordering::Release);
        Working(&record.skills_compacting)
    });
    let (cwd, selector, workflows_active) = if let Some(record) = &record {
        let state = record.state.lock().await;
        let config = inner.project_config(Path::new(&state.session.cwd))?;
        (
            state.session.cwd.clone(),
            Some(state.model.clone()),
            crate::workflow::workflows_active(&state, &config.workflow),
        )
    } else {
        let cwd = params
            .cwd
            .unwrap_or_else(|| inner.boot.cwd.to_string_lossy().into_owned());
        (cwd, params.model, false)
    };
    let cwd = Path::new(&cwd);
    let config = inner.project_config(cwd)?;
    let model = match selector {
        Some(selector) => Some(config.resolve_model(&selector)?),
        None => config.last_model()?,
    };
    let context_window = model.as_ref().map_or(0, |model| model.context_window);
    let cancel = ctx.cancel();
    check_cancel(&cancel)?;
    // 这个文件锁是缓存操作的写租约，不占用会话状态锁；普通读取一直看到旧快照。
    let writer = Writer::acquire(&config.home)?;
    let mut cache = Cache::load(&config.home)?;
    let mut index = discover(cwd, &config, workflows_active);
    cache.apply(&mut index);
    let before = tokens(&index, &config, context_window);
    let mut result = SkillsCompactResult {
        compressed: 0,
        skipped: 0,
        removed: 0,
        tokens_before: before,
        tokens_after: before,
        message: String::new(),
        warnings: Vec::new(),
    };
    match params.action {
        SkillsCompactAction::Cleanup => {
            (result.removed, result.warnings) = cache.cleanup();
        }
        SkillsCompactAction::Clear => {
            result.removed = cache.entries.len();
            cache.entries.clear();
        }
        SkillsCompactAction::Missing | SkillsCompactAction::Force => {
            let candidates: Vec<_> = index
                .list()
                .filter(|skill| skill.model_invocable)
                .filter(|skill| {
                    params.action == SkillsCompactAction::Force || cache.hint(skill).is_none()
                })
                .collect();
            result.skipped =
                index.list().filter(|skill| skill.model_invocable).count() - candidates.len();
            push_progress(
                &ctx,
                &cancel,
                SkillsProgressPhase::Generating,
                0,
                candidates.len(),
                result.skipped,
            )
            .await?;
            let mut start = 0;
            while start < candidates.len() {
                let model = model
                    .as_ref()
                    .ok_or_else(|| CoreError::Config("请先选择用于技能压缩的模型".into()))?;
                // 输入和输出都预留空间；分批生成，但整次命令只提交一次。
                let output_budget = model.max_output_tokens.min(4096);
                let batch_limit = (output_budget as usize / 192).clamp(1, 32);
                let input_budget = model
                    .context_window
                    .saturating_sub(estimate_tokens(PROMPT) + u64::from(output_budget) + 256)
                    / 2;
                check_cancel(&cancel)?;
                let mut batch = BTreeMap::new();
                let mut cost = 0;
                let mut end = start;
                while end < candidates.len() && end - start < batch_limit {
                    let skill = candidates[end];
                    let value = json!({"name": skill.name, "description": skill.description,
                        "when_to_use": skill.when_to_use, "paths": skill.globs});
                    let item_cost = estimate_tokens(&value.to_string()) + 12;
                    if cost + item_cost > input_budget {
                        break;
                    }
                    cost += item_cost;
                    batch.insert(end.to_string(), value);
                    end += 1;
                }
                if end == start {
                    return Err(CoreError::Config(format!(
                        "模型上下文不足以压缩技能 {}",
                        candidates[start].name
                    )));
                }
                let mut request = build_request(
                    model,
                    &config,
                    vec![Message::user_text(format!(
                        "{PROMPT}\n\n{}",
                        serde_json::to_string(&batch).expect("技能可序列化")
                    ))],
                    Vec::new(),
                    "skills-compact",
                    CacheAnchors::default(),
                );
                request.max_output_tokens = Some(output_budget);
                let _permit = inner.provider_permit(model, &cancel).await?;
                let response = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    response = inner.client.complete(&model.llm, &request) => response?,
                };
                if !matches!(
                    response.finish_reason,
                    FinishReason::Stop | FinishReason::StopSequence
                ) {
                    return Err(CoreError::Config(
                        "技能摘要未正常完成，未提交任何缓存".into(),
                    ));
                }
                let hints = parse_hints(&response.message.text(), start, end)?;
                for (id, hint) in hints {
                    let skill = candidates[id];
                    cache.entries.insert(
                        skill.skill_file(),
                        Entry {
                            name: skill.name.clone(),
                            fingerprint: fingerprint(skill),
                            hint,
                            model: model.actual_selector().to_owned(),
                            created_at_ms: crate::session::store::now_ms(),
                        },
                    );
                }
                start = end;
                push_progress(
                    &ctx,
                    &cancel,
                    SkillsProgressPhase::Generating,
                    start,
                    candidates.len(),
                    result.skipped,
                )
                .await?;
            }
            result.compressed = candidates.len();
            // 只验证本次候选，未处理的其它项目不影响原子提交。
            let current = discover(cwd, &config, workflows_active);
            for original in candidates {
                let unchanged = current.get(&original.name).is_some_and(|skill| {
                    skill.skill_file() == original.skill_file()
                        && fingerprint(skill) == fingerprint(original)
                });
                if !unchanged {
                    return Err(CoreError::Config(format!(
                        "技能 {} 在压缩期间已变化，未提交任何缓存",
                        original.name
                    )));
                }
            }
        }
    }
    push_progress(
        &ctx,
        &cancel,
        SkillsProgressPhase::Committing,
        result.compressed,
        result.compressed,
        result.skipped,
    )
    .await?;
    check_cancel(&cancel)?;
    if result.compressed > 0 || result.removed > 0 {
        // 同步发布区间不再检查取消：rename 完成意味着提交成功，不能随后报告回滚。
        writer.commit(&cache)?;
    }
    cache.apply(&mut index);
    result.tokens_after = tokens(&index, &config, context_window);
    let mut project_index = super::discovery::discover_index(
        cwd,
        &config.home,
        crate::paths::home_dir().as_deref(),
        &config.skills,
    );
    cache.apply(&mut project_index);
    let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
    {
        let mut indexes = inner.skill_indexes.write();
        indexes.clear();
        indexes.insert(key, Arc::new(project_index));
    }
    let delta = i128::from(result.tokens_before) - i128::from(result.tokens_after);
    let percent = if result.tokens_before == 0 {
        0.0
    } else {
        delta as f64 * 100.0 / result.tokens_before as f64
    };
    result.message = format!(
        "技能摘要：压缩 {} 个，缓存跳过 {} 个，清理 {} 个；目录预估 {} → {} tokens（{} {}，{:.1}%）",
        result.compressed,
        result.skipped,
        result.removed,
        result.tokens_before,
        result.tokens_after,
        if delta >= 0 { "减少" } else { "增加" },
        delta.abs(),
        percent.abs()
    );
    Ok(result)
}

/// 通知发生在发布之前；取消或连接背压不会让半成品变成可见缓存。
async fn push_progress(
    ctx: &RequestContext,
    cancel: &CancellationToken,
    phase: SkillsProgressPhase,
    completed: usize,
    total: usize,
    skipped: usize,
) -> Result<(), CoreError> {
    let updates = ctx.updates();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CoreError::Cancelled),
        sent = updates.notify::<SkillsProgress>(SkillsProgressParams { phase, completed, total, skipped }) => {
            sent.map_err(|error| CoreError::Protocol(error.to_string()))
        }
    }
}

fn discover(
    cwd: &Path,
    config: &crate::config::ResolvedConfig,
    workflows_active: bool,
) -> SkillIndex {
    let mut index = super::discovery::discover_index(
        cwd,
        &config.home,
        crate::paths::home_dir().as_deref(),
        &config.skills,
    );
    if workflows_active {
        crate::workflow::skill::inject(&mut index, &config.home);
    }
    index
}

fn tokens(index: &SkillIndex, config: &crate::config::ResolvedConfig, window: u64) -> u64 {
    super::catalog::render(index, &config.skills, window)
        .as_deref()
        .map_or(0, estimate_tokens)
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        Err(CoreError::Cancelled)
    } else {
        Ok(())
    }
}

/// 不经 Value/BTreeMap 反序列化，以免重复键被静默覆盖。
fn parse_hints(text: &str, start: usize, end: usize) -> Result<Vec<(usize, String)>, CoreError> {
    struct Hints(Vec<(String, String)>);
    impl<'de> Deserialize<'de> for Hints {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Hints;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an object of routing hints")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Hints, A::Error> {
                    let mut entries = Vec::new();
                    while let Some(entry) = map.next_entry()? {
                        entries.push(entry);
                    }
                    Ok(Hints(entries))
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    let Hints(raw) = serde_json::from_str(text)
        .map_err(|error| CoreError::Config(format!("技能摘要 JSON 无效，未提交缓存：{error}")))?;
    let mut seen = vec![false; end - start];
    let mut hints = Vec::with_capacity(raw.len());
    for (key, hint) in raw {
        let id = key
            .parse::<usize>()
            .ok()
            .filter(|id| (start..end).contains(id) && id.to_string() == key)
            .ok_or_else(|| CoreError::Config("技能摘要包含未知 ID，未提交缓存".into()))?;
        if seen[id - start] || !valid_hint(&hint) {
            return Err(CoreError::Config(
                "技能摘要重复、为空或超出长度，未提交缓存".into(),
            ));
        }
        seen[id - start] = true;
        hints.push((id, hint.trim().to_owned()));
    }
    if seen.iter().any(|seen| !seen) {
        return Err(CoreError::Config("技能摘要遗漏条目，未提交缓存".into()));
    }
    Ok(hints)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_output_must_cover_the_batch_exactly() {
        assert_eq!(
            parse_hints(
                r#"{"2":"Review changes", "3":"Inspect external source"}"#,
                2,
                4
            )
            .unwrap()
            .len(),
            2
        );
        for invalid in [
            r#"{"2":"a"}"#,
            r#"{"2":"a","2":"b","3":"c"}"#,
            r#"{"2":"a","3":"b","4":"c"}"#,
            r#"{"2":"","3":"b"}"#,
            r#"{"2":"a\nb","3":"c"}"#,
        ] {
            assert!(parse_hints(invalid, 2, 4).is_err(), "{invalid}");
        }
    }
}
