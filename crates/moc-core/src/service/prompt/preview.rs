//! 模型流到临时工具预览的投影；执行参数始终由 LLM 累加器提供。
mod json;

use json::JsonPreview;
use moc_llm::StreamEvent;
use moc_protocol::{
    ProtocolError, SessionUpdate, ToolPreviewContent, ToolPreviewResolution, UpdateSink,
};
use std::collections::BTreeMap;

pub(super) const INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

struct Pending {
    preview_id: String,
    id: Option<String>,
    name: Option<String>,
    json: JsonPreview,
    dirty: bool,
    last: Option<(Option<String>, Option<String>, ToolPreviewContent)>,
}

#[derive(Default)]
pub(super) struct Previews {
    // 上游 index 稀疏，按 index 确定批量刷新顺序；元素数受 LLM 流预算约束。
    blocks: BTreeMap<usize, Pending>,
}

impl Previews {
    /// 返回是否需要立即刷新：首个预览或切换到正文/思考时必须保持到达顺序。
    pub(super) fn apply(&mut self, event: &StreamEvent) -> bool {
        let index = match event {
            StreamEvent::ToolCallStart { index, .. }
            | StreamEvent::ToolCallArgumentsDelta { index, .. }
            | StreamEvent::ToolCallSnapshot { index, .. } => *index,
            _ => return true,
        };
        let block = self.blocks.entry(index).or_insert_with(|| Pending {
            preview_id: format!("preview_{}", crate::ids::entry_id()),
            id: None,
            name: None,
            json: JsonPreview::default(),
            dirty: false,
            last: None,
        });
        match event {
            StreamEvent::ToolCallStart { id, name, .. } => {
                block.id = Some(id.clone());
                block.name = Some(name.clone());
            }
            StreamEvent::ToolCallArgumentsDelta { json, .. } => block.json.feed(json),
            StreamEvent::ToolCallSnapshot {
                id,
                name,
                arguments,
                ..
            } => {
                if let Some(id) = id {
                    block.id = Some(id.clone());
                }
                if let Some(name) = name {
                    block.name = Some(name.clone());
                }
                if let Some(arguments) = arguments {
                    block.json = JsonPreview::default();
                    block.json.feed(arguments);
                }
            }
            _ => unreachable!(),
        }
        block.dirty = true;
        block.last.is_none()
    }

    pub(super) async fn flush(
        &mut self,
        update: &UpdateSink,
        session: &str,
        prompt: &str,
    ) -> Result<(), ProtocolError> {
        for block in self.blocks.values_mut().filter(|block| block.dirty) {
            block.dirty = false;
            let current = (
                block.id.clone(),
                block.name.clone(),
                block
                    .json
                    .content(block.name.as_deref().unwrap_or_default()),
            );
            if block.last.as_ref() == Some(&current) {
                continue;
            }
            update
                .emit(
                    session.to_owned(),
                    SessionUpdate::ToolPreview {
                        prompt_id: prompt.to_owned(),
                        preview_id: block.preview_id.clone(),
                        tool_call_id: block.id.clone(),
                        name: block.name.clone(),
                        preview: current.2.clone(),
                    },
                )
                .await?;
            block.last = Some(current);
        }
        Ok(())
    }

    pub(super) async fn resolve(
        &mut self,
        update: &UpdateSink,
        session: &str,
        prompt: &str,
        identities: &[(usize, String)],
    ) -> Result<(), ProtocolError> {
        if self.blocks.is_empty() {
            return Ok(());
        }
        if !identities.is_empty() {
            self.flush(update, session, prompt).await?;
        }
        let calls = identities
            .iter()
            .filter_map(|(index, id)| {
                self.blocks.get(index).map(|block| ToolPreviewResolution {
                    preview_id: block.preview_id.clone(),
                    tool_call_id: id.clone(),
                })
            })
            .collect();
        update
            .emit(
                session.to_owned(),
                SessionUpdate::ToolPreviewsResolved {
                    prompt_id: prompt.to_owned(),
                    calls,
                },
            )
            .await?;
        self.blocks.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_identity_and_snapshot_override() {
        let mut p = Previews::default();
        p.apply(&StreamEvent::ToolCallArgumentsDelta {
            index: 9,
            json: r#"{"command":"old"}"#.into(),
        });
        let key = p.blocks[&9].preview_id.clone();
        p.apply(&StreamEvent::ToolCallStart {
            index: 9,
            id: "late".into(),
            name: "bash".into(),
        });
        p.apply(&StreamEvent::ToolCallSnapshot {
            index: 9,
            id: Some("final".into()),
            name: None,
            arguments: Some(r#"{"command":"new"}"#.into()),
        });
        p.apply(&StreamEvent::ToolCallStart {
            index: 2,
            id: "other".into(),
            name: "write".into(),
        });
        assert_eq!(p.blocks[&9].preview_id, key);
        assert_eq!(p.blocks[&9].json.content("bash").body, "new");
        assert_eq!(p.blocks[&9].id.as_deref(), Some("final"));
        let mut retry = Previews::default();
        retry.apply(&StreamEvent::ToolCallStart {
            index: 9,
            id: "final".into(),
            name: "bash".into(),
        });
        assert_ne!(retry.blocks[&9].preview_id, key);
    }
}
