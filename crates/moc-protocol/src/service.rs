//! 服务端（core）契约：`AgentService` 与请求上下文。
//!
//! core 实现 [`AgentService`]，协议层负责报文、分帧、并发门禁、取消传播与方法分发。
//! 职责边界只有一条：**协议层不持有任何会话状态**——状态机、上下文、持久化、
//! 锁全在 core；协议层只搬运参数与结果，并把三条取消来源合并到同一个令牌。
//!
//! # 取消树
//!
//! ```text
//! Server::shutdown() ──► 连接令牌 ──► 每请求令牌（提示请求额外登记进提示注册表）
//! ```
//!
//! 于是「前端调 `session/cancel`」「连接断开」「server 关闭」三条路径都通过
//! [`RequestContext::cancel`] 传到 core，core 只需在长任务里 `select!` 这个令牌。
//!
//! # 实现要求
//!
//! 服务方法的 future **必须可被 drop**（取消安全）：连接关闭时协议层会 abort 在飞
//! 任务，future 被直接丢弃。因此不要在 `await` 之间留下「半个会话状态」——
//! 需要跨 `await` 的一致状态要么放进 core 自己的状态机，要么用 Drop guard 收拾。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::connection::{Outbound, Traffic};
use crate::error::{ProtocolError, RpcCallError, RpcError};
use crate::message::Notification;
use crate::method::Method;
use crate::methods::{
    AgentListParams, AgentListResult, AgentMessageParams, AgentMessageResult, AgentSpawnParams,
    AgentSpawnResult, AgentStopParams, AgentStopResult, AgentWaitParams, AgentWaitResult,
    BtwParams, BtwResult, BtwUpdate, BtwUpdateMethod, BtwUpdateParams, ChartAnalyzeParams,
    ChartAnalyzeResult, CompactResult, ConfigGetParams, ConfigGetResult, ContextContentResult,
    ContextUsageResult, Empty, FsCreateDirParams, FsCreateDirResult, FsListDirParams,
    FsListDirResult, InitializeParams, InitializeResult, PermissionRequestParams, PermissionResult,
    PluginListParams, PluginListResult, PluginReloadParams, PluginReloadResult,
    PluginRunCommandParams, PluginRunCommandResult, PromptParams, PromptResult,
    ResourceCompleteParams, ResourceCompleteResult, SessionCompactParams,
    SessionContextContentParams, SessionContextParams, SessionDeleteParams, SessionInfo,
    SessionListParams, SessionListResult, SessionLoadPageParams, SessionLoadPageResult,
    SessionLoadParams, SessionLoadResult, SessionNewParams, SessionPlanApproveParams,
    SessionPlanApproveResult, SessionPlanDiffParams, SessionPlanDiffResult, SessionPlanModeParams,
    SessionPlanReviseParams, SessionPlanReviseResult, SessionRedoParams, SessionRenameParams,
    SessionRetryParams, SessionRewindParams, SessionSetModeParams, SessionSetModelParams,
    SessionSetModelResult, SessionSetPermissionParams, SessionUnloadParams, SessionUnlockParams,
    SessionUpdateMethod, SessionUpdateParams, SessionUsageParams, SessionUsageResult,
    UnsteerParams, UnsteerResult, WorkspaceGitBranchParams, WorkspaceGitBranchResult,
};

