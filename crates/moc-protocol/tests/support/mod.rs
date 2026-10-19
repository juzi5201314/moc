//! 集成测试支撑：脚本化 core 与原始对端。
//!
//! 两个角色：
//!
//! - [`ScriptedService`]：`AgentService` 的可编程实现。它把协议层全部路径都跑到
//!   （标准剧本含权限询问、取消、锁、并发门禁），同时记录调用与观察值供断言。
//! - [`RawPeer`]：直接读写原始帧的对端，用来验证**服务端自身**的行为——用自家
//!   客户端测服务端会把两边的缺陷互相掩盖。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Ready;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use moc_protocol::methods::{
    AgentProfile, AskOption, AskParams, AskQuestion, AskResult, AssistantDelta, ClientInfo,
    ConfigGetParams, ConfigGetResult, ContextContentResult, Empty, InitializeParams,
    InitializeResult, InputPart, ModelInfo, ModelInput, OpenVikingDeleteParams,
    OpenVikingDeleteResult, OpenVikingListParams, OpenVikingListResult, OpenVikingReadParams,
    OpenVikingReadResult, OpenVikingWriteParams, OpenVikingWriteResult, PermissionRequestParams,
    PermissionResult, PromptParams, PromptResult, ServerCapabilities, ServerInfo,
    SessionCompactParams, SessionContextContentParams, SessionContextParams, SessionDeleteParams,
    SessionInfo, SessionListParams, SessionListResult, SessionLoadParams, SessionLoadResult,
    SessionLockInfo, SessionMode, SessionNewParams, SessionPermission, SessionPlanApproveParams,
    SessionPlanApproveResult, SessionRenameParams, SessionRequestAsk, SessionSetModeParams,
    SessionSetModelParams, SessionSetPermissionParams, SessionUnloadParams, SessionUnlockParams,
    SessionUpdate, SessionUpdateParams, ToolExecution, ToolStatus, TranscriptEntry,
    default_options,
};
use moc_protocol::{
    AgentService, ClientBuilder, ClientHandle, ClientRequestContext, ContentPart, FinishReason,
    FrameDecoder, FrameEncoder, Framing, Id, Message, Notification, PROTOCOL_VERSION,
    ProtocolError, ReasoningBlock, Request, RequestContext, Response, Role, RpcError, RpcErrorCode,
    RpcMessage, Server, ServerConnection, ServerOptions, ToolCall, ToolResult, Usage, transport,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

/// 读超时：超过它说明协议层该回的东西没回。
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

// ───────────────────────────── 脚本化 core ─────────────────────────────

/// 一次 `session/prompt` 的行为脚本。
#[derive(Clone, Default)]
pub enum PromptMode {
    /// 标准剧本：推完整更新序列，中间发一次权限询问，返回 [`PromptResult`]。
    #[default]
    Full,
    /// 等 `Notify` 触发后走标准剧本（并发测试用来制造重叠提示）。
    Gated(Arc<Notify>),
    /// 等取消令牌触发，然后返回 `finish_reason = Cancelled` 的一次提示。
    UntilCancelled,
    /// 永远挂起：用于验证 future 被 abort 时是否留下了痕迹。
    Hang,
    /// 发起一次 `session/request_ask` 以供反向请求集成测试。
    Ask,
}

/// 一个会话的记录。
pub struct SessionRecord {
    /// 元信息。
    pub info: SessionInfo,
    /// transcript。
    pub entries: Vec<TranscriptEntry>,
}

/// 脚本化 core 的观察状态。
pub struct State {
    /// 服务方法调用记录（不含 `initialize`；握手另见 [`State::initialize`]）。
    pub calls: Vec<String>,
    /// 会话表。
    pub sessions: BTreeMap<String, SessionRecord>,
    /// 可用模型。
    pub models: Vec<ModelInfo>,
    /// 默认模型。
    pub default_model: Option<String>,
    /// 可用 agent。
    pub agents: Vec<AgentProfile>,
    /// 默认 agent。
    pub default_agent: Option<String>,
    /// core 的工作目录。
    pub cwd: String,
    /// 宣告的服务端能力。
    pub capabilities: ServerCapabilities,
    /// 收到的 `initialize` 参数。
    pub initialize: Option<InitializeParams>,
    /// 一次行为脚本。
    pub prompt_mode: PromptMode,
    /// 正在跑的提示数量与峰值（并发观察）。
    pub running_prompts: usize,
    /// 并发峰值。
    pub peak_prompts: usize,
    /// 授权询问结果：`Ok(option_id)` 或 `Err(错误码)`。
    pub asks: Vec<Result<String, i32>>,
    /// 收到的用户提问答案。
    pub ask_results: Vec<AskResult>,
    /// 上一个挂起提示的 drop 标记。
    pub prompt_dropped: Option<Arc<AtomicBool>>,
    /// drop 时取消令牌是否已触发。
    pub cancel_at_drop: Option<Arc<AtomicBool>>,
    /// 观察到取消令牌触发的次数。
    pub cancel_seen: usize,
    /// `config/get` 的额外填充长度（制造超大响应）。
    pub config_padding: usize,
    /// 会话 / 提示计数器。
    next_session: u32,
    next_prompt: u32,
}

/// 脚本化 core。
#[derive(Clone)]
pub struct ScriptedService {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
}

/// drop 时执行闭包的守卫（不用命名具体类型即可捕获取消令牌）。
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

impl Default for ScriptedService {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedService {
    /// 预置三个会话（`updated_at_ms` 依次为 300 / 200 / 100，便于断言排序）。
    pub fn new() -> Self {
        let mut sessions = BTreeMap::new();
        for (index, updated_at_ms) in [(1u32, 300u64), (2, 200), (3, 100)] {
            let id = format!("ses_{index}");
            sessions.insert(
                id.clone(),
                SessionRecord {
                    info: SessionInfo {
                        id: id.clone(),
                        title: format!("会话 {index}"),
                        model: "deepseek-flash".to_owned(),
                        permission: SessionPermission::Full,
                        mode: SessionMode::Normal,
                        parent_session_id: None,
                        forked_from: None,
                        cwd: "/home/soeur/project/moc".to_owned(),
                        created_at_ms: 10,
                        updated_at_ms,
                        lock: None,
                    },
                    entries: transcript(&id),
                },
            );
        }
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    calls: Vec::new(),
                    sessions,
                    models: vec![
                        ModelInfo {
                            id: "deepseek-flash".to_owned(),
                            display_name: "DeepSeek Flash".to_owned(),
                            thinking_levels: None,
                            context_window: Some(128_000),
                            max_output_tokens: Some(8_192),
                            pricing: None,
                            inputs: vec![ModelInput::Text],
                            remote_compaction: false,
                        },
                        ModelInfo {
                            id: "claude-sonnet".to_owned(),
                            display_name: "Claude Sonnet".to_owned(),
                            thinking_levels: None,
                            context_window: Some(200_000),
                            max_output_tokens: None,
                            pricing: None,
                            inputs: vec![ModelInput::Text, ModelInput::Image],
                            remote_compaction: false,
                        },
                    ],
                    default_model: Some("deepseek-flash".to_owned()),
                    agents: vec![AgentProfile {
                        id: "build".to_owned(),
                        display_name: "Build".to_owned(),
                        description: Some("默认 agent".to_owned()),
                    }],
                    default_agent: Some("build".to_owned()),
                    cwd: "/home/soeur/project/moc".to_owned(),
                    capabilities: ServerCapabilities {
                        permission_requests: true,
                        ask_requests: true,
                        session_locking: true,
                    },
                    initialize: None,
                    prompt_mode: PromptMode::Full,
                    running_prompts: 0,
                    peak_prompts: 0,
                    asks: Vec::new(),
                    ask_results: Vec::new(),
                    prompt_dropped: None,
                    cancel_at_drop: None,
                    cancel_seen: 0,
                    config_padding: 0,
                    next_session: 0,
                    next_prompt: 0,
                }),
            }),
        }
    }

    /// 读状态。
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 改状态。
    pub fn with<R>(&self, action: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state();
        action(&mut state)
    }

    /// 设置提示脚本。
    pub fn prompt_mode(&self, mode: PromptMode) {
        self.with(|state| state.prompt_mode = mode);
    }

    /// 给某个会话上锁（模拟另一个 core 进程正在跑提示）。
    pub fn lock_session(&self, session_id: &str, pid: u32, frontend: &str) {
        self.with(|state| {
            if let Some(record) = state.sessions.get_mut(session_id) {
                record.info.lock = Some(SessionLockInfo {
                    pid,
                    frontend: frontend.to_owned(),
                    process_name: Some("other-core".to_owned()),
                    acquired_at_ms: 1_700_000_000_000,
                });
            }
        });
    }

    /// 当前会话的锁投影。
    pub fn lock_of(&self, session_id: &str) -> Option<SessionLockInfo> {
        self.state()
            .sessions
            .get(session_id)
            .and_then(|record| record.info.lock.clone())
    }

    /// 调用记录快照。
    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }
}

