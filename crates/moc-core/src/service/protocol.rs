//! JSON-RPC 协议适配器：保持单一 trait 实现，业务操作委派给各领域模块。

use super::errors::{invalid_params, job_error, to_rpc, write_lease_error};
use super::{
    Core, bash, btw, config_api, context, history, management, mode, plugin_api, prompt,
    resource_api, session_api, session_config, usage, workflow_api,
};
use crate::compact;
use moc_protocol::methods::{
    AgentListParams, AgentListResult, AgentMessageParams, AgentMessageResult, AgentSpawnParams,
    AgentSpawnResult, AgentStopParams, AgentStopResult, AgentWaitParams, AgentWaitResult,
    ChartAnalyzeParams, ChartAnalyzeResult, CompactResult, ConfigGetParams, ConfigGetResult,
    ContextContentResult, ContextUsageResult, Empty, FsCreateDirParams, FsCreateDirResult,
    FsCreateFileParams, FsCreateFileResult, FsDeleteParams, FsFileEncoding, FsListDirParams,
    FsListDirResult, FsReadFileParams, FsReadFileResult, FsRenameParams, FsRenameResult,
    FsWriteFileParams, FsWriteFileResult, InitializeParams, InitializeResult, JobListParams,
    JobListResult, JobOutputParams, JobOutputResult, JobResult, JobTarget, PluginListParams,
    PluginListResult, PluginReloadParams, PluginReloadResult, PluginRunCommandParams,
    PluginRunCommandResult, PromptResult, ResourceCompleteParams, ResourceCompleteResult,
    ResourceListParams, ResourceListResult, ResourceReadParams, ResourceReadResult,
    ServerCapabilities, ServerInfo, SessionCompactParams, SessionContextContentParams,
    SessionContextParams, SessionDeleteParams, SessionGoalParams, SessionGoalResult, SessionInfo,
    SessionListParams, SessionListResult, SessionLoadPageParams, SessionLoadPageResult,
    SessionLoadParams, SessionLoadResult, SessionNewParams, SessionPlanApproveParams,
    SessionPlanApproveResult, SessionPlanDiffParams, SessionPlanDiffResult,
    SessionPlanReviseParams, SessionPlanReviseResult, SessionReaskParams, SessionRedoParams,
    SessionRenameParams, SessionRetryParams, SessionRewindParams, SessionSearchParams,
    SessionSearchResult, SessionSetAutonomyParams, SessionSetEngineParams,
    SessionSetFastModeParams, SessionSetModelParams, SessionSetModelResult,
    SessionSetPermissionParams, SessionUnloadParams, SessionUnlockParams, SessionUsageParams,
    SessionUsageResult, UnsteerParams, UnsteerResult, WebStateDocument, WebStateGetParams,
    WebStateSaveParams, WorkspaceGitBranchParams, WorkspaceGitBranchResult,
};
use moc_protocol::{AgentService, RequestContext, RpcError, RpcErrorCode};
use serde_json::json;
use std::{path::Path, sync::Arc};