/// core 实现的接口：每方法一个 async 方法。
///
/// 用 Rust 2024 的 RPITIT（返回位置 impl Trait）而不是 `async-trait`：两者都要
/// `+ Send` 约束，但 RPITIT 不引入动态派发、不要求额外的 crate 依赖。
pub trait AgentService: Send + Sync + 'static {
    /// 握手：协商版本并登记前端能力。返回的 `protocol_version` 应当回当前版本。
    fn initialize(
        &self,
        ctx: RequestContext,
        params: InitializeParams,
    ) -> impl Future<Output = Result<InitializeResult, RpcError>> + Send;

    /// 只读配置：模型 / agent 候选集与默认值。
    fn config_get(
        &self,
        ctx: RequestContext,
        params: ConfigGetParams,
    ) -> impl Future<Output = Result<ConfigGetResult, RpcError>> + Send;

    /// 可选管理能力；不支持本地配置的嵌入式服务须明确拒绝，而非忽略写入。
    fn config_manage(
        &self,
        _ctx: RequestContext,
        _params: crate::ConfigAction,
    ) -> impl Future<Output = Result<crate::ConfigReply, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn session_steer(
        &self,
        _ctx: RequestContext,
        _params: crate::SteerParams,
    ) -> impl Future<Output = Result<crate::SteerResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 撤回一条尚未写入会话的 steering 消息；不支持时明确拒绝。
    fn session_unsteer(
        &self,
        _ctx: RequestContext,
        _params: UnsteerParams,
    ) -> impl Future<Output = Result<UnsteerResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn agent_spawn(
        &self,
        _ctx: RequestContext,
        _params: AgentSpawnParams,
    ) -> impl Future<Output = Result<AgentSpawnResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn agent_list(
        &self,
        _ctx: RequestContext,
        _params: AgentListParams,
    ) -> impl Future<Output = Result<AgentListResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn agent_message(
        &self,
        _ctx: RequestContext,
        _params: AgentMessageParams,
    ) -> impl Future<Output = Result<AgentMessageResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn agent_stop(
        &self,
        _ctx: RequestContext,
        _params: AgentStopParams,
    ) -> impl Future<Output = Result<AgentStopResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn agent_wait(
        &self,
        _ctx: RequestContext,
        _params: AgentWaitParams,
    ) -> impl Future<Output = Result<AgentWaitResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn plugin_list(
        &self,
        ctx: RequestContext,
        params: PluginListParams,
    ) -> impl Future<Output = Result<PluginListResult, RpcError>> + Send;

    fn plugin_run_command(
        &self,
        ctx: RequestContext,
        params: PluginRunCommandParams,
    ) -> impl Future<Output = Result<PluginRunCommandResult, RpcError>> + Send;

    /// 重新发现并加载插件；插件在本进程内被停用后靠它恢复。
    fn plugin_reload(
        &self,
        _ctx: RequestContext,
        _params: PluginReloadParams,
    ) -> impl Future<Output = Result<PluginReloadResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    fn resource_complete(
        &self,
        _ctx: RequestContext,
        _params: ResourceCompleteParams,
    ) -> impl Future<Output = Result<ResourceCompleteResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 列出目录下的子目录，供前端选择工作目录。
    fn fs_list_dir(
        &self,
        _ctx: RequestContext,
        _params: FsListDirParams,
    ) -> impl Future<Output = Result<FsListDirResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 在指定目录下新建子目录，供前端的目录选择器建工作区。
    fn fs_create_dir(
        &self,
        _ctx: RequestContext,
        _params: FsCreateDirParams,
    ) -> impl Future<Output = Result<FsCreateDirResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 根据已有 Core 图表语义生成浏览器可消费的数据描述。
    fn chart_analyze(
        &self,
        _ctx: RequestContext,
        _params: ChartAnalyzeParams,
    ) -> impl Future<Output = Result<ChartAnalyzeResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 读取工作目录当前 Git 分支；不是仓库时返回空分支。
    fn workspace_git_branch(
        &self,
        _ctx: RequestContext,
        _params: WorkspaceGitBranchParams,
    ) -> impl Future<Output = Result<WorkspaceGitBranchResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 新建会话。
    fn session_new(
        &self,
        ctx: RequestContext,
        params: SessionNewParams,
    ) -> impl Future<Output = Result<SessionInfo, RpcError>> + Send;

    /// 列出全部会话（按 `updated_at_ms` 降序）。
    fn session_list(
        &self,
        ctx: RequestContext,
        params: SessionListParams,
    ) -> impl Future<Output = Result<SessionListResult, RpcError>> + Send;

    /// 读取一页 transcript；分页结果必须保持单帧有界。
    fn session_load_page(
        &self,
        _ctx: RequestContext,
        _params: SessionLoadPageParams,
    ) -> impl Future<Output = Result<SessionLoadPageResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }
    /// 读取 transcript；`max_entries` 截断必须由 core 真正执行（`truncated` 是契约）。
    fn session_load(
        &self,
        ctx: RequestContext,
        params: SessionLoadParams,
    ) -> impl Future<Output = Result<SessionLoadResult, RpcError>> + Send;
    /// 回退到指定 transcript 消息；用户消息本身不保留。
    fn session_rewind(
        &self,
        _ctx: RequestContext,
        _params: SessionRewindParams,
    ) -> impl Future<Output = Result<SessionLoadResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 向前滚：把最近一次回退截掉的尾部接回到指定条目。
    fn session_redo(
        &self,
        _ctx: RequestContext,
        _params: SessionRedoParams,
    ) -> impl Future<Output = Result<SessionLoadResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 释放会话常驻资源：不删除会话、不影响列表。
    fn session_unload(
        &self,
        ctx: RequestContext,
        params: SessionUnloadParams,
    ) -> impl Future<Output = Result<Empty, RpcError>> + Send;

    /// 改标题；被他人持锁时回 [`crate::RpcErrorCode::SessionLocked`]。
    fn session_rename(
        &self,
        ctx: RequestContext,
        params: SessionRenameParams,
    ) -> impl Future<Output = Result<SessionInfo, RpcError>> + Send;

    /// 删除会话；被他人持锁时回 [`crate::RpcErrorCode::SessionLocked`]。
    fn session_delete(
        &self,
        ctx: RequestContext,
        params: SessionDeleteParams,
    ) -> impl Future<Output = Result<Empty, RpcError>> + Send;

    /// 强夺 / 释放锁；`force = false` 只释放本进程持有的锁。
    fn session_unlock(
        &self,
        ctx: RequestContext,
        params: SessionUnlockParams,
    ) -> impl Future<Output = Result<Empty, RpcError>> + Send;

    /// 切换会话模式方言；只在空白会话期允许，已有提示历史时回 `InvalidParams`；
    /// 被他人持锁时回 `SessionLocked`。
    fn session_set_mode(
        &self,
        ctx: RequestContext,
        params: SessionSetModeParams,
    ) -> impl Future<Output = Result<SessionInfo, RpcError>> + Send;

    /// 进入 / 退出 plan 叠加态；被他人持锁时回 `SessionLocked`。
    fn session_plan_mode(
        &self,
        ctx: RequestContext,
        params: SessionPlanModeParams,
    ) -> impl Future<Output = Result<SessionInfo, RpcError>> + Send;

    /// 切换会话命令执行权限；被他人持锁时回 `SessionLocked`。
    fn session_set_permission(
        &self,
        ctx: RequestContext,
        params: SessionSetPermissionParams,
    ) -> impl Future<Output = Result<SessionInfo, RpcError>> + Send;
    /// 切换会话模型（按选择器）；被他人持锁时回 `SessionLocked`。
    ///
    /// 目标模型容不下当前上下文时按 [`SessionSetModelParams::overflow`] 处理。
    fn session_set_model(
        &self,
        ctx: RequestContext,
        params: SessionSetModelParams,
    ) -> impl Future<Output = Result<SessionSetModelResult, RpcError>> + Send;

    /// 批准或拒绝已提交计划。
    fn session_plan_approve(
        &self,
        ctx: RequestContext,
        params: SessionPlanApproveParams,
    ) -> impl Future<Output = Result<SessionPlanApproveResult, RpcError>> + Send;

    /// 提交计划批注并生成修订提示。
    fn session_plan_revise(
        &self,
        ctx: RequestContext,
        params: SessionPlanReviseParams,
    ) -> impl Future<Output = Result<SessionPlanReviseResult, RpcError>> + Send;

    /// 计划版本与上一版的差异。
    fn session_plan_diff(
        &self,
        ctx: RequestContext,
        params: SessionPlanDiffParams,
    ) -> impl Future<Output = Result<SessionPlanDiffResult, RpcError>> + Send;

    /// 手动压缩会话上下文。
    fn session_compact(
        &self,
        ctx: RequestContext,
        params: SessionCompactParams,
    ) -> impl Future<Output = Result<CompactResult, RpcError>> + Send;

    /// 估算当前会话上下文，并返回系统提示词、工具提示词与历史明细。
    fn session_context(
        &self,
        ctx: RequestContext,
        params: SessionContextParams,
    ) -> impl Future<Output = Result<ContextUsageResult, RpcError>> + Send;
    /// 读取当前上下文里的真实正文（系统提示片段、工具 schema、注入与转写消息）。
    fn session_context_content(
        &self,
        ctx: RequestContext,
        params: SessionContextContentParams,
    ) -> impl Future<Output = Result<ContextContentResult, RpcError>> + Send;
    /// 返回当前会话各模型的累计调用、token、缓存、思考与激活时间。
    fn session_usage(
        &self,
        _ctx: RequestContext,
        _params: SessionUsageParams,
    ) -> impl Future<Output = Result<SessionUsageResult, RpcError>> + Send {
        async { Err(crate::RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 手动执行命令，不调用模型；与提示共享会话繁忙和取消边界。
    fn session_bash(
        &self,
        _ctx: RequestContext,
        _params: crate::BashParams,
    ) -> impl Future<Output = Result<crate::BashResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 跑一次提示 agent 工作：挂到本次提示结束才返回，过程增量走 `session/update` 通知。
    ///
    /// 提示结束时必须自行 emit `prompt_finished`，且其 `message` / `finish_reason` /
    /// `usage` 与返回值一致（前端据此把流式内容替换为权威版本）。
    fn session_prompt(
        &self,
        ctx: RequestContext,
        params: PromptParams,
    ) -> impl Future<Output = Result<PromptResult, RpcError>> + Send;

    /// 不追加任何用户消息，按会话现有上下文重跑一次提示。
    ///
    /// 与 [`Self::session_prompt`] 共用提示锁、取消令牌与 `prompt_started` /
    /// `prompt_finished` 更新；语义区别只在输入：不落库用户消息，直接把当前
    /// transcript 原样再请求一次模型。会话没有可重试的上下文时回
    /// [`crate::RpcErrorCode::InvalidParams`]。
    fn session_retry(
        &self,
        _ctx: RequestContext,
        _params: SessionRetryParams,
    ) -> impl Future<Output = Result<PromptResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }

    /// 在只读快照上跑一次旁路问答：不写会话、不抢主提示锁、不执行工具。
    ///
    /// 过程增量走 `session/btw_update`，正文以返回值为权威版本。实现必须尊重
    /// `ctx.cancel()`（定向取消与连接关闭共用它），被取消时返回部分结果。
    fn session_btw(
        &self,
        _ctx: RequestContext,
        _params: BtwParams,
    ) -> impl Future<Output = Result<BtwResult, RpcError>> + Send {
        async { Err(RpcError::of(crate::RpcErrorCode::NotSupported)) }
    }
}

/// 一次请求的上下文，按值传入服务方法。
///
/// 全部字段都是廉价克隆的句柄，因此可以直接搬进 core 自己的任务里。
#[derive(Clone)]
pub struct RequestContext {
    connection_id: u64,
    client: crate::methods::ClientInfo,
    updates: UpdateSink,
    requester: Requester,
    cancel: CancellationToken,
}

impl RequestContext {
    /// 协议层内部构造。
    pub(crate) fn new(
        connection_id: u64,
        client: crate::methods::ClientInfo,
        traffic: Arc<Traffic>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            connection_id,
            client,
            updates: UpdateSink {
                traffic: Arc::clone(&traffic),
            },
            requester: Requester {
                traffic,
                cancel: cancel.clone(),
            },
            cancel,
        }
    }

    /// 复用连接与前端信息，但替换当前操作取消令牌；core 内部用于脱离父请求的子代理。
    pub fn with_cancel(&self, cancel: CancellationToken) -> Self {
        Self {
            connection_id: self.connection_id,
            client: self.client.clone(),
            updates: self.updates.clone(),
            requester: Requester {
                traffic: Arc::clone(&self.requester.traffic),
                cancel: cancel.clone(),
            },
            cancel,
        }
    }

    /// 连接号；用于日志关联（前端侧恒为 `0`）。
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// 发起本次请求的前端信息。
    pub fn client(&self) -> &crate::methods::ClientInfo {
        &self.client
    }

    /// 更新通知出口。
    pub fn updates(&self) -> UpdateSink {
        self.updates.clone()
    }

    /// 反向请求出口。
    pub fn requester(&self) -> Requester {
        self.requester.clone()
    }

    /// 取消令牌：任一条取消路径触发（前端 `session/cancel`、连接断开、server 关闭）。
    pub fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 向发起该请求的前端要一次授权。
    ///
    /// 等价于 `requester().request::<SessionRequestPermission>(params)`；独立成方法
    /// 是因为这是 core 最常用的反向请求。前端未注册处理器、连接断开、提示被取消时
    /// 都以 `Err` 返回，core 应当按「未授权」处理。
    pub async fn ask_permission(
        &self,
        params: PermissionRequestParams,
    ) -> Result<PermissionResult, RpcCallError> {
        self.requester
            .request::<crate::methods::SessionRequestPermission>(params)
            .await
    }
}