/// 每个会话固定的 5 条 transcript（最新一条带工具执行记录）。
fn transcript(session_id: &str) -> Vec<TranscriptEntry> {
    (0..5)
        .map(|index| TranscriptEntry {
            id: format!("{session_id}_e{index}"),
            at_ms: 100 + index,
            message: Message {
                role: if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: vec![ContentPart::Text(format!("第 {index} 条"))],
            },
            tool: (index == 4).then(|| ToolExecution {
                tool_call_id: "call_1".to_owned(),
                status: ToolStatus::Completed,
                duration_ms: Some(12),
            }),
            bash: None,
            agent: None,
        })
        .collect()
}

/// 推送更新失败时 core 侧应当看到的错误。
fn emit_error(error: ProtocolError) -> RpcError {
    RpcError::new(
        RpcErrorCode::InternalError.code(),
        format!("推送更新失败：{error}"),
    )
}

impl ScriptedService {
    /// 标准剧本：完整更新序列 + 一次权限询问。
    async fn run_full_prompt(
        &self,
        ctx: &RequestContext,
        session_id: &str,
        prompt_id: &str,
    ) -> Result<PromptResult, RpcError> {
        let updates = ctx.updates();
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            ..Usage::default()
        };
        let tool_call_id = "call_9";

        updates
            .emit(
                session_id,
                SessionUpdate::prompt_started(prompt_id, "deepseek-flash", 1_700_000_000_000),
            )
            .await
            .map_err(emit_error)?;
        for update in [
            AssistantDelta::Text {
                delta: "好的".to_owned(),
            },
            AssistantDelta::Reasoning {
                delta: "先看 README".to_owned(),
            },
        ] {
            updates
                .emit(session_id, SessionUpdate::delta(prompt_id, update))
                .await
                .map_err(emit_error)?;
        }
        updates
            .emit(
                session_id,
                SessionUpdate::ToolStarted {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: tool_call_id.to_owned(),
                    name: "bash".to_owned(),
                    parent_tool_call_id: None,
                    arguments: "{\"cmd\":\"cargo test\"}".to_owned(),
                },
            )
            .await
            .map_err(emit_error)?;
        updates
            .emit(
                session_id,
                SessionUpdate::PermissionRequested {
                    prompt_id: prompt_id.to_owned(),
                    request_id: "perm_1".to_owned(),
                    tool_call_id: tool_call_id.to_owned(),
                },
            )
            .await
            .map_err(emit_error)?;

