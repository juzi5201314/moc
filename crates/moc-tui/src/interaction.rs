//! 将异步管理操作的回执投影到 UI：配置、投递与模型切换回执；队列以 RPC 收尾而非
//! PromptFinished 通知为边界。

use crate::{
    app::App,
    modal::Modal,
    protocol::{AppEvent, EmbeddedConnection},
};

pub fn handle_event(app: &mut App, connection: &EmbeddedConnection, event: AppEvent) {
    match event {
        AppEvent::SettingsResponse { panel, job, result } => {
            crate::settings::response(app, connection, panel, job, result)
        }
        AppEvent::ConfigurationLoaded(config) => {
            app.models = config.models;
            if app.session.is_none() && config.default_model.is_some() {
                if app.settings.is_some() && !app.saving_config() {
                    app.settings = None;
                }
                let client = connection.client.clone();
                let events = app.events.clone();
                let cwd = app.startup_cwd.clone();
                let mode = app.startup_mode;
                tokio::spawn(async move {
                    match client
                        .call::<moc_protocol::SessionNew>(moc_protocol::SessionNewParams {
                            cwd: Some(cwd),
                            model: None,
                            title: None,
                            mode,
                        })
                        .await
                    {
                        Ok(session) => {
                            let _ = events.send(AppEvent::Loaded(session, Vec::new(), false, None));
                        }
                        Err(error) => {
                            let _ = events.send(AppEvent::Notice(format!(
                                "模型已保存，但会话创建失败：{error}"
                            )));
                        }
                    }
                });
            }
        }
        AppEvent::PlanApproval(result) => {
            crate::event::apply_plan_approval(app, connection, result)
        }
        AppEvent::PlanExecutionLoaded {
            result,
            auto_prompt,
        } => crate::event::apply_plan_execution_loaded(app, connection, result, auto_prompt),
        AppEvent::PromptReturned { session, result } => {
            app.inflight_prompts.remove(&session);
            let interrupted = app.interrupt_pending.remove(&session);
            if app
                .session
                .as_ref()
                .is_some_and(|current| current.id == session)
            {
                if app.prompt_running {
                    app.finish_prompt();
                }
                match result {
                    Ok(()) => crate::queue::dispatch(app, connection),
                    Err(_) if interrupted => crate::queue::dispatch(app, connection),
                    Err(error) => app.set_notice(format!(
                        "未能完成：{error}；已排队的消息仍保留，可编辑后重发"
                    )),
                }
            }
        }
        AppEvent::SteerReturned {
            session,
            prompt,
            result,
        } => {
            match result {
                Ok(true) => {
                    if app
                        .session
                        .as_ref()
                        .is_some_and(|current| current.id == session)
                    {
                        app.push_user(prompt.draft.text);
                        app.set_notice("已收到补充要求，会在安全时机交给模型");
                    }
                }
                Ok(false) => {
                    // UI 发送与提示关闭相遇：消息没有进入当前提示，转为下一条正常请求。
                    app.queues.entry(session).or_default().push_front(prompt);
                    crate::queue::dispatch(app, connection);
                }
                Err(error) => {
                    app.queues.entry(session).or_default().push_front(prompt);
                    app.set_notice(format!("未能送达，已排到待发送：{error}"));
                }
            }
        }
        AppEvent::ModelSwitchConfirm {
            session_id,
            selector,
            overflow,
        } => {
            // 选择器那次调用已收尾；随后由卡片自身挡住派发，无需保留忙标记。
            app.inflight_session_ops.remove(&session_id);
            if matches!(app.modal, Some(Modal::Model { .. })) {
                app.modal = Some(Modal::SwitchModel {
                    session_id,
                    selector,
                    overflow,
                    option: 0,
                    loading: false,
                    error: None,
                });
            }
        }
        AppEvent::SessionControlSettled {
            session_id,
            notice,
            result,
        } => {
            app.inflight_session_ops.remove(&session_id);
            match result {
                Ok(compaction_error) => {
                    // 只关闭这次操作打开的选择器与确认卡片，不碰用户后来打开的模态。
                    if matches!(
                        app.modal,
                        Some(Modal::Model { .. } | Modal::SwitchModel { .. })
                    ) {
                        app.modal = None;
                    }
                    if let Some(error) = compaction_error {
                        app.push_note(format!("上下文压缩失败：{error}"), true);
                    }
                    if let Some(notice) = notice {
                        app.set_notice(notice);
                    }
                }
                Err(error) => {
                    let shown = app
                        .modal
                        .as_mut()
                        .is_some_and(|modal| modal.set_error(&error));
                    if !shown {
                        app.set_notice(error);
                    }
                }
            }
            crate::queue::dispatch(app, connection);
        }
        _ => unreachable!("只接收配置、投递与模型切换回执"),
    }
}
