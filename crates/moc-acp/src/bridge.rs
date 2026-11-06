//! moc → ACP 方向的胶合：会话更新转发，以及 core 反向请求到 ACP 客户端的桥接。

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAcceptAction, ElicitationAction, ElicitationContentValue,
    ElicitationFormMode, ElicitationMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, EnumOption, MultiSelectPropertySchema,
    PermissionOption as AcpPermissionOption, PermissionOptionKind as AcpPermissionKind,
    RequestPermissionOutcome, RequestPermissionRequest, StringPropertySchema, ToolCallUpdate,
    ToolCallUpdateFields,
};
use moc_protocol::error::{RpcError, RpcErrorCode};
use moc_protocol::methods::{
    AskAnswer, AskParams, AskQuestion, AskResult, ClientCapabilities, ClientInfo,
    PermissionOptionKind as MocPermissionKind, PermissionRequestParams, PermissionResult,
    PlanAction, SessionPlanApprove, SessionPlanApproveParams, SessionRequestAsk,
    SessionRequestPermission, SessionUpdate as MocUpdate, SessionUpdateMethod, SessionUpdateParams,
};
use moc_protocol::{ClientBuilder, ClientRequestContext};

use crate::backend::Backend;
use crate::{Bridge, translate};

impl<B: Backend> Bridge<B> {
    /// 进程内 core 客户端：注册会话更新通知与权限 / ask 反向请求处理器。
    ///
    /// 通知到达早于 `connect` 返回——更新处理器只依赖 `Bridge` 内部槽位，
    /// `session/new` 响应写入会话投影前不可能有该会话的更新。
    pub(crate) fn moc_client_builder(self: &std::sync::Arc<Self>) -> ClientBuilder {
        let update_bridge = self.clone();
        let permission_bridge = self.clone();
        let ask_bridge = self.clone();
        ClientBuilder::new(ClientInfo {
            name: "moc-acp".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            frontend: "acp".into(),
        })
        .capabilities(ClientCapabilities {
            permission_requests: true,
            ask_requests: true,
        })
        .on_notification::<SessionUpdateMethod, _, _>(move |params, _ctx| {
            update_bridge.forward_update(params);
            std::future::ready(())
        })
        .on_request::<SessionRequestPermission, _, _>(move |params, ctx| {
            let bridge = permission_bridge.clone();
            async move { bridge.request_permission(params, ctx).await }
        })
        .on_request::<SessionRequestAsk, _, _>(move |params, ctx| {
            let bridge = ask_bridge.clone();
            async move { bridge.request_ask(params, ctx).await }
        })
    }

    /// `session/update` 通知 → 0..n 条 ACP `session/update` 通知。
    ///
    /// 同步处理：更新只排队进 ACP 的出站通道，不占住 moc 侧的 dispatch。
    fn forward_update(&self, params: SessionUpdateParams) {
        // 无人值守档位的计划提交由这里自动批准——与 REPL 的语义一致。
        if let MocUpdate::PlanSubmitted { plan } = &params.update
            && self
                .projection(&params.session_id)
                .is_some_and(|p| p.autonomy == moc_protocol::methods::SessionAutonomy::Unattended)
            && let Some(moc) = self.moc()
        {
            let session_id = params.session_id.clone();
            let plan_id = plan.plan_id.clone();
            tokio::spawn(async move {
                let _ = moc
                    .call::<SessionPlanApprove>(SessionPlanApproveParams {
                        session_id,
                        plan_id,
                        action: PlanAction::ExecuteCompacted,
                        execution_engine: None,
                    })
                    .await;
            });
        }
        let updates = self.translate_update(&params.session_id, &params.update);
        self.emit(&params.session_id, updates);
    }

