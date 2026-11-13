//! 会话列表的轻量投影：每个 zstd 帧只暂存最终元数据，校验通过后提交。

use std::path::Path;

use moc_protocol::{SessionAutonomy, SessionEngine, SessionInfo, SessionPermission};

use super::super::entries::{Entry, PersistedEngine};
use super::super::log::{self, LineVisitor, Policy};
use crate::error::CoreError;

pub(super) async fn read_session_info(
    path: &Path,
) -> Result<Option<(SessionInfo, bool)>, CoreError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_session_info_sync(&path))
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
}

fn read_session_info_sync(path: &Path) -> Result<Option<(SessionInfo, bool)>, CoreError> {
    let mut listing = Listing {
        path,
        info: None,
        header_seen: false,
        skip: false,
        has_message: false,
        pending: Pending::default(),
    };
    log::visit_lines(path, Policy::SkipFrame, &mut listing)?;
    Ok(listing.info.map(|info| (info, listing.has_message)))
}

/// 忽略大消息字段，不构造正文、工具参数或思维链的对象树。
#[derive(serde::Deserialize)]
struct Metadata {
    #[serde(rename = "type")]
    kind: String,
    at_ms: u64,
    title: Option<String>,
    model: Option<String>,
    #[serde(default, alias = "mode")]
    engine: Option<PersistedEngine>,
    #[serde(default)]
    autonomy: Option<SessionAutonomy>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    plan_active: Option<bool>,
    #[serde(default)]
    permission: Option<SessionPermission>,
    #[serde(default)]
    message: Option<MessageMeta>,
    #[serde(default)]
    bash: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    agent: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    reminder: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
struct MessageMeta {
    role: moc_llm::Role,
}

/// 帧大小与消息数量无关：每个字段只保留本帧最后一次变化，不复制已提交的 SessionInfo。
#[derive(Default)]
struct Pending {
    header_seen: bool,
    header: Option<SessionInfo>,
    skip: bool,
    has_message: bool,
    updated_at_ms: Option<u64>,
    title: Option<String>,
    model: Option<String>,
    engine: Option<(SessionEngine, bool)>,
    autonomy: Option<SessionAutonomy>,
    plan_active: Option<bool>,
    permission: Option<SessionPermission>,
    engine_locked: bool,
}

struct Listing<'path> {
    path: &'path Path,
    info: Option<SessionInfo>,
    header_seen: bool,
    skip: bool,
    has_message: bool,
    pending: Pending,
}

impl LineVisitor for Listing<'_> {
    fn line(&mut self, line: &[u8]) -> Result<(), CoreError> {
        if !self.header_seen && !self.pending.header_seen {
            self.pending.header_seen = true;
            let header: Entry = serde_json::from_slice(line)
                .map_err(|error| CoreError::Session(format!("{}：{error}", self.path.display())))?;
            self.pending.header = header.session_info(None);
            self.pending.skip = self.pending.header.is_none();
            return Ok(());
        }
        if self.skip || self.pending.skip {
            return Ok(());
        }
        let metadata: Metadata = match serde_json::from_slice(line) {
            Ok(metadata) => metadata,
            Err(error) => {
                // 明文的坏记录仍按有内容处理；压缩帧损坏则在 finish_frame 整体丢弃。
                self.pending.has_message = true;
                tracing::warn!(path = %self.path.display(), %error, "跳过损坏的 transcript 记录");
                return Ok(());
            }
        };
        self.pending.updated_at_ms = Some(metadata.at_ms);
        match metadata.kind.as_str() {
            "title_change" => {
                if let Some(title) = metadata.title {
                    self.pending.title = Some(title);
                }
            }
            "model_change" => {
                if let Some(model) = metadata.model {
                    self.pending.model = Some(model);
                }
            }
            "mode_change" | "engine_change" => {
                if let Some(engine) = metadata.engine {
                    self.pending.engine = Some((
                        engine.engine(),
                        metadata
                            .plan_active
                            .unwrap_or_else(|| engine.legacy_enters_plan()),
                    ));
                    // 后来的 engine_change 覆盖此前 plan_mode 的投影。
                    self.pending.plan_active = None;
                }
            }
            "autonomy_change" => {
                if let Some(autonomy) = metadata.autonomy {
                    self.pending.autonomy = Some(autonomy);
                }
            }
            "plan_mode" => {
                if let Some(active) = metadata.active {
                    self.pending.plan_active = Some(active);
                }
            }
            "permission_change" => {
                if let Some(permission) = metadata.permission {
                    self.pending.permission = Some(permission);
                }
            }
            "message" => {
                self.pending.has_message = true;
                if let Some(message) = metadata.message
                    && message.role == moc_llm::Role::User
                    && metadata.bash.is_none()
                    && metadata.agent.is_none()
                    && metadata.reminder.is_none()
                {
                    self.pending.engine_locked = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish_frame(&mut self, valid: bool) {
        let pending = std::mem::take(&mut self.pending);
        if !valid {
            return;
        }
        if pending.header_seen {
            self.header_seen = true;
            self.info = pending.header;
            self.skip = pending.skip;
        }
        self.has_message |= pending.has_message;
        let Some(info) = self.info.as_mut() else {
            return;
        };
        if let Some(updated) = pending.updated_at_ms {
            info.updated_at_ms = updated;
        }
        if let Some(title) = pending.title {
            info.title = title;
        }
        if let Some(model) = pending.model {
            info.model = model;
        }
        if let Some((engine, plan_active)) = pending.engine {
            info.engine = engine;
            info.plan_active = plan_active;
        }
        if let Some(autonomy) = pending.autonomy {
            info.autonomy = autonomy;
        }
        if let Some(active) = pending.plan_active {
            info.plan_active = active;
        }
        if let Some(permission) = pending.permission {
            info.permission = permission;
        }
        info.engine_locked |= pending.engine_locked;
    }
}