/// 更新通知出口。
///
/// 出站队列满时 `emit` 会 `await`：背压直接传回 core 的 LLM 流读取，天然限速，
/// 不会在协议层堆出无界缓冲。连接已死时返回 [`ProtocolError::ConnectionClosed`]，
/// core 据此中止本次提示。
#[derive(Clone)]
pub struct UpdateSink {
    traffic: Arc<Traffic>,
}

impl UpdateSink {
    /// 推送一条 `session/update` 通知。
    pub async fn emit(
        &self,
        session_id: impl Into<String>,
        update: crate::methods::SessionUpdate,
    ) -> Result<(), ProtocolError> {
        let params = SessionUpdateParams {
            session_id: session_id.into(),
            update,
        };
        let value = serde_json::to_value(params)
            .map_err(|error| ProtocolError::Encode(error.to_string()))?;
        self.traffic
            .send(Outbound::Notification(Notification::new(
                <SessionUpdateMethod as Method>::NAME,
                Some(value),
            )))
            .await
    }

    /// 推送一条 `session/btw_update` 通知。
    ///
    /// 与 [`UpdateSink::emit`] 分开方法名，避免旁路增量误进主转写 reducer；
    /// 背压与连接关闭语义完全相同。
    pub async fn emit_btw(
        &self,
        session_id: impl Into<String>,
        request_id: u64,
        update: BtwUpdate,
    ) -> Result<(), ProtocolError> {
        let params = BtwUpdateParams {
            session_id: session_id.into(),
            request_id,
            update,
        };
        let value = serde_json::to_value(params)
            .map_err(|error| ProtocolError::Encode(error.to_string()))?;
        self.traffic
            .send(Outbound::Notification(Notification::new(
                <BtwUpdateMethod as Method>::NAME,
                Some(value),
            )))
            .await
    }
}

/// 反向请求出口。
#[derive(Clone)]
pub struct Requester {
    traffic: Arc<Traffic>,
    cancel: CancellationToken,
}

impl Requester {
    /// 发起一次反向请求。
    ///
    /// - 客户端回错（如未注册处理器时的 `-32601`）→ [`RpcCallError::Rpc`]。
    /// - 提示被取消、连接关闭、前端始终不回 → [`RpcCallError::Cancelled`]。
    pub async fn request<M: crate::method::ServerRequest>(
        &self,
        params: M::Params,
    ) -> Result<M::Result, RpcCallError> {
        self.traffic
            .reverse_request::<M>(params, &self.cancel)
            .await
    }
}