        let asked = ctx
            .ask_permission(PermissionRequestParams {
                session_id: session_id.to_owned(),
                prompt_id: prompt_id.to_owned(),
                request_id: "perm_1".to_owned(),
                tool_call_id: tool_call_id.to_owned(),
                tool_name: "bash".to_owned(),
                title: "执行命令：cargo test".to_owned(),
                body: None,
                diff: None,
                options: default_options(),
            })
            .await;
        let (option_id, allowed) = match &asked {
            Ok(result) => (
                Some(result.option_id.clone()),
                result.option_id.starts_with("allow"),
            ),
            Err(_) => (None, false),
        };
        self.with(|state| {
            state.asks.push(match &asked {
                Ok(result) => Ok(result.option_id.clone()),
                Err(error) => Err(error.code().unwrap_or(0)),
            });
        });
        updates
            .emit(
                session_id,
                SessionUpdate::PermissionResolved {
                    prompt_id: prompt_id.to_owned(),
                    request_id: "perm_1".to_owned(),
                    option_id,
                },
            )
            .await
            .map_err(emit_error)?;

        let status = if allowed {
            ToolStatus::Completed
        } else {
            ToolStatus::Denied
        };
        updates
            .emit(
                session_id,
                SessionUpdate::ToolFinished {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: tool_call_id.to_owned(),
                    name: "bash".to_owned(),
                    parent_tool_call_id: None,
                    arguments: "{\"cmd\":\"cargo test\"}".to_owned(),
                    status,
                    result: ToolResult {
                        tool_call_id: tool_call_id.to_owned(),
                        content: if allowed {
                            "退出码 0".to_owned()
                        } else {
                            "未获授权".to_owned()
                        },
                        is_error: !allowed,
                        images: Vec::new(),
                    },
                    duration_ms: Some(12),
                    diff: None,
                },
            )
            .await
            .map_err(emit_error)?;
        updates
            .emit(
                session_id,
                SessionUpdate::delta(prompt_id, AssistantDelta::Usage { usage }),
            )
            .await
            .map_err(emit_error)?;

        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Text("好的".to_owned()),
                ContentPart::Reasoning(ReasoningBlock::text("先看 README")),
                ContentPart::ToolCall(ToolCall::new(
                    tool_call_id,
                    "bash",
                    "{\"cmd\":\"cargo test\"}",
                )),
            ],
        };
        let result = PromptResult {
            session_id: session_id.to_owned(),
            prompt_id: prompt_id.to_owned(),
            message: message.clone(),
            finish_reason: FinishReason::ToolCalls,
            usage,
        };
        updates
            .emit(
                session_id,
                SessionUpdate::PromptFinished {
                    prompt_id: prompt_id.to_owned(),
                    message,
                    finish_reason: result.finish_reason.clone(),
                    usage,
                },
            )
            .await
            .map_err(emit_error)?;
        self.with(|state| {
            if let Some(record) = state.sessions.get_mut(session_id) {
                record.info.updated_at_ms = 400;
                record.entries.push(TranscriptEntry {
                    id: format!("{session_id}_prompt"),
                    at_ms: 400,
                    message: result.message.clone(),
                    tool: None,
                    bash: None,
                    agent: None,
                });
            }
        });
        Ok(result)
    }

    /// 挂起提示：记录 drop 与「drop 时取消是否已触发」，然后永远等待。
    async fn hang_prompt(&self, ctx: &RequestContext) -> Result<PromptResult, RpcError> {
        let dropped = Arc::new(AtomicBool::new(false));
        let cancel_at_drop = Arc::new(AtomicBool::new(false));
        self.with(|state| {
            state.prompt_dropped = Some(Arc::clone(&dropped));
            state.cancel_at_drop = Some(Arc::clone(&cancel_at_drop));
        });
        let token = ctx.cancel();
        let _guard = OnDrop(move || {
            dropped.store(true, Ordering::SeqCst);
            cancel_at_drop.store(token.is_cancelled(), Ordering::SeqCst);
        });
        std::future::pending().await
    }
}

