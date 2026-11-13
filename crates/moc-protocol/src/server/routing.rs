use super::SKILLS_FLIGHT;
use super::ServerDispatch;
use crate::connection::params_from_value;
use crate::connection::value_from;
use crate::error::RpcError;
use crate::error::RpcErrorCode;
use crate::method::Method;
use crate::methods::AgentList;
use crate::methods::AgentMessage;
use crate::methods::AgentSpawn;
use crate::methods::AgentStop;
use crate::methods::AgentWait;
use crate::methods::BtwUpdate;
use crate::methods::ChartAnalyze;
use crate::methods::ClientInfo;
use crate::methods::ConfigGet;
use crate::methods::ConfigManage;
use crate::methods::FsCreateDir;
use crate::methods::FsCreateFile;
use crate::methods::FsDelete;
use crate::methods::FsListDir;
use crate::methods::FsReadFile;
use crate::methods::FsRename;
use crate::methods::FsWriteFile;
use crate::methods::JobBackground;
use crate::methods::JobKill;
use crate::methods::JobList;
use crate::methods::JobReadOutput;
use crate::methods::PluginList;
use crate::methods::PluginReload;
use crate::methods::PluginRunCommand;
use crate::methods::ResourceComplete;
use crate::methods::ResourceList;
use crate::methods::ResourceRead;
use crate::methods::SessionBash;
use crate::methods::SessionBtw;
use crate::methods::SessionBtwCancel;
use crate::methods::SessionCancel;
use crate::methods::SessionCompact;
use crate::methods::SessionContext;
use crate::methods::SessionContextContent;
use crate::methods::SessionDelete;
use crate::methods::SessionGoal;
use crate::methods::SessionList;
use crate::methods::SessionLoad;
use crate::methods::SessionLoadPage;
use crate::methods::SessionNew;
use crate::methods::SessionPlanApprove;
use crate::methods::SessionPlanDiff;
use crate::methods::SessionPlanMode;
use crate::methods::SessionPlanRevise;
use crate::methods::SessionPrompt;
use crate::methods::SessionReask;
use crate::methods::SessionRedo;
use crate::methods::SessionRename;
use crate::methods::SessionRetry;
use crate::methods::SessionRewind;
use crate::methods::SessionSearch;
use crate::methods::SessionSetAutonomy;
use crate::methods::SessionSetEngine;
use crate::methods::SessionSetFastMode;
use crate::methods::SessionSetModel;
use crate::methods::SessionSetPermission;
use crate::methods::SessionSteer;
use crate::methods::SessionUnload;
use crate::methods::SessionUnlock;
use crate::methods::SessionUnsteer;
use crate::methods::SessionUsage;
use crate::methods::SessionWorkflowActivate;
use crate::methods::SessionWorkflowApprove;
use crate::methods::SessionWorkflowArtifact;
use crate::methods::SessionWorkflowResume;
use crate::methods::SessionWorkflowRun;
use crate::methods::SessionWorkflowRuns;
use crate::methods::SessionWorkflowSetRoleModel;
use crate::methods::SessionWorkflowStop;
use crate::methods::SkillsCancel;
use crate::methods::SkillsCompact;
use crate::methods::WebStateGet;
use crate::methods::WebStateSave;
use crate::methods::WorkspaceGitBranch;
use crate::service::AgentService;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl<S: AgentService> ServerDispatch<S> {
    /// 已握手连接的方法分发。
    ///
    /// 占位守卫由 [`ServerDispatch::dispatch`] 持有，这里不再接触注册表。
    pub(super) async fn route(
        self: &Arc<Self>,
        client: ClientInfo,
        method: &str,
        params: Option<Value>,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            <SkillsCompact as Method>::NAME => {
                let params = params_from_value::<SkillsCompact>(params)?;
                value_from(
                    self.server
                        .service
                        .skills_compact(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SkillsCancel as Method>::NAME => {
                let _ = params_from_value::<SkillsCancel>(params)?;
                value_from(self.cancel_prompt(SKILLS_FLIGHT))
            }
            <ConfigManage as Method>::NAME => {
                let params = params_from_value::<ConfigManage>(params)?;
                value_from(
                    self.server
                        .service
                        .config_manage(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionSteer as Method>::NAME => {
                let params = params_from_value::<SessionSteer>(params)?;
                value_from(
                    self.server
                        .service
                        .session_steer(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionUnsteer as Method>::NAME => {
                let params = params_from_value::<SessionUnsteer>(params)?;
                value_from(
                    self.server
                        .service
                        .session_unsteer(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ConfigGet as Method>::NAME => {
                let params = params_from_value::<ConfigGet>(params)?;
                let result = self
                    .server
                    .service
                    .config_get(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <WebStateGet as Method>::NAME => {
                let params = params_from_value::<WebStateGet>(params)?;
                value_from(
                    self.server
                        .service
                        .web_state(self.context(client, cancel), params)
                        .await?,
                )
            }
            <WebStateSave as Method>::NAME => {
                let params = params_from_value::<WebStateSave>(params)?;
                value_from(
                    self.server
                        .service
                        .web_state_save(self.context(client, cancel), params)
                        .await?,
                )
            }
            <PluginList as Method>::NAME => {
                let params = params_from_value::<PluginList>(params)?;
                value_from(
                    self.server
                        .service
                        .plugin_list(self.context(client, cancel), params)
                        .await?,
                )
            }
            <PluginReload as Method>::NAME => {
                let params = params_from_value::<PluginReload>(params)?;
                value_from(
                    self.server
                        .service
                        .plugin_reload(self.context(client, cancel), params)
                        .await?,
                )
            }
            <PluginRunCommand as Method>::NAME => {
                let params = params_from_value::<PluginRunCommand>(params)?;
                value_from(
                    self.server
                        .service
                        .plugin_run_command(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ResourceComplete as Method>::NAME => {
                let params = params_from_value::<ResourceComplete>(params)?;
                value_from(
                    self.server
                        .service
                        .resource_complete(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ResourceList as Method>::NAME => {
                let params = params_from_value::<ResourceList>(params)?;
                value_from(
                    self.server
                        .service
                        .resource_list(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ResourceRead as Method>::NAME => {
                let params = params_from_value::<ResourceRead>(params)?;
                value_from(
                    self.server
                        .service
                        .resource_read(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsListDir as Method>::NAME => {
                let params = params_from_value::<FsListDir>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_list_dir(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsCreateDir as Method>::NAME => {
                let params = params_from_value::<FsCreateDir>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_create_dir(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsReadFile as Method>::NAME => {
                let params = params_from_value::<FsReadFile>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_read_file(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsWriteFile as Method>::NAME => {
                let params = params_from_value::<FsWriteFile>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_write_file(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsCreateFile as Method>::NAME => {
                let params = params_from_value::<FsCreateFile>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_create_file(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsRename as Method>::NAME => {
                let params = params_from_value::<FsRename>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_rename(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsDelete as Method>::NAME => {
                let params = params_from_value::<FsDelete>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_delete(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ChartAnalyze as Method>::NAME => {
                let params = params_from_value::<ChartAnalyze>(params)?;
                value_from(
                    self.server
                        .service
                        .chart_analyze(self.context(client, cancel), params)
                        .await?,
                )
            }
            <WorkspaceGitBranch as Method>::NAME => {
                let params = params_from_value::<WorkspaceGitBranch>(params)?;
                value_from(
                    self.server
                        .service
                        .workspace_git_branch(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionNew as Method>::NAME => {
                let params = params_from_value::<SessionNew>(params)?;
                let result = self
                    .server
                    .service
                    .session_new(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionList as Method>::NAME => {
                let params = params_from_value::<SessionList>(params)?;
                let result = self
                    .server
                    .service
                    .session_list(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSearch as Method>::NAME => {
                let params = params_from_value::<SessionSearch>(params)?;
                let result = self
                    .server
                    .service
                    .session_search(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionLoad as Method>::NAME => {
                let params = params_from_value::<SessionLoad>(params)?;
                let result = self
                    .server
                    .service
                    .session_load(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <crate::methods::SessionHistory as Method>::NAME => {
                let params = params_from_value::<crate::methods::SessionHistory>(params)?;
                value_from(
                    self.server
                        .service
                        .session_history(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionLoadPage as Method>::NAME => {
                let params = params_from_value::<SessionLoadPage>(params)?;
                let result = self
                    .server
                    .service
                    .session_load_page(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionRewind as Method>::NAME => {
                let params = params_from_value::<SessionRewind>(params)?;
                let result = self
                    .server
                    .service
                    .session_rewind(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionRedo as Method>::NAME => {
                let params = params_from_value::<SessionRedo>(params)?;
                let result = self
                    .server
                    .service
                    .session_redo(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionReask as Method>::NAME => {
                let params = params_from_value::<SessionReask>(params)?;
                let result = self
                    .server
                    .service
                    .session_reask(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionUnload as Method>::NAME => {
                let params = params_from_value::<SessionUnload>(params)?;
                let result = self
                    .server
                    .service
                    .session_unload(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionRename as Method>::NAME => {
                let params = params_from_value::<SessionRename>(params)?;
                let result = self
                    .server
                    .service
                    .session_rename(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionDelete as Method>::NAME => {
                let params = params_from_value::<SessionDelete>(params)?;
                let result = self
                    .server
                    .service
                    .session_delete(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionUnlock as Method>::NAME => {
                let params = params_from_value::<SessionUnlock>(params)?;
                let result = self
                    .server
                    .service
                    .session_unlock(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetEngine as Method>::NAME => {
                let params = params_from_value::<SessionSetEngine>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_engine(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetAutonomy as Method>::NAME => {
                let params = params_from_value::<SessionSetAutonomy>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_autonomy(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetPermission as Method>::NAME => {
                let params = params_from_value::<SessionSetPermission>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_permission(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetModel as Method>::NAME => {
                let params = params_from_value::<SessionSetModel>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_model(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetFastMode as Method>::NAME => {
                let params = params_from_value::<SessionSetFastMode>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_fast_mode(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionGoal as Method>::NAME => {
                let params = params_from_value::<SessionGoal>(params)?;
                let result = self
                    .server
                    .service
                    .session_goal(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionPlanMode as Method>::NAME => {
                let params = params_from_value::<SessionPlanMode>(params)?;
                let result = self
                    .server
                    .service
                    .session_plan_mode(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionPlanApprove as Method>::NAME => {
                let params = params_from_value::<SessionPlanApprove>(params)?;
                let result = self
                    .server
                    .service
                    .session_plan_approve(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionPlanRevise as Method>::NAME => {
                let params = params_from_value::<SessionPlanRevise>(params)?;
                let result = self
                    .server
                    .service
                    .session_plan_revise(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionPlanDiff as Method>::NAME => {
                let params = params_from_value::<SessionPlanDiff>(params)?;
                let result = self
                    .server
                    .service
                    .session_plan_diff(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            // —— workflow（§9）：激活、审批、run 查询与控制 ——
            <SessionWorkflowActivate as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowActivate>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_activate(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowApprove as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowApprove>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_approve(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowRuns as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowRuns>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_runs(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowRun as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowRun>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_run(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowStop as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowRun>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_stop(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowResume as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowRun>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_resume(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowSetRoleModel as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowSetRoleModel>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_set_role_model(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionWorkflowArtifact as Method>::NAME => {
                let params = params_from_value::<SessionWorkflowArtifact>(params)?;
                let result = self
                    .server
                    .service
                    .session_workflow_artifact(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionCompact as Method>::NAME => {
                let params = params_from_value::<SessionCompact>(params)?;
                let result = self
                    .server
                    .service
                    .session_compact(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }

            <crate::methods::SessionTools as Method>::NAME => {
                let params = params_from_value::<crate::methods::SessionTools>(params)?;
                value_from(
                    self.server
                        .service
                        .session_tools(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionContext as Method>::NAME => {
                let params = params_from_value::<SessionContext>(params)?;
                let result = self
                    .server
                    .service
                    .session_context(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionContextContent as Method>::NAME => {
                let params = params_from_value::<SessionContextContent>(params)?;
                let result = self
                    .server
                    .service
                    .session_context_content(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <crate::methods::SessionContextRead as Method>::NAME => {
                let params = params_from_value::<crate::methods::SessionContextRead>(params)?;
                value_from(
                    self.server
                        .service
                        .session_context_read(self.context(client, cancel), params)
                        .await?,
                )
            }
            <crate::methods::SessionContextClose as Method>::NAME => {
                let params = params_from_value::<crate::methods::SessionContextClose>(params)?;
                value_from(
                    self.server
                        .service
                        .session_context_close(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionUsage as Method>::NAME => {
                let params = params_from_value::<SessionUsage>(params)?;
                let result = self
                    .server
                    .service
                    .session_usage(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionBash as Method>::NAME => {
                let params = params_from_value::<SessionBash>(params)?;
                value_from(
                    self.server
                        .service
                        .session_bash(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionPrompt as Method>::NAME => {
                // 占位已在读循环里完成（保证「先提示后 cancel」的顺序），守卫由
                // `dispatch` 持有：解析失败、core 报错、未握手被拒都会摘掉占位
                let params = params_from_value::<SessionPrompt>(params)?;
                self.prompt(client, params, cancel).await
            }
            <SessionRetry as Method>::NAME => {
                // 与提示共用注册表、守卫与取消落点；core 侧不追加用户消息。
                let params = params_from_value::<SessionRetry>(params)?;
                value_from(
                    self.server
                        .service
                        .session_retry(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionBtw as Method>::NAME => {
                let params = params_from_value::<SessionBtw>(params)?;
                let ctx = self.context(client, cancel);
                // `Started` 必须在 core 动手之前入队：前端据此把「Started 之前收到的
                // 取消意图」换成真正的定向取消，不会因为取消早于登记而落空。
                ctx.updates()
                    .emit_btw(
                        params.session_id.clone(),
                        params.request_id,
                        BtwUpdate::Started,
                    )
                    .await
                    .map_err(|error| {
                        RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
                    })?;
                value_from(self.server.service.session_btw(ctx, params).await?)
            }
            // 旁路取消只 trip 对应请求的令牌；主提示与其它旁路完全不受影响。
            <SessionBtwCancel as Method>::NAME => {
                let params = params_from_value::<SessionBtwCancel>(params)?;
                value_from(self.cancel_btw(&params.session_id, params.request_id))
            }
            // 取消由协议层自答：trip 令牌即可，不经过 core，也不受出站队列拥塞影响
            <SessionCancel as Method>::NAME => {
                let params = params_from_value::<SessionCancel>(params)?;
                value_from(self.cancel_prompt(&params.session_id))
            }
            <AgentSpawn as Method>::NAME => {
                let params = params_from_value::<AgentSpawn>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_spawn(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentList as Method>::NAME => {
                let params = params_from_value::<AgentList>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_list(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentMessage as Method>::NAME => {
                let params = params_from_value::<AgentMessage>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_message(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentStop as Method>::NAME => {
                let params = params_from_value::<AgentStop>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_stop(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentWait as Method>::NAME => {
                let params = params_from_value::<AgentWait>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_wait(self.context(client, cancel), params)
                        .await?,
                )
            }
            <JobList as Method>::NAME => {
                let params = params_from_value::<JobList>(params)?;
                value_from(
                    self.server
                        .service
                        .job_list(self.context(client, cancel), params)
                        .await?,
                )
            }
            <JobReadOutput as Method>::NAME => {
                let params = params_from_value::<JobReadOutput>(params)?;
                value_from(
                    self.server
                        .service
                        .job_output(self.context(client, cancel), params)
                        .await?,
                )
            }
            <JobKill as Method>::NAME => {
                let params = params_from_value::<JobKill>(params)?;
                value_from(
                    self.server
                        .service
                        .job_kill(self.context(client, cancel), params)
                        .await?,
                )
            }
            <JobBackground as Method>::NAME => {
                let params = params_from_value::<JobBackground>(params)?;
                value_from(
                    self.server
                        .service
                        .job_background(self.context(client, cancel), params)
                        .await?,
                )
            }
            _ => Err(RpcError::of(RpcErrorCode::MethodNotFound)),
        }
    }
}