impl AgentService for Core {
    async fn initialize(
        &self,
        ctx: RequestContext,
        params: InitializeParams,
    ) -> Result<InitializeResult, RpcError> {
        self.inner
            .capabilities
            .write()
            .insert(ctx.connection_id(), params.capabilities);
        Ok(InitializeResult {
            protocol_version: moc_protocol::PROTOCOL_VERSION.into(),
            server: ServerInfo {
                name: "moc-core".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            capabilities: ServerCapabilities {
                permission_requests: true,
                session_locking: self.inner.sessions.locking_enabled(),
                ask_requests: true,
            },
            tips: crate::tips::TIPS.iter().map(|&tip| tip.into()).collect(),
        })
    }

    async fn config_manage(
        &self,
        ctx: RequestContext,
        params: moc_protocol::ConfigAction,
    ) -> Result<moc_protocol::ConfigReply, RpcError> {
        management::manage(&self.inner, ctx, params)
            .await
            .map_err(to_rpc)
    }

    async fn skills_compact(
        &self,
        ctx: RequestContext,
        params: moc_protocol::SkillsCompactParams,
    ) -> Result<moc_protocol::SkillsCompactResult, RpcError> {
        let inner = Arc::clone(&self.inner);
        self.run_operation(ctx.cancel(), async move {
            crate::skills::compact::run(inner, ctx, params)
                .await
                .map_err(to_rpc)
        })
        .await
    }

    async fn web_state(
        &self,
        ctx: RequestContext,
        _params: WebStateGetParams,
    ) -> Result<WebStateDocument, RpcError> {
        management::web_state(&self.inner, &ctx)
            .await
            .map_err(to_rpc)
    }

    async fn web_state_save(
        &self,
        ctx: RequestContext,
        params: WebStateSaveParams,
    ) -> Result<WebStateDocument, RpcError> {
        management::web_state_save(&self.inner, &ctx, params)
            .await
            .map_err(to_rpc)
    }

    async fn session_steer(
        &self,
        _ctx: RequestContext,
        params: moc_protocol::SteerParams,
    ) -> Result<moc_protocol::SteerResult, RpcError> {
        management::steer(&self.inner, params).await.map_err(to_rpc)
    }

    async fn session_unsteer(
        &self,
        _ctx: RequestContext,
        params: UnsteerParams,
    ) -> Result<UnsteerResult, RpcError> {
        management::unsteer(&self.inner, params)
            .await
            .map_err(to_rpc)
    }

    async fn agent_spawn(
        &self,
        ctx: RequestContext,
        params: AgentSpawnParams,
    ) -> Result<AgentSpawnResult, RpcError> {
        crate::agents::spawn(
            Arc::clone(&self.inner),
            ctx,
            params.session_id,
            crate::agents::SpawnSpec {
                task: params.task,
                title: params.title,
                profile: crate::agents::SpawnProfile::FixedRole(params.role),
                seed_entries: Vec::new(),
            },
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_list(
        &self,
        _ctx: RequestContext,
        params: AgentListParams,
    ) -> Result<AgentListResult, RpcError> {
        crate::agents::list(&self.inner, params.session_id)
            .await
            .map_err(to_rpc)
    }

    async fn agent_message(
        &self,
        ctx: RequestContext,
        params: AgentMessageParams,
    ) -> Result<AgentMessageResult, RpcError> {
        crate::agents::message(
            Arc::clone(&self.inner),
            ctx,
            params.session_id,
            params.target,
            params.message,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_stop(
        &self,
        _ctx: RequestContext,
        params: AgentStopParams,
    ) -> Result<AgentStopResult, RpcError> {
        crate::agents::stop(
            &self.inner,
            params.session_id,
            params.agent_id,
            params.reason,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_wait(
        &self,
        ctx: RequestContext,
        params: AgentWaitParams,
    ) -> Result<AgentWaitResult, RpcError> {
        crate::agents::wait(
            &self.inner,
            &params.session_id,
            params.agent_ids,
            params.timeout_secs,
            None,
            &ctx.cancel(),
        )
        .await
        .map_err(to_rpc)
    }

    async fn job_list(
        &self,
        _ctx: RequestContext,
        params: JobListParams,
    ) -> Result<JobListResult, RpcError> {
        let runtime = self.inner.runtime(&params.session_id).await;
        let jobs = runtime.jobs.list();
        let mut outputs = std::collections::BTreeMap::new();
        if let Some(lines) = params.tail_lines {
            for job in &jobs {
                if job.status.is_terminal() {
                    continue;
                }
                if let Ok((_info, text, _path)) = runtime.jobs.output(&job.job_id, lines.min(2000))
                {
                    outputs.insert(job.job_id.clone(), text);
                }
            }
        }
        Ok(JobListResult { jobs, outputs })
    }

    async fn job_output(
        &self,
        _ctx: RequestContext,
        params: JobOutputParams,
    ) -> Result<JobOutputResult, RpcError> {
        let runtime = self.inner.runtime(&params.session_id).await;
        let lines = params.tail_lines.unwrap_or(200).min(2000);
        let (job, text, _path) = runtime
            .jobs
            .output(&params.job_id, lines)
            .map_err(job_error)?;
        Ok(JobOutputResult { job, text })
    }

    async fn job_kill(
        &self,
        _ctx: RequestContext,
        params: JobTarget,
    ) -> Result<JobResult, RpcError> {
        let runtime = self.inner.runtime(&params.session_id).await;
        let job = runtime.jobs.kill(&params.job_id).await.map_err(job_error)?;
        Ok(JobResult { job })
    }

    async fn job_background(
        &self,
        _ctx: RequestContext,
        params: JobTarget,
    ) -> Result<JobResult, RpcError> {
        let runtime = self.inner.runtime(&params.session_id).await;
        let job = runtime
            .jobs
            .background_by_user(&params.job_id)
            .await
            .map_err(job_error)?;
        Ok(JobResult { job })
    }

    async fn config_get(
        &self,
        _ctx: RequestContext,
        _params: ConfigGetParams,
    ) -> Result<ConfigGetResult, RpcError> {
        config_api::config_get(self, _ctx, _params).await
    }
    async fn plugin_list(
        &self,
        _ctx: RequestContext,
        params: PluginListParams,
    ) -> Result<PluginListResult, RpcError> {
        plugin_api::plugin_list(self, _ctx, params).await
    }

    async fn plugin_reload(
        &self,
        _ctx: RequestContext,
        params: PluginReloadParams,
    ) -> Result<PluginReloadResult, RpcError> {
        plugin_api::plugin_reload(self, _ctx, params).await
    }

    async fn plugin_run_command(
        &self,
        ctx: RequestContext,
        params: PluginRunCommandParams,
    ) -> Result<PluginRunCommandResult, RpcError> {
        plugin_api::plugin_run_command(self, ctx, params).await
    }

    async fn resource_complete(
        &self,
        ctx: RequestContext,
        params: ResourceCompleteParams,
    ) -> Result<ResourceCompleteResult, RpcError> {
        resource_api::resource_complete(self, ctx, params).await
    }

    async fn resource_list(
        &self,
        ctx: RequestContext,
        params: ResourceListParams,
    ) -> Result<ResourceListResult, RpcError> {
        resource_api::resource_list(self, ctx, params).await
    }

    async fn resource_read(
        &self,
        ctx: RequestContext,
        params: ResourceReadParams,
    ) -> Result<ResourceReadResult, RpcError> {
        resource_api::resource_read(self, ctx, params).await
    }

    async fn fs_list_dir(
        &self,
        _ctx: RequestContext,
        params: FsListDirParams,
    ) -> Result<FsListDirResult, RpcError> {
        let default_cwd = self.inner.config().cwd.clone();
        tokio::task::spawn_blocking(move || {
            crate::fs::list_dir(
                params.path.as_deref(),
                &default_cwd,
                params.show_hidden,
                params.include_files,
            )
        })
        .await
        .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
        .map_err(|error| invalid_params(error.to_string()))
    }

    async fn fs_create_dir(
        &self,
        _ctx: RequestContext,
        params: FsCreateDirParams,
    ) -> Result<FsCreateDirResult, RpcError> {
        tokio::task::spawn_blocking(move || crate::fs::create_dir(&params.parent, &params.name))
            .await
            .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
            .map_err(|error| invalid_params(error.to_string()))
    }

    async fn fs_read_file(
        &self,
        _ctx: RequestContext,
        params: FsReadFileParams,
    ) -> Result<FsReadFileResult, RpcError> {
        tokio::task::spawn_blocking(move || crate::fs::read_file(&params.path, params.max_bytes))
            .await
            .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
            .map(|read| FsReadFileResult {
                path: read.path.to_string_lossy().into_owned(),
                size: read.size,
                modified_ms: read.modified_ms,
                encoding: if read.binary {
                    FsFileEncoding::Binary
                } else {
                    FsFileEncoding::Utf8
                },
                content: read.content,
                truncated: read.truncated,
            })
            .map_err(|error| invalid_params(error.to_string()))
    }

    async fn fs_write_file(
        &self,
        _ctx: RequestContext,
        params: FsWriteFileParams,
    ) -> Result<FsWriteFileResult, RpcError> {
        tokio::task::spawn_blocking(move || {
            crate::fs::write_file(&params.path, &params.content, params.expected_modified_ms)
        })
        .await
        .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
        .map(|write| FsWriteFileResult {
            path: write.path.to_string_lossy().into_owned(),
            size: write.size,
            modified_ms: write.modified_ms,
        })
        .map_err(|error| match error {
            crate::fs::WriteFileError::Conflict { modified_ms } => {
                RpcError::of(RpcErrorCode::FileConflict).with_data(json!({
                    "modified_ms": modified_ms,
                }))
            }
            other => invalid_params(other.to_string()),
        })
    }

    async fn fs_create_file(
        &self,
        _ctx: RequestContext,
        params: FsCreateFileParams,
    ) -> Result<FsCreateFileResult, RpcError> {
        tokio::task::spawn_blocking(move || crate::fs::create_file(&params.parent, &params.name))
            .await
            .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
            .map_err(|error| invalid_params(error.to_string()))
    }

    async fn fs_rename(
        &self,
        _ctx: RequestContext,
        params: FsRenameParams,
    ) -> Result<FsRenameResult, RpcError> {
        tokio::task::spawn_blocking(move || crate::fs::rename(&params.from, &params.to))
            .await
            .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
            .map_err(|error| invalid_params(error.to_string()))
    }

    async fn fs_delete(
        &self,
        _ctx: RequestContext,
        params: FsDeleteParams,
    ) -> Result<Empty, RpcError> {
        tokio::task::spawn_blocking(move || crate::fs::delete(&params.path))
            .await
            .map_err(|error| RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()))?
            .map_err(|error| invalid_params(error.to_string()))?;
        Ok(Empty {})
    }

    async fn chart_analyze(
        &self,
        _ctx: RequestContext,
        params: ChartAnalyzeParams,
    ) -> Result<ChartAnalyzeResult, RpcError> {
        resource_api::chart_analyze(_ctx, params).await
    }

    async fn workspace_git_branch(
        &self,
        _ctx: RequestContext,
        params: WorkspaceGitBranchParams,
    ) -> Result<WorkspaceGitBranchResult, RpcError> {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(Path::new(&params.cwd))
            .output()
            .await
            .ok();
        let branch = output
            .filter(|result| result.status.success())
            .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_owned())
            .filter(|branch| !branch.is_empty());
        Ok(WorkspaceGitBranchResult { branch })
    }

    async fn session_new(
        &self,
        ctx: RequestContext,
        params: SessionNewParams,
    ) -> Result<SessionInfo, RpcError> {
        session_api::session_new(self, ctx, params).await
    }

    async fn session_list(
        &self,
        _ctx: RequestContext,
        _params: SessionListParams,
    ) -> Result<SessionListResult, RpcError> {
        let mut sessions = self.inner.sessions.list().await.map_err(to_rpc)?;
        for session in &mut sessions {
            session.permission = self.inner.effective_permission(session.permission);
        }
        Ok(SessionListResult { sessions })
    }

    async fn session_search(
        &self,
        _ctx: RequestContext,
        params: SessionSearchParams,
    ) -> Result<SessionSearchResult, RpcError> {
        let limit = params.limit.unwrap_or(20).clamp(1, 100) as usize;
        let (mut matches, truncated) = self
            .inner
            .sessions
            .search(&params.query, params.cwd.as_deref(), limit)
            .await
            .map_err(to_rpc)?;
        for hit in &mut matches {
            hit.session.permission = self.inner.effective_permission(hit.session.permission);
        }
        Ok(SessionSearchResult { matches, truncated })
    }

    async fn session_load(
        &self,
        ctx: RequestContext,
        params: SessionLoadParams,
    ) -> Result<SessionLoadResult, RpcError> {
        session_api::session_load(self, ctx, params).await
    }

    fn session_history(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::SessionHistoryParams,
    ) -> impl std::future::Future<
        Output = Result<moc_protocol::methods::SessionHistoryResult, RpcError>,
    > + Send {
        history::handle(self, ctx, params)
    }

    async fn session_load_page(
        &self,
        _ctx: RequestContext,
        params: SessionLoadPageParams,
    ) -> Result<SessionLoadPageResult, RpcError> {
        session_api::session_load_page(self, _ctx, params).await
    }
    async fn session_rewind(
        &self,
        ctx: RequestContext,
        params: SessionRewindParams,
    ) -> Result<SessionLoadResult, RpcError> {
        session_api::session_rewind(self, ctx, params).await
    }

    async fn session_reask(
        &self,
        ctx: RequestContext,
        params: SessionReaskParams,
    ) -> Result<SessionLoadResult, RpcError> {
        session_api::session_reask(self, ctx, params).await
    }

    async fn session_redo(
        &self,
        ctx: RequestContext,
        params: SessionRedoParams,
    ) -> Result<SessionLoadResult, RpcError> {
        session_api::session_redo(self, ctx, params).await
    }

    async fn session_unload(
        &self,
        _ctx: RequestContext,
        params: SessionUnloadParams,
    ) -> Result<Empty, RpcError> {
        session_api::session_unload(self, _ctx, params).await
    }

    async fn session_rename(
        &self,
        ctx: RequestContext,
        params: SessionRenameParams,
    ) -> Result<SessionInfo, RpcError> {
        session_api::session_rename(self, ctx, params).await
    }

    async fn session_delete(
        &self,
        _ctx: RequestContext,
        params: SessionDeleteParams,
    ) -> Result<Empty, RpcError> {
        session_api::session_delete(self, _ctx, params).await
    }

    async fn session_unlock(
        &self,
        _ctx: RequestContext,
        params: SessionUnlockParams,
    ) -> Result<Empty, RpcError> {
        session_api::session_unlock(self, _ctx, params).await
    }

    async fn session_set_permission(
        &self,
        ctx: RequestContext,
        params: SessionSetPermissionParams,
    ) -> Result<SessionInfo, RpcError> {
        session_config::session_set_permission(self, ctx, params).await
    }

    async fn session_set_engine(
        &self,
        ctx: RequestContext,
        params: SessionSetEngineParams,
    ) -> Result<SessionInfo, RpcError> {
        session_config::session_set_engine(self, ctx, params).await
    }

    async fn session_set_autonomy(
        &self,
        ctx: RequestContext,
        params: SessionSetAutonomyParams,
    ) -> Result<SessionInfo, RpcError> {
        session_config::session_set_autonomy(self, ctx, params).await
    }

    async fn session_set_fast_mode(
        &self,
        ctx: RequestContext,
        params: SessionSetFastModeParams,
    ) -> Result<SessionInfo, RpcError> {
        session_config::session_set_fast_mode(self, ctx, params).await
    }

    async fn session_goal(
        &self,
        ctx: RequestContext,
        params: SessionGoalParams,
    ) -> Result<SessionGoalResult, RpcError> {
        crate::goal::apply_action(&self.inner, &ctx, &params.session_id, params.action)
            .await
            .map(|goal| SessionGoalResult { goal })
    }

    fn session_plan_mode(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::SessionPlanModeParams,
    ) -> impl std::future::Future<Output = Result<SessionInfo, RpcError>> + Send {
        mode::plan_mode(&self.inner, ctx, params)
    }

    async fn session_set_model(
        &self,
        ctx: RequestContext,
        params: SessionSetModelParams,
    ) -> Result<SessionSetModelResult, RpcError> {
        session_config::session_set_model(self, ctx, params).await
    }

    async fn session_plan_approve(
        &self,
        ctx: RequestContext,
        params: SessionPlanApproveParams,
    ) -> Result<SessionPlanApproveResult, RpcError> {
        session_api::session_plan_approve(self, ctx, params).await
    }

    fn session_plan_revise(
        &self,
        ctx: RequestContext,
        params: SessionPlanReviseParams,
    ) -> impl std::future::Future<Output = Result<SessionPlanReviseResult, RpcError>> + Send {
        mode::revise_plan(&self.inner, ctx, params)
    }

    async fn session_plan_diff(
        &self,
        _ctx: RequestContext,
        params: SessionPlanDiffParams,
    ) -> Result<SessionPlanDiffResult, RpcError> {
        mode::plan_diff(&self.inner, params).await
    }

    // —— workflow（§9）——

    async fn session_workflow_activate(
        &self,
        ctx: RequestContext,
        params: moc_protocol::SessionWorkflowActivateParams,
    ) -> Result<SessionInfo, RpcError> {
        workflow_api::session_workflow_activate(self, ctx, params).await
    }

    async fn session_workflow_approve(
        &self,
        ctx: RequestContext,
        params: moc_protocol::SessionWorkflowApproveParams,
    ) -> Result<moc_protocol::WorkflowAck, RpcError> {
        workflow_api::session_workflow_approve(self, ctx, params).await
    }

    async fn session_workflow_runs(
        &self,
        _ctx: RequestContext,
        params: moc_protocol::SessionWorkflowRunsParams,
    ) -> Result<moc_protocol::SessionWorkflowRunsResult, RpcError> {
        workflow_api::session_workflow_runs(self, _ctx, params).await
    }

    async fn session_workflow_run(
        &self,
        _ctx: RequestContext,
        params: moc_protocol::SessionWorkflowRunParams,
    ) -> Result<moc_protocol::WorkflowRunSnapshot, RpcError> {
        workflow_api::session_workflow_run(self, _ctx, params).await
    }

    async fn session_workflow_stop(
        &self,
        ctx: RequestContext,
        params: moc_protocol::SessionWorkflowRunParams,
    ) -> Result<moc_protocol::WorkflowRunSummary, RpcError> {
        workflow_api::session_workflow_stop(self, ctx, params).await
    }

    async fn session_workflow_resume(
        &self,
        ctx: RequestContext,
        params: moc_protocol::SessionWorkflowRunParams,
    ) -> Result<moc_protocol::WorkflowRunSummary, RpcError> {
        workflow_api::session_workflow_resume(self, ctx, params).await
    }

    async fn session_workflow_set_role_model(
        &self,
        ctx: RequestContext,
        params: moc_protocol::SessionWorkflowSetRoleModelParams,
    ) -> Result<moc_protocol::WorkflowAck, RpcError> {
        workflow_api::session_workflow_set_role_model(self, ctx, params).await
    }

    async fn session_workflow_artifact(
        &self,
        _ctx: RequestContext,
        params: moc_protocol::SessionWorkflowArtifactParams,
    ) -> Result<moc_protocol::WorkflowArtifactResult, RpcError> {
        workflow_api::session_workflow_artifact(self, _ctx, params).await
    }

    async fn session_compact(
        &self,
        ctx: RequestContext,
        params: SessionCompactParams,
    ) -> Result<CompactResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let result = compact::manual(
            &self.inner,
            &ctx,
            &params.session_id,
            params.instructions.as_deref(),
            None,
            None,
        )
        .await
        .map_err(to_rpc)?;
        Ok(result)
    }

    fn session_tools(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::SessionToolsParams,
    ) -> impl std::future::Future<Output = Result<Vec<moc_llm::ToolDef>, RpcError>> + Send {
        context::tools(&self.inner, ctx, params)
    }

    fn session_context(
        &self,
        ctx: RequestContext,
        params: SessionContextParams,
    ) -> impl std::future::Future<Output = Result<ContextUsageResult, RpcError>> + Send {
        context::usage(&self.inner, ctx, params)
    }

    fn session_context_content(
        &self,
        ctx: RequestContext,
        params: SessionContextContentParams,
    ) -> impl std::future::Future<Output = Result<ContextContentResult, RpcError>> + Send {
        context::content(&self.inner, ctx, params)
    }
    fn session_context_read(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::SessionContextReadParams,
    ) -> impl std::future::Future<
        Output = Result<moc_protocol::methods::ContextReadResult, RpcError>,
    > + Send {
        context::read(&self.inner, ctx, params)
    }
    fn session_context_close(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::SessionContextCloseParams,
    ) -> impl std::future::Future<Output = Result<Empty, RpcError>> + Send {
        context::close(&self.inner, ctx, params)
    }
    async fn session_usage(
        &self,
        _ctx: RequestContext,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, RpcError> {
        usage::session_usage(self, _ctx, params).await
    }

    async fn session_bash(
        &self,
        ctx: RequestContext,
        params: moc_protocol::BashParams,
    ) -> Result<moc_protocol::BashResult, RpcError> {
        self.run_operation(
            ctx.cancel(),
            bash::run(Arc::clone(&self.inner), ctx, params),
        )
        .await
    }
    async fn session_btw(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::BtwParams,
    ) -> Result<moc_protocol::methods::BtwResult, RpcError> {
        self.run_operation(ctx.cancel(), btw::run(Arc::clone(&self.inner), ctx, params))
            .await
    }

    async fn session_prompt(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::PromptParams,
    ) -> Result<PromptResult, RpcError> {
        self.run_operation(
            ctx.cancel(),
            prompt::run_prompt(Arc::clone(&self.inner), ctx, params),
        )
        .await
    }

    async fn session_retry(
        &self,
        ctx: RequestContext,
        params: SessionRetryParams,
    ) -> Result<PromptResult, RpcError> {
        self.run_operation(
            ctx.cancel(),
            prompt::run_retry(Arc::clone(&self.inner), ctx, params),
        )
        .await
    }
}