impl AgentService for ScriptedService {
    async fn initialize(
        &self,
        _ctx: RequestContext,
        params: InitializeParams,
    ) -> Result<InitializeResult, RpcError> {
        let mut state = self.state();
        let capabilities = state.capabilities;
        state.initialize = Some(params);
        Ok(InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            server: ServerInfo {
                name: "scripted".to_owned(),
                version: "0.0.1".to_owned(),
            },
            capabilities,
        })
    }

    async fn config_get(
        &self,
        _ctx: RequestContext,
        _params: ConfigGetParams,
    ) -> Result<ConfigGetResult, RpcError> {
        let mut state = self.state();
        state.calls.push("config_get".to_owned());
        let mut models = state.models.clone();
        if state.config_padding > 0 {
            models.push(ModelInfo {
                id: "padded".to_owned(),
                display_name: "x".repeat(state.config_padding),
                thinking_levels: None,
                context_window: None,
                max_output_tokens: None,
                pricing: None,
                inputs: Vec::new(),
                remote_compaction: false,
            });
        }
        Ok(ConfigGetResult {
            models,
            default_model: state.default_model.clone(),
            agents: state.agents.clone(),
            default_agent: state.default_agent.clone(),
            cwd: state.cwd.clone(),
        })
    }
    async fn openviking_list(
        &self,
        _ctx: RequestContext,
        params: OpenVikingListParams,
    ) -> Result<OpenVikingListResult, RpcError> {
        Ok(OpenVikingListResult {
            uri: params.uri,
            entries: Vec::new(),
        })
    }

    async fn openviking_read(
        &self,
        _ctx: RequestContext,
        params: OpenVikingReadParams,
    ) -> Result<OpenVikingReadResult, RpcError> {
        Ok(OpenVikingReadResult {
            uri: params.uri,
            content: String::new(),
        })
    }

    async fn openviking_write(
        &self,
        _ctx: RequestContext,
        params: OpenVikingWriteParams,
    ) -> Result<OpenVikingWriteResult, RpcError> {
        Ok(OpenVikingWriteResult {
            uri: params.uri,
            result: Value::Null,
        })
    }

    async fn openviking_delete(
        &self,
        _ctx: RequestContext,
        params: OpenVikingDeleteParams,
    ) -> Result<OpenVikingDeleteResult, RpcError> {
        Ok(OpenVikingDeleteResult {
            uri: params.uri,
            result: Value::Null,
        })
    }

    async fn session_new(
        &self,
        _ctx: RequestContext,
        params: SessionNewParams,
    ) -> Result<SessionInfo, RpcError> {
        let mut state = self.state();
        state.calls.push("session_new".to_owned());
        state.next_session += 1;
        let id = format!("ses_{}", 100 + state.next_session);
        let info = SessionInfo {
            id: id.clone(),
            title: params.title.clone().unwrap_or_else(|| "新会话".to_owned()),
            model: params
                .model
                .clone()
                .or_else(|| state.default_model.clone())
                .unwrap_or_else(|| "deepseek-flash".to_owned()),
            permission: SessionPermission::Full,
            parent_session_id: None,
            forked_from: None,
            cwd: params.cwd.clone().unwrap_or_else(|| state.cwd.clone()),
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
            lock: None,
            mode: params.mode,
        };
        state.sessions.insert(
            id,
            SessionRecord {
                info: info.clone(),
                entries: Vec::new(),
            },
        );
        Ok(info)
    }

    async fn session_list(
        &self,
        _ctx: RequestContext,
        _params: SessionListParams,
    ) -> Result<SessionListResult, RpcError> {
        let mut state = self.state();
        state.calls.push("session_list".to_owned());
        let mut sessions: Vec<SessionInfo> = state
            .sessions
            .values()
            .map(|record| record.info.clone())
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
        Ok(SessionListResult { sessions })
    }

    async fn session_load(
        &self,
        _ctx: RequestContext,
        params: SessionLoadParams,
    ) -> Result<SessionLoadResult, RpcError> {
        let mut state = self.state();
        state
            .calls
            .push(format!("session_load({})", params.session_id));
        let record = state
            .sessions
            .get(&params.session_id)
            .ok_or_else(|| RpcError::of(RpcErrorCode::SessionNotFound))?;
        let mut entries = record.entries.clone();
        let mut truncated = false;
        if let Some(max) = params.max_entries {
            let max = max as usize;
            if max < entries.len() {
                entries.drain(..entries.len() - max);
                truncated = true;
            }
        }
        Ok(SessionLoadResult {
            session: record.info.clone(),
            entries,
            truncated,
            todos: Vec::new(),
            plans: Vec::new(),
            child_calls: Vec::new(),
        })
    }

    async fn session_unload(
        &self,
        _ctx: RequestContext,
        params: SessionUnloadParams,
    ) -> Result<Empty, RpcError> {
        let mut state = self.state();
        state
            .calls
            .push(format!("session_unload({})", params.session_id));
        if !state.sessions.contains_key(&params.session_id) {
            return Err(RpcError::of(RpcErrorCode::SessionNotFound));
        }
        Ok(Empty {})
    }

    async fn session_rename(
        &self,
        ctx: RequestContext,
        params: SessionRenameParams,
    ) -> Result<SessionInfo, RpcError> {
        let info = {
            let mut state = self.state();
            state
                .calls
                .push(format!("session_rename({})", params.session_id));
            let record = state
                .sessions
                .get_mut(&params.session_id)
                .ok_or_else(|| RpcError::of(RpcErrorCode::SessionNotFound))?;
            if record.info.lock.is_some() {
                return Err(RpcError::of(RpcErrorCode::SessionLocked));
            }
            record.info.title = params.title.clone();
            record.info.updated_at_ms = 500;
            record.info.clone()
        };
        ctx.updates()
            .emit(
                info.id.as_str(),
                SessionUpdate::TitleChanged {
                    title: params.title,
                },
            )
            .await
            .map_err(emit_error)?;
        Ok(info)
    }

    async fn session_delete(
        &self,
        _ctx: RequestContext,
        params: SessionDeleteParams,
    ) -> Result<Empty, RpcError> {
        let mut state = self.state();
        state
            .calls
            .push(format!("session_delete({})", params.session_id));
        let record = state
            .sessions
            .get(&params.session_id)
            .ok_or_else(|| RpcError::of(RpcErrorCode::SessionNotFound))?;
        if record.info.lock.is_some() {
            return Err(RpcError::of(RpcErrorCode::SessionLocked));
        }
        state.sessions.remove(&params.session_id);
        Ok(Empty {})
    }

    async fn session_unlock(
        &self,
        ctx: RequestContext,
        params: SessionUnlockParams,
    ) -> Result<Empty, RpcError> {
        let unlocked = {
            let mut state = self.state();
            state.calls.push(format!(
                "session_unlock({}, force={})",
                params.session_id, params.force
            ));
            let record = state
                .sessions
                .get_mut(&params.session_id)
                .ok_or_else(|| RpcError::of(RpcErrorCode::SessionNotFound))?;
            match (&record.info.lock, params.force) {
                (None, _) => false,
                (Some(_), true) => {
                    record.info.lock = None;
                    true
                }
                // 别人的锁 `force = false` 解不掉：这是强夺才有的语义
                (Some(_), false) => return Err(RpcError::of(RpcErrorCode::SessionLocked)),
            }
        };
        if unlocked {
            ctx.updates()
                .emit(
                    params.session_id.as_str(),
                    SessionUpdate::LockChanged { lock: None },
                )
                .await
                .map_err(emit_error)?;
        }
        Ok(Empty {})
    }

    async fn session_set_mode(
        &self,
        _ctx: RequestContext,
        _params: SessionSetModeParams,
    ) -> Result<SessionInfo, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }

    async fn session_set_permission(
        &self,
        _ctx: RequestContext,
        _params: SessionSetPermissionParams,
    ) -> Result<SessionInfo, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }

    async fn session_set_model(
        &self,
        _ctx: RequestContext,
        _params: SessionSetModelParams,
    ) -> Result<moc_protocol::methods::SessionSetModelResult, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }

    async fn session_plan_approve(
        &self,
        _ctx: RequestContext,
        _params: SessionPlanApproveParams,
    ) -> Result<SessionPlanApproveResult, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }

    async fn session_compact(
        &self,
        _ctx: RequestContext,
        _params: SessionCompactParams,
    ) -> Result<moc_protocol::methods::CompactResult, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }
    async fn session_context(
        &self,
        _ctx: RequestContext,
        _params: SessionContextParams,
    ) -> Result<moc_protocol::methods::ContextUsageResult, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }
    async fn session_context_content(
        &self,
        _ctx: RequestContext,
        _params: SessionContextContentParams,
    ) -> Result<ContextContentResult, RpcError> {
        Err(RpcError::of(RpcErrorCode::NotSupported))
    }
    async fn session_prompt(
        &self,
        ctx: RequestContext,
        params: PromptParams,
    ) -> Result<PromptResult, RpcError> {
        let (mode, prompt_id) = {
            let mut state = self.state();
            state
                .calls
                .push(format!("session_prompt({})", params.session_id));
            let record = state
                .sessions
                .get(&params.session_id)
                .ok_or_else(|| RpcError::of(RpcErrorCode::SessionNotFound))?;
            if record.info.lock.is_some() {
                return Err(RpcError::of(RpcErrorCode::SessionLocked));
            }
            state.running_prompts += 1;
            state.peak_prompts = state.peak_prompts.max(state.running_prompts);
            state.next_prompt += 1;
            (
                state.prompt_mode.clone(),
                format!("prompt_{}", state.next_prompt),
            )
        };
        let session_id = params.session_id.as_str();
        let outcome = match mode {
            PromptMode::Full => self.run_full_prompt(&ctx, session_id, &prompt_id).await,
            PromptMode::Gated(notify) => {
                notify.notified().await;
                self.run_full_prompt(&ctx, session_id, &prompt_id).await
            }
            PromptMode::Ask => {
                let request = AskParams {
                    session_id: session_id.to_owned(),
                    prompt_id: prompt_id.clone(),
                    request_id: "ask_1".to_owned(),
                    questions: vec![AskQuestion {
                        id: "choice".to_owned(),
                        question: "选择语言".to_owned(),
                        header: Some("语言".to_owned()),
                        multi: false,
                        recommended: vec![0],
                        options: vec![AskOption {
                            label: "Rust".to_owned(),
                            description: Some("系统语言".to_owned()),
                            preview: None,
                        }],
                    }],
                };
                match ctx.requester().request::<SessionRequestAsk>(request).await {
                    Ok(answer) => {
                        self.with(|state| state.ask_results.push(answer));
                        self.run_full_prompt(&ctx, session_id, &prompt_id).await
                    }
                    Err(_) => Err(RpcError::of(RpcErrorCode::InternalError)),
                }
            }
            PromptMode::UntilCancelled => {
                ctx.cancel().cancelled().await;
                self.with(|state| state.cancel_seen += 1);
                Ok(PromptResult {
                    session_id: params.session_id.clone(),
                    prompt_id,
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentPart::Text("已取消".to_owned())],
                    },
                    finish_reason: FinishReason::Cancelled,
                    usage: Usage::default(),
                })
            }
            PromptMode::Hang => self.hang_prompt(&ctx).await,
        };
        self.with(|state| state.running_prompts -= 1);
        outcome
    }
}

