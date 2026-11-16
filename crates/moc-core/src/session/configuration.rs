use super::SessionTable;
use super::entries::EntryPayload;
use super::lock;
use super::sanitize_title;
use crate::error::CoreError;
use moc_protocol::methods::SessionAutonomy;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionPermission;
use moc_protocol::methods::SessionRenameParams;
use rust_i18n::t;

impl SessionTable {
    /// 改名，并追加一条可重放的标题事件。
    pub async fn rename(
        &self,
        session_id: &str,
        params: SessionRenameParams,
    ) -> Result<SessionInfo, CoreError> {
        if params.session_id != session_id {
            return Err(CoreError::Session(
                "INVALID_PARAMS: session_id 不匹配".into(),
            ));
        }
        let record = self.get(session_id).await?;
        let title = sanitize_title(&params.title);
        if title.is_empty() {
            return Err(CoreError::Session(t!("core.err.title_empty").into_owned()));
        }
        self.append(session_id, EntryPayload::TitleChange { title })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话引擎的变更。
    ///
    /// 引擎是会话级属性：transcript 里每一种引擎的工具调用形态不同，混排会让
    /// 模型模仿已失效的调用格式，所以一旦会话跑过用户提示（`prompt_sequence > 0`
    /// 即 [`SessionInfo::engine_locked`]）就拒绝一切引擎切换；回退到空白后自然解锁。
    pub async fn set_engine(
        &self,
        session_id: &str,
        engine: SessionEngine,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        let plan_active = {
            let state = record.state.lock().await;
            if state.prompt_sequence > 0 && engine != state.engine {
                return Err(CoreError::Session(
                    "INVALID_PARAMS: 会话已有提示历史，引擎已锁定；请新建会话选择引擎".into(),
                ));
            }
            state.plan_active
        };
        self.append(
            session_id,
            EntryPayload::EngineChange {
                engine,
                plan_active,
            },
        )
        .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话自主程度的变更。
    ///
    /// 自主程度随时可改：它只影响之后确认由谁应答，不改动工具表与历史，
    /// 所以不受引擎锁定限制。
    pub async fn set_autonomy(
        &self,
        session_id: &str,
        autonomy: SessionAutonomy,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::AutonomyChange { autonomy })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录 plan 模式的进入或退出。
    pub async fn set_plan_active(
        &self,
        session_id: &str,
        active: bool,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::PlanMode { active })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话执行权限的变更。
    pub async fn set_permission(
        &self,
        session_id: &str,
        permission: SessionPermission,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::PermissionChange { permission })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话模型的变更，并清除旧模型专用的回放数据。
    pub async fn set_model(
        &self,
        session_id: &str,
        model: String,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::ModelChange { model })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话 Fast Mode 开关的变更。
    pub async fn set_fast_mode(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::FastModeChange { enabled })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }
}