    /// `session/request_permission`（core→前端）→ ACP `session/request_permission`。
    ///
    /// moc 的 dispatch 任务与 ACP 的 dispatch 各自独立，这里直接 `block_task()`
    /// 等客户端答复是安全的；连接级取消转 `RequestCancelled`，core 按拒绝处理。
    async fn request_permission(
        &self,
        params: PermissionRequestParams,
        ctx: ClientRequestContext,
    ) -> Result<PermissionResult, RpcError> {
        let Some(cx) = self.acp() else {
            return Err(RpcError::of(RpcErrorCode::MethodNotFound));
        };
        let mut content = Vec::new();
        if let Some(body) = &params.body {
            content.push(agent_client_protocol::schema::v1::ToolCallContent::Content(
                agent_client_protocol::schema::v1::Content::new(
                    agent_client_protocol::schema::v1::ContentBlock::Text(
                        agent_client_protocol::schema::v1::TextContent::new(body.clone()),
                    ),
                ),
            ));
        }
        if let Some(diff) = &params.diff {
            content.extend(translate::diff_content(diff));
        }
        let tool_call = ToolCallUpdate::new(
            params.tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .title(params.title.clone())
                .content(content),
        );
        let options = params
            .options
            .iter()
            .map(|o| {
                AcpPermissionOption::new(
                    o.id.clone(),
                    o.label.clone(),
                    match o.kind {
                        MocPermissionKind::AllowOnce => AcpPermissionKind::AllowOnce,
                        MocPermissionKind::AllowSession => AcpPermissionKind::AllowAlways,
                        MocPermissionKind::RejectOnce => AcpPermissionKind::RejectOnce,
                    },
                )
            })
            .collect();
        let request = RequestPermissionRequest::new(params.session_id.clone(), tool_call, options);
        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(RpcError::of(RpcErrorCode::RequestCancelled)),
            response = cx.send_request(request).block_task() => response,
        };
        match response {
            Ok(response) => match response.outcome {
                RequestPermissionOutcome::Selected(selected) => Ok(PermissionResult {
                    option_id: selected.option_id.0.to_string(),
                }),
                _ => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
            },
            Err(error) => Err(RpcError::new(i32::from(error.code), error.message)),
        }
    }

    /// `session/request_ask`（core→前端）→ ACP `elicit`。
    ///
    /// 客户端不支持 form elicitation 时明确 MethodNotFound——core 据此前端无 ask
    /// 能力自动降级。回答映射回 `AskAnswer`：选项答案进 `selected`，表单里
    /// 手填的非选项值进 `custom`。
    async fn request_ask(
        &self,
        params: AskParams,
        ctx: ClientRequestContext,
    ) -> Result<AskResult, RpcError> {
        let form_supported = self
            .client_caps()
            .elicitation
            .and_then(|e| e.form)
            .is_some();
        if !form_supported {
            return Err(RpcError::of(RpcErrorCode::MethodNotFound));
        }
        let Some(cx) = self.acp() else {
            return Err(RpcError::of(RpcErrorCode::MethodNotFound));
        };
        let (message, schema) = ask_elicitation(&params.questions);
        let request = CreateElicitationRequest::new(
            ElicitationMode::Form(ElicitationFormMode::new(
                ElicitationSessionScope::new(params.session_id.clone()),
                schema,
            )),
            message,
        );
        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(RpcError::of(RpcErrorCode::RequestCancelled)),
            response = cx.send_request(request).block_task() => response,
        };
        match response {
            Ok(response) => match response.action {
                ElicitationAction::Accept(ElicitationAcceptAction { content, .. }) => {
                    Ok(ask_result(&params.questions, content.as_ref()))
                }
                _ => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
            },
            Err(error) => Err(RpcError::new(i32::from(error.code), error.message)),
        }
    }
}

/// moc ask 问题集 → ACP form elicitation（单问题消息=问题正文，多问题拼接）。
fn ask_elicitation(questions: &[AskQuestion]) -> (String, ElicitationSchema) {
    let message = if questions.len() == 1 {
        questions[0].question.clone()
    } else {
        questions
            .iter()
            .map(|q| q.question.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut schema = ElicitationSchema::new();
    for q in questions {
        let property = if q.multi {
            let p = if q.options.iter().any(|o| o.description.is_some()) {
                MultiSelectPropertySchema::titled(
                    q.options
                        .iter()
                        .map(|o| {
                            let option = EnumOption::new(o.label.clone(), o.label.clone());
                            match &o.description {
                                Some(d) => option.description(d.clone()),
                                None => option,
                            }
                        })
                        .collect(),
                )
            } else {
                MultiSelectPropertySchema::new(q.options.iter().map(|o| o.label.clone()).collect())
            };
            let recommended: Vec<String> = q
                .recommended
                .iter()
                .filter_map(|i| q.options.get(*i as usize))
                .map(|o| o.label.clone())
                .collect();
            let p = p.title(q.question.clone());
            let p = if recommended.is_empty() {
                p
            } else {
                p.default_value(recommended)
            };
            ElicitationPropertySchema::Array(p)
        } else {
            let p = StringPropertySchema::new().title(q.question.clone());
            if q.options.is_empty() {
                ElicitationPropertySchema::String(p)
            } else {
                let p = p.one_of(
                    q.options
                        .iter()
                        .map(|o| {
                            let option = EnumOption::new(o.label.clone(), o.label.clone());
                            match &o.description {
                                Some(d) => option.description(d.clone()),
                                None => option,
                            }
                        })
                        .collect::<Vec<_>>(),
                );
                let p = match q
                    .recommended
                    .first()
                    .and_then(|i| q.options.get(*i as usize))
                {
                    Some(o) => p.default_value(o.label.clone()),
                    None => p,
                };
                ElicitationPropertySchema::String(p)
            }
        };
        schema = schema.property(q.id.clone(), property, true);
    }
    (message, schema)
}

/// elicitation 表单内容 → `AskResult`：单选且值是选项之一 → `selected`；其余进 `custom`。
fn ask_result(
    questions: &[AskQuestion],
    content: Option<&BTreeMap<String, ElicitationContentValue>>,
) -> AskResult {
    AskResult {
        answers: questions
            .iter()
            .map(|q| {
                let value = content.and_then(|c| c.get(&q.id));
                let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
                match value {
                    Some(ElicitationContentValue::StringArray(selected)) => AskAnswer {
                        id: q.id.clone(),
                        selected: selected.clone(),
                        custom: None,
                    },
                    Some(ElicitationContentValue::String(value))
                        if labels.contains(&value.as_str()) =>
                    {
                        AskAnswer {
                            id: q.id.clone(),
                            selected: vec![value.clone()],
                            custom: None,
                        }
                    }
                    Some(value) => AskAnswer {
                        id: q.id.clone(),
                        selected: Vec::new(),
                        custom: Some(match value {
                            ElicitationContentValue::String(s) => s.clone(),
                            ElicitationContentValue::Integer(i) => i.to_string(),
                            ElicitationContentValue::Number(n) => n.to_string(),
                            ElicitationContentValue::Boolean(b) => b.to_string(),
                            ElicitationContentValue::StringArray(v) => v.join(", "),
                            _ => String::new(),
                        }),
                    },
                    None => AskAnswer {
                        id: q.id.clone(),
                        selected: Vec::new(),
                        custom: None,
                    },
                }
            })
            .collect(),
    }
}