// ───────────────────────────── 前端侧辅助 ─────────────────────────────

/// 测试用的前端信息。
pub fn client_info() -> ClientInfo {
    ClientInfo {
        name: "moc-test".to_owned(),
        version: "0.0.1".to_owned(),
        frontend: "tui".to_owned(),
    }
}

/// 收集 `session/update` 通知。
#[derive(Clone, Default)]
pub struct UpdateLog {
    inner: Arc<Mutex<Vec<SessionUpdate>>>,
}

impl UpdateLog {
    /// 推入一条更新。
    pub fn push(&self, update: SessionUpdate) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(update);
    }

    /// 取走并清空。
    pub fn take(&self) -> Vec<SessionUpdate> {
        std::mem::take(&mut *self.inner.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// 全部更新的 `type` 标签。
    pub fn tags(&self) -> Vec<&'static str> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(update_tag)
            .collect()
    }

    /// 第一条满足类型的更新。
    pub fn find(&self, tag: &str) -> Option<SessionUpdate> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|update| update_tag(update) == tag)
            .cloned()
    }
}

/// 更新的 `type` 标签。
pub fn update_tag(update: &SessionUpdate) -> &'static str {
    match update {
        SessionUpdate::AgentStarted { .. } => "agent_started",
        SessionUpdate::AgentStatusChanged { .. } => "agent_status_changed",
        SessionUpdate::AgentMessage { .. } => "agent_message",
        SessionUpdate::ToolProgress { .. } => "tool_progress",
        SessionUpdate::ToolBatchStarted { .. } => "tool_batch_started",
        SessionUpdate::PromptStarted { .. } => "prompt_started",
        SessionUpdate::GenerationStarted { .. } => "generation_started",
        SessionUpdate::GuardNotice { .. } => "guard_notice",
        SessionUpdate::AssistantDelta { .. } => "assistant_delta",
        SessionUpdate::BashStarted { .. } => "bash_started",
        SessionUpdate::ToolStarted { .. } => "tool_started",
        SessionUpdate::ToolFinished { .. } => "tool_finished",
        SessionUpdate::PermissionRequested { .. } => "permission_requested",
        SessionUpdate::PermissionResolved { .. } => "permission_resolved",
        SessionUpdate::PromptFinished { .. } => "prompt_finished",
        SessionUpdate::TitleChanged { .. } => "title_changed",
        SessionUpdate::LockChanged { .. } => "lock_changed",
        SessionUpdate::ModeChanged { .. } => "mode_changed",
        SessionUpdate::PermissionChanged { .. } => "permission_changed",
        SessionUpdate::ModelChanged { .. } => "model_changed",
        SessionUpdate::TodoChanged { .. } => "todo_changed",
        SessionUpdate::TodoReminder { .. } => "todo_reminder",
        SessionUpdate::PlanSubmitted { .. } => "plan_submitted",
        SessionUpdate::ContextCompacted { .. } => "context_compacted",
        SessionUpdate::JobFinished { .. } => "job_finished",
        SessionUpdate::WorkspaceChanged { .. } => "workspace_changed",
        SessionUpdate::UpstreamMismatch { .. } => "upstream_mismatch",
        SessionUpdate::SteeringApplied { .. } => "steering_applied",
        SessionUpdate::Error { .. } => "error",
    }
}

