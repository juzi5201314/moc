//! ACP agent 侧：方法处理器注册与 stdio 连接驱动。

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    self as acp, AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    DeleteSessionRequest, DeleteSessionResponse, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionDeleteCapabilities,
    SessionId as AcpSessionId, SessionInfo as AcpSessionInfo, SessionListCapabilities,
    SessionResumeCapabilities, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse,
};
use agent_client_protocol::{Agent, Responder, Stdio, on_receive_notification, on_receive_request};
use moc_protocol::error::{RpcCallError, RpcErrorCode};
use moc_protocol::methods::{
    PromptParams, PromptResult, SessionAutonomy, SessionCancel, SessionCancelParams, SessionDelete,
    SessionDeleteParams, SessionInfo, SessionList, SessionListParams, SessionLoad,
    SessionLoadParams, SessionModelOverflowAction, SessionNew, SessionNewParams, SessionPlanMode,
    SessionPlanModeParams, SessionPrompt, SessionSetAutonomy, SessionSetAutonomyParams,
    SessionSetModel, SessionSetModelParams, SessionUnload, SessionUnloadParams,
};

use crate::Bridge;
use crate::backend::Backend;
use crate::translate::{
    self, CONFIG_MODEL, MODE_DEFAULT, MODE_DONT_ASK, MODE_PLAN, MODE_UNATTENDED,
};

/// ACP 声明的能力：moc 自己跑工具与终端，不委托客户端；MCP 下挂不支持。
fn agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(
            PromptCapabilities::new()
                .image(true)
                .audio(false)
                .embedded_context(true),
        )
        .mcp_capabilities(McpCapabilities::new().http(false).sse(false))
        .session_capabilities(
            SessionCapabilities::new()
                .list(SessionListCapabilities::new())
                .delete(SessionDeleteCapabilities::new())
                .resume(SessionResumeCapabilities::new())
                .close(SessionCloseCapabilities::new()),
        )
}

/// `session/list` 每页大小；cursor 是过滤后偏移量的十进制字符串。
const LIST_PAGE: usize = 200;