/// 通知处理器：把更新记进 [`UpdateLog`]。
pub fn update_handler(
    log: UpdateLog,
) -> impl Fn(SessionUpdateParams, ClientRequestContext) -> Ready<()> + Send + Sync + 'static {
    move |params, _ctx| {
        log.push(params.update);
        std::future::ready(())
    }
}

/// 反向请求处理器：固定回一个选项 ID。
pub fn permission_handler(
    option_id: &'static str,
) -> impl Fn(PermissionRequestParams, ClientRequestContext) -> Ready<Result<PermissionResult, RpcError>>
+ Send
+ Sync
+ 'static {
    move |_params, _ctx| {
        std::future::ready(Ok(PermissionResult {
            option_id: option_id.to_owned(),
        }))
    }
}

/// 轮询等待条件成立；超时 panic。
pub async fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "等待条件超时");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 起一个服务端 + 原始对端。
pub fn raw_pair(
    service: ScriptedService,
    options: ServerOptions,
) -> (Arc<Server<ScriptedService>>, ServerConnection, RawPeer) {
    let (client_side, server_side) = transport::duplex(64 * 1024);
    let server = Server::with_options(service, options);
    let connection = server.spawn(server_side);
    let peer = RawPeer::new(client_side, options.framing, options.max_frame_bytes);
    (server, connection, peer)
}

/// 用 crate 客户端接上服务端（TUI 内嵌 core 的同款接法）。
///
/// 返回服务端、服务端连接句柄与客户端句柄；服务端连接句柄用来观察「句柄丢弃是否
/// 真的把连接收掉了」。
pub async fn local_client(
    service: ScriptedService,
    builder: ClientBuilder,
) -> (
    Arc<Server<ScriptedService>>,
    ServerConnection,
    ClientHandle,
    InitializeResult,
) {
    let (client_side, server_side) = transport::duplex(64 * 1024);
    let server = Server::new(service);
    let connection = server.spawn(server_side);
    let (handle, handshake) = builder.connect(client_side).await.expect("握手成功");
    (server, connection, handle, handshake)
}

/// 一次 `session/prompt` 的参数。
pub fn prompt_params(session_id: &str, text: &str) -> PromptParams {
    PromptParams {
        session_id: session_id.to_owned(),
        input: vec![InputPart::Text {
            text: text.to_owned(),
        }],
        mode: None,
        agent_message: None,
    }
}

/// 从通知里取出更新。
pub fn update_of(notification: &Notification) -> SessionUpdate {
    assert_eq!(notification.method, "session/update");
    let params: SessionUpdateParams =
        serde_json::from_value(notification.params.clone().expect("带参数")).expect("参数合法");
    params.update
}

/// 读到指定响应，并收集期间收到的全部更新。
///
/// 遇到反向请求直接 panic：需要应答的测试用 [`recv_with_auto_permission`]。
pub async fn recv_response_with_updates(
    peer: &mut RawPeer,
    id: i64,
) -> (Response, Vec<SessionUpdate>) {
    let mut updates = Vec::new();
    loop {
        match peer.recv().await {
            RpcMessage::Response(response) if response.id == Some(Id::Number(id)) => {
                return (response, updates);
            }
            RpcMessage::Response(other) => panic!("收到意外响应：{other:?}"),
            RpcMessage::Notification(notification) => updates.push(update_of(&notification)),
            RpcMessage::Request(request) => panic!("收到意外反向请求：{request:?}"),
        }
    }
}

/// 读到指定响应，期间**自动应答权限询问**（回 `option_id`），并收集全部更新。
///
/// 这是「有交互能力的前端」的行为：不响应反向请求会让 core 一直等下去。
pub async fn recv_with_auto_permission(
    peer: &mut RawPeer,
    id: i64,
    option_id: &str,
) -> (Response, Vec<SessionUpdate>) {
    let mut updates = Vec::new();
    loop {
        match peer.recv().await {
            RpcMessage::Response(response) if response.id == Some(Id::Number(id)) => {
                return (response, updates);
            }
            RpcMessage::Response(other) => panic!("收到意外响应：{other:?}"),
            RpcMessage::Notification(notification) => updates.push(update_of(&notification)),
            RpcMessage::Request(request) => {
                assert_eq!(request.method, "session/request_permission");
                peer.send(&RpcMessage::Response(Response::result(
                    request.id,
                    serde_json::json!({ "option_id": option_id }),
                )))
                .await;
            }
        }
    }
}

// ───────────────────────────── 原始对端 ─────────────────────────────

/// 原始对端：直接读写原始帧。
pub struct RawPeer {
    stream: tokio::io::DuplexStream,
    encoder: FrameEncoder,
    decoder: FrameDecoder,
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl RawPeer {
    /// 在给定传输上构造对端。
    pub fn new(stream: tokio::io::DuplexStream, framing: Framing, max_frame_bytes: usize) -> Self {
        Self {
            stream,
            encoder: FrameEncoder::new(framing),
            decoder: FrameDecoder::new(framing, max_frame_bytes),
            buffer: vec![0u8; 8 * 1024],
            max_frame_bytes,
        }
    }

    /// 写一条报文。
    pub async fn send(&mut self, message: &RpcMessage) {
        let body = message.to_vec().expect("报文可序列化");
        self.send_payload(&body).await;
    }

    /// 写一段报文体（按当前分帧组帧）。
    pub async fn send_payload(&mut self, payload: &[u8]) {
        let mut frame = Vec::new();
        assert!(
            self.encoder
                .encode(payload, self.max_frame_bytes, &mut frame),
            "测试载荷不超过上限"
        );
        self.send_bytes(&frame).await;
    }

    /// 原样写字节（用于构造分帧错位）。
    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("写成功");
        self.stream.flush().await.expect("flush 成功");
    }