/// 驱动 ACP stdio 连接：处理器在此注册，`connect_with` 返回前连接一直存活。
///
/// `session/prompt` 响应被 `cx.spawn` 进独立任务、持有 responder 直到 moc 提示
/// 结束——否则 dispatch 循环被长请求占住，`session/cancel` 与 `session/update`
/// 都进不来。
pub(crate) async fn serve<B: Backend>(bridge: &Arc<Bridge<B>>) -> anyhow::Result<()> {
    let parked = bridge.clone();
    Agent
        .builder()
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: InitializeRequest, responder, _cx| {
                    bridge.initialize(req, responder)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: NewSessionRequest, responder, _cx| {
                    let result = bridge.new_session(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: LoadSessionRequest, responder, cx| {
                    let session_id = req.session_id.0.to_string();
                    let (result, replay) = bridge.load_session(req).await;
                    match result {
                        Ok(response) => {
                            responder.respond(response)?;
                            bridge.emit_updates(&cx, &session_id, replay);
                            Ok(())
                        }
                        Err(error) => responder.respond_with_error(error),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: ResumeSessionRequest, responder, _cx| {
                    let result = bridge.resume_session(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: ListSessionsRequest, responder, _cx| {
                    let result = bridge.list_sessions(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: CloseSessionRequest, responder, _cx| {
                    let result = bridge.close_session(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: DeleteSessionRequest, responder, _cx| {
                    let result = bridge.delete_session(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: SetSessionModeRequest, responder, _cx| {
                    let result = bridge.set_mode(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                    let result = bridge.set_config_option(req).await;
                    responder.respond_with_result(result)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let bridge = bridge.clone();
                async move |req: PromptRequest, responder, cx| {
                    let task = {
                        let bridge = bridge.clone();
                        async move {
                            let result = bridge.prompt(req).await;
                            responder.respond_with_result(result)
                        }
                    };
                    match cx.spawn(task) {
                        Ok(()) => Ok(()),
                        // spawn 失败即连接收尾中：请求注定无响应，让 handler 直接报错。
                        Err(error) => Err(error),
                    }
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let bridge = bridge.clone();
                async move |notif: CancelNotification, _cx| {
                    bridge.cancel_prompt(notif);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .connect_with(Stdio::new(), async move |cx| {
            parked.attach_acp(cx.clone());
            cx.incoming_closed().await;
            parked.detach_acp();
            Ok(())
        })
        .await?;
    Ok(())
}

impl<B: Backend> Bridge<B> {
    fn initialize(
        &self,
        req: InitializeRequest,
        responder: Responder<InitializeResponse>,
    ) -> Result<(), acp::Error> {
        tracing::info!(
            client = ?req.client_info.as_ref().map(|i| i.name.as_str()),
            version = ?req.client_info.as_ref().map(|i| i.version.as_str()),
            "ACP 客户端已初始化"
        );
        self.set_client_caps(req.client_capabilities);
        responder.respond(
            InitializeResponse::new(ProtocolVersion::V1)
                .agent_info(Implementation::new("moc", env!("CARGO_PKG_VERSION")))
                .agent_capabilities(agent_capabilities()),
        )
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, acp::Error> {
        if !req.mcp_servers.is_empty() {
            tracing::warn!(
                count = req.mcp_servers.len(),
                "客户端附带的 MCP server 被忽略：moc-acp 声明 mcpCapabilities=false"
            );
        }
        let info = self
            .moc_call::<SessionNew>(SessionNewParams {
                cwd: Some(req.cwd.to_string_lossy().into_owned()),
                model: self.options().model.clone(),
                title: None,
                engine: self.options().engine,
                plan_active: self.options().plan,
                fast_mode: false,
            })
            .await
            .map_err(|e| rpc_err(&e, None))?;
        let proj = self.track_session(&info);
        Ok(NewSessionResponse::new(info.id.clone())
            .modes(translate::mode_state(&proj))
            .config_options(self.config_options(&proj)))
    }

    /// `session/load`：装载会话并把 transcript 条目按响应之后到达的顺序重放。
    ///
    /// 返回 `(响应, 重放更新)`——ACP 要求 load 的历史更新在响应之后才允许到达，
    /// 所以由调用点先 respond 再 `emit_updates`。
    async fn load_session(
        &self,
        req: LoadSessionRequest,
    ) -> (
        Result<LoadSessionResponse, acp::Error>,
        Vec<acp::SessionUpdate>,
    ) {
        let session_id = req.session_id.0.to_string();
        let result = self
            .moc_call::<SessionLoad>(SessionLoadParams {
                omit_auxiliary: false,
                session_id: session_id.clone(),
                max_entries: None,
                read_only: false,
            })
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => return (Err(rpc_err(&error, Some(&session_id))), Vec::new()),
        };
        let proj = self.track_session(&result.session);
        let response = LoadSessionResponse::new()
            .modes(translate::mode_state(&proj))
            .config_options(self.config_options(&proj));
        let mut replay: Vec<acp::SessionUpdate> = result
            .entries
            .iter()
            .flat_map(translate::replay_entry)
            .collect();
        // 当前待办与已提交计划：作为最后一条投影更新，让客户端恢复状态面板。
        if !result.todos.is_empty() {
            replay.push(acp::SessionUpdate::Plan(translate::todo_plan(
                &result.todos,
            )));
        }
        for plan in &result.plans {
            if !matches!(plan.status, moc_protocol::methods::PlanStatus::Superseded) {
                replay.push(translate::plan_submitted_update(plan));
            }
        }
        (Ok(response), replay)
    }

    /// `session/resume`：恢复会话但不重放历史（ACP 明确区分于 load）。
    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, acp::Error> {
        let session_id = req.session_id.0.to_string();
        let result = self
            .moc_call::<SessionLoad>(SessionLoadParams {
                omit_auxiliary: false,
                session_id: session_id.clone(),
                max_entries: Some(0),
                read_only: false,
            })
            .await
            .map_err(|e| rpc_err(&e, Some(&session_id)))?;
        let proj = self.track_session(&result.session);
        Ok(ResumeSessionResponse::new()
            .modes(translate::mode_state(&proj))
            .config_options(self.config_options(&proj)))
    }

    async fn list_sessions(
        &self,
        req: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, acp::Error> {
        let offset = req
            .cursor
            .as_deref()
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        let result = self
            .moc_call::<SessionList>(SessionListParams {})
            .await
            .map_err(|e| rpc_err(&e, None))?;
        let filtered: Vec<&SessionInfo> = result
            .sessions
            .iter()
            .filter(|s| {
                req.cwd
                    .as_ref()
                    .is_none_or(|cwd| s.cwd == cwd.to_string_lossy())
            })
            .collect();
        let next_cursor =
            (offset + LIST_PAGE < filtered.len()).then(|| (offset + LIST_PAGE).to_string());
        let sessions = filtered
            .iter()
            .skip(offset)
            .take(LIST_PAGE)
            .map(|info| {
                AcpSessionInfo::new(info.id.clone(), PathBuf::from(&info.cwd))
                    .title(info.title.clone())
                    .updated_at(rfc3339(info.updated_at_ms))
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions).next_cursor(next_cursor))
    }

    async fn close_session(
        &self,
        req: CloseSessionRequest,
    ) -> Result<CloseSessionResponse, acp::Error> {
        let session_id = req.session_id.0.to_string();
        self.moc_call::<SessionUnload>(SessionUnloadParams {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| rpc_err(&e, Some(&session_id)))?;
        self.drop_session(&session_id);
        Ok(CloseSessionResponse::new())
    }

    async fn delete_session(
        &self,
        req: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, acp::Error> {
        let session_id = req.session_id.0.to_string();
        self.moc_call::<SessionDelete>(SessionDeleteParams {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| rpc_err(&e, Some(&session_id)))?;
        self.drop_session(&session_id);
        Ok(DeleteSessionResponse::new())
    }

    /// ACP 模式 ↔ (plan 激活态, 自主度) 二元组；`plan` 是独立维度上的开关，
    /// 自主度三档共用 plan_active=false。
    async fn set_mode(
        &self,
        req: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse, acp::Error> {
        let session_id = req.session_id.0.to_string();
        let info = match req.mode_id.0.as_ref() {
            MODE_PLAN => self
                .moc_call::<SessionPlanMode>(SessionPlanModeParams {
                    session_id: session_id.clone(),
                    active: true,
                })
                .await
                .map_err(|e| rpc_err(&e, Some(&session_id)))?,
            MODE_DEFAULT | MODE_DONT_ASK | MODE_UNATTENDED => {
                let autonomy = match req.mode_id.0.as_ref() {
                    MODE_DONT_ASK => SessionAutonomy::DontAsk,
                    MODE_UNATTENDED => SessionAutonomy::Unattended,
                    _ => SessionAutonomy::Default,
                };
                let info = self
                    .moc_call::<SessionSetAutonomy>(SessionSetAutonomyParams {
                        session_id: session_id.clone(),
                        autonomy,
                    })
                    .await
                    .map_err(|e| rpc_err(&e, Some(&session_id)))?;
                // ACP 模式互斥：离开 plan 需要显式退出。
                if info.plan_active {
                    self.moc_call::<SessionPlanMode>(SessionPlanModeParams {
                        session_id: session_id.clone(),
                        active: false,
                    })
                    .await
                    .map_err(|e| rpc_err(&e, Some(&session_id)))?
                } else {
                    info
                }
            }
            other => return Err(acp::Error::invalid_params().data(format!("未知会话模式 {other}"))),
        };
        self.track_session(&info);
        Ok(SetSessionModeResponse::new())
    }

    /// 目前唯一暴露的 config_option 是模型；其余 id 直接拒绝。
    async fn set_config_option(
        &self,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, acp::Error> {
        if req.config_id.0.as_ref() != CONFIG_MODEL {
            return Err(
                acp::Error::invalid_params().data(format!("不支持的配置项 {}", req.config_id.0))
            );
        }
        let Some(selector) = req.value.as_value_id() else {
            return Err(acp::Error::invalid_params().data("model 选项需要 value id"));
        };
        let session_id = req.session_id.0.to_string();
        let result = self
            .moc_call::<SessionSetModel>(SessionSetModelParams {
                session_id: session_id.clone(),
                selector: selector.0.to_string(),
                // ACP 没有容量缺口确认路径：直接切换，交给既有压缩逻辑兜底。
                overflow: SessionModelOverflowAction::Switch,
            })
            .await
            .map_err(|e| rpc_err(&e, Some(&session_id)))?;
        let proj = self.track_session(&result.session);
        Ok(SetSessionConfigOptionResponse::new(
            self.config_options(&proj).unwrap_or_default(),
        ))
    }

    /// `session/prompt`：转 InputPart 后直发 core，响应挂到提示结束。
    ///
    /// 本函数在 `cx.spawn` 的任务里跑——dispatch 循环保持可收 `session/cancel`。
    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, acp::Error> {
        let session_id = req.session_id.0.to_string();
        let input = req
            .prompt
            .iter()
            .map(translate::input_part)
            .collect::<Result<Vec<_>, _>>()?;
        let result: PromptResult = self
            .moc_call::<SessionPrompt>(PromptParams {
                session_id: session_id.clone(),
                input,
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
            .map_err(|e| rpc_err(&e, Some(&session_id)))?;
        // 流式增量与 usage 已由 session/update 发出；最终消息不再回放，usage 不重复上报。
        Ok(PromptResponse::new(translate::stop_reason(
            &result.finish_reason,
        )))
    }

    /// `session/cancel` 通知 → moc `session/cancel`（后台调用，不占 dispatch）。
    fn cancel_prompt(&self, notif: CancelNotification) {
        let Some(moc) = self.moc() else { return };
        let session_id = notif.session_id.0.to_string();
        tokio::spawn(async move {
            let result = moc
                .call::<SessionCancel>(SessionCancelParams { session_id })
                .await;
            if let Err(error) = result {
                tracing::debug!(?error, "session/cancel 调用失败");
            }
        });
    }
}

/// moc 调用错误的 ACP 化：SessionNotFound 用专用错误码，其余保留原 code。
fn rpc_err(error: &RpcCallError, session_id: Option<&str>) -> acp::Error {
    match error {
        RpcCallError::Rpc(e) => match session_id {
            Some(id) if e.code == RpcErrorCode::SessionNotFound.code() => {
                acp::Error::session_not_found(AcpSessionId::new(id.to_owned()))
            }
            _ => acp::Error::new(e.code, e.message.clone()),
        },
        RpcCallError::Cancelled => acp::Error::request_cancelled(),
        RpcCallError::Closed => acp::Error::internal_error().data("与 core 的连接已关闭"),
        _ => acp::Error::internal_error().data(error.to_string()),
    }
}

/// UNIX 毫秒 → RFC 3339（`SessionInfo.updated_at` 的 ISO-8601 串）。
fn rfc3339(at_ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms as i64)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

// 供 bridge.rs 复用的会话级工具方法在 lib.rs 的 Bridge 上；这里是 ACP 侧入口。

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, StopReason, TextContent};
    use moc_protocol::methods::{
        SessionEngine, SessionListResult, SessionLoadResult, SessionPermission, TranscriptEntry,
    };
    use moc_protocol::{FinishReason, Message, Usage};

    use super::*;
    use crate::AcpOptions;
    use crate::backend::stub::StubBackend;

    fn session_info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            title: "t".into(),
            model: "m".into(),
            permission: SessionPermission::Full,
            engine: SessionEngine::Standard,
            autonomy: SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        }
    }

    fn bridge() -> (Arc<Bridge<StubBackend>>, StubBackend) {
        let backend = StubBackend::default();
        let bridge = Arc::new(Bridge::new(AcpOptions::default()));
        bridge.attach_moc(backend.clone());
        (bridge, backend)
    }

    fn transcript_entry(role_text: Message) -> TranscriptEntry {
        TranscriptEntry {
            id: "e1".into(),
            at_ms: 0,
            message: role_text,
            tool: None,
            bash: None,
            agent: None,
            goal_round: None,
            reminder: None,
            agent_result: None,
        }
    }

    #[tokio::test]
    async fn new_session_maps_id_modes_and_records_call() {
        let (bridge, backend) = bridge();
        backend.push::<SessionNew>(session_info("ses_1"));
        let response = bridge
            .new_session(NewSessionRequest::new(PathBuf::from("/tmp")))
            .await
            .unwrap();
        assert_eq!(response.session_id.0.as_ref(), "ses_1");
        let modes = response.modes.expect("modes");
        assert_eq!(modes.current_mode_id.0.as_ref(), MODE_DEFAULT);
        assert_eq!(modes.available_modes.len(), 4);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls[0].0, "session/new");
        assert_eq!(calls[0].1["cwd"], "/tmp");
    }

    #[tokio::test]
    async fn prompt_translates_input_and_stop_reason() {
        let (bridge, backend) = bridge();
        backend.push::<SessionPrompt>(PromptResult {
            session_id: "ses_1".into(),
            prompt_id: "p1".into(),
            message: Message::assistant_text("done"),
            finish_reason: FinishReason::Length,
            usage: Usage::default(),
        });
        let response = bridge
            .prompt(PromptRequest::new(
                "ses_1",
                vec![ContentBlock::Text(TextContent::new("hi"))],
            ))
            .await
            .unwrap();
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls[0].0, "session/prompt");
        assert_eq!(calls[0].1["input"][0]["type"], "text");
        assert_eq!(calls[0].1["input"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn set_mode_plan_then_leaving_plan_exits_plan_mode() {
        let (bridge, backend) = bridge();
        backend.push::<SessionPlanMode>(SessionInfo {
            plan_active: true,
            ..session_info("ses_1")
        });
        bridge
            .set_mode(SetSessionModeRequest::new("ses_1", MODE_PLAN))
            .await
            .unwrap();

        // 离开 plan 到 unattended：先 set_autonomy，返回 plan_active=true 时再补一次退出。
        backend.push::<SessionSetAutonomy>(SessionInfo {
            autonomy: SessionAutonomy::Unattended,
            plan_active: true,
            ..session_info("ses_1")
        });
        backend.push::<SessionPlanMode>(SessionInfo {
            autonomy: SessionAutonomy::Unattended,
            ..session_info("ses_1")
        });
        bridge
            .set_mode(SetSessionModeRequest::new("ses_1", MODE_UNATTENDED))
            .await
            .unwrap();

        let calls = backend.calls.lock().unwrap();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            [
                "session/plan_mode",
                "session/set_autonomy",
                "session/plan_mode"
            ]
        );
        assert_eq!(calls[0].1["active"], true);
        assert_eq!(calls[1].1["autonomy"], "unattended");
        assert_eq!(calls[2].1["active"], false);
        let proj = bridge.projection("ses_1").unwrap();
        assert!(!proj.plan_active);
    }

    #[tokio::test]
    async fn load_session_replays_user_and_agent_chunks() {
        let (bridge, backend) = bridge();
        backend.push::<SessionLoad>(SessionLoadResult {
            session: session_info("ses_1"),
            entries: vec![
                transcript_entry(Message::user_text("问")),
                transcript_entry(Message::assistant_text("答")),
            ],
            truncated: false,
            todos: Vec::new(),
            plans: Vec::new(),
            child_calls: Vec::new(),
            goal: None,
        });
        let (result, replay) = bridge
            .load_session(LoadSessionRequest::new("ses_1", PathBuf::from("/tmp")))
            .await;
        result.unwrap();
        assert!(matches!(replay[0], SessionUpdate::UserMessageChunk(_)));
        assert!(matches!(replay[1], SessionUpdate::AgentMessageChunk(_)));
        assert_eq!(replay.len(), 2);
    }

    #[tokio::test]
    async fn list_sessions_filters_by_cwd() {
        let (bridge, backend) = bridge();
        backend.push::<SessionList>(SessionListResult {
            sessions: vec![
                session_info("ses_a"),
                SessionInfo {
                    cwd: "/other".into(),
                    ..session_info("ses_b")
                },
            ],
        });
        let mut req = ListSessionsRequest::new();
        req.cwd = Some(PathBuf::from("/tmp"));
        let response = bridge.list_sessions(req).await.unwrap();
        assert_eq!(response.sessions.len(), 1);
        assert_eq!(response.sessions[0].session_id.0.as_ref(), "ses_a");
    }
}