    /// 读下一个原始报文体。
    pub async fn recv_payload(&mut self) -> Vec<u8> {
        loop {
            if let Some(frame) = self.decoder.next_frame().expect("分帧合法") {
                return frame.to_vec();
            }
            let read = tokio::time::timeout(READ_TIMEOUT, self.stream.read(&mut self.buffer))
                .await
                .expect("读超时：协议层没有回应该回的东西")
                .expect("读成功");
            assert!(read > 0, "对端在报文完成前关闭了连接");
            self.decoder.push(&self.buffer[..read]);
        }
    }

    /// 读下一条报文。
    pub async fn recv(&mut self) -> RpcMessage {
        let payload = self.recv_payload().await;
        RpcMessage::parse(&payload).expect("对端发出的是合法报文")
    }

    /// 读原始报文体并解析为 JSON（断言线形状用）。
    pub async fn recv_json(&mut self) -> Value {
        let payload = self.recv_payload().await;
        serde_json::from_slice(&payload).expect("报文体是合法 JSON")
    }

    /// 读到满足条件的报文；不满足的丢弃。
    pub async fn recv_until<F: FnMut(&RpcMessage) -> bool>(
        &mut self,
        mut predicate: F,
    ) -> RpcMessage {
        loop {
            let message = self.recv().await;
            if predicate(&message) {
                return message;
            }
        }
    }

    /// 读指定 ID 的响应（跳过通知与其它响应）。
    pub async fn recv_response(&mut self, id: i64) -> Response {
        match self
            .recv_until(|message| matches!(message, RpcMessage::Response(response) if response.id == Some(Id::Number(id))))
            .await
        {
            RpcMessage::Response(response) => response,
            other => panic!("期待响应，收到 {other:?}"),
        }
    }

    /// 读指定 ID 的响应并断言它成功了，返回结果。
    pub async fn recv_ok(&mut self, id: i64) -> Value {
        self.recv_response(id).await.payload.expect("请求成功")
    }

    /// 读指定 ID 的响应并断言它失败了，返回错误码。
    pub async fn recv_err(&mut self, id: i64) -> i32 {
        match self.recv_response(id).await.payload {
            Ok(value) => panic!("期待失败，收到结果 {value}"),
            Err(error) => error.code,
        }
    }

    /// 窗口内必须没有任何字节（通知永不被应答）。
    pub async fn expect_silence(&mut self, window: Duration) -> bool {
        match tokio::time::timeout(window, self.stream.read(&mut self.buffer)).await {
            Err(_) => true,
            Ok(Ok(0)) => false,
            Ok(Ok(read)) => {
                self.decoder.push(&self.buffer[..read]);
                false
            }
            Ok(Err(_)) => false,
        }
    }

    /// 等到对端关闭写半边。
    pub async fn expect_eof(&mut self, window: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.stream.read(&mut self.buffer)).await {
                Ok(Ok(0)) => return true,
                Ok(Ok(read)) => self.decoder.push(&self.buffer[..read]),
                Ok(Err(_)) => return true,
                Err(_) => return false,
            }
        }
    }
}

/// 构造请求报文。
pub fn request(id: i64, method: &'static str, params: Value) -> RpcMessage {
    RpcMessage::Request(Request::new(Id::number(id), method, Some(params)))
}

/// 构造通知报文。
pub fn notification(method: &'static str, params: Value) -> RpcMessage {
    RpcMessage::Notification(Notification::new(method, Some(params)))
}

/// 一组合法的 `initialize` 参数。
pub fn initialize_params(protocol_version: &str) -> Value {
    serde_json::json!({
        "protocol_version": protocol_version,
        "client": { "name": "raw-peer", "version": "0.0.1", "frontend": "tui" },
        "capabilities": { "permission_requests": true }
    })
}

/// 一份合法的 `initialize` 结果（扮演 core 侧时用）。
pub fn initialize_result() -> Value {
    serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "server": { "name": "raw-core", "version": "9.9.9" },
        "capabilities": { "permission_requests": true, "session_locking": true, "ask_requests": false }
    })
}

/// 完成一次握手的便捷流程。
pub async fn handshake(peer: &mut RawPeer) {
    peer.send(&request(
        1,
        "initialize",
        initialize_params(PROTOCOL_VERSION),
    ))
    .await;
    peer.recv_ok(1).await;
}

/// 默认服务端参数（自定义分帧）。
pub fn options_with_framing(framing: Framing) -> ServerOptions {
    ServerOptions {
        framing,
        ..ServerOptions::default()
    }
}
