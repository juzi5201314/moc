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
                            let _ = events.send(AppEvent::Loaded(
                                session,
                                Vec::new(),
                                false,
                                None,
                                Vec::new(),
                                false,
                            ));
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
        AppEvent::PlanRevised(result) => crate::event::apply_plan_revised(app, connection, result),
        AppEvent::PlanDiff(result) => crate::event::apply_plan_diff(app, result),
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
            prompt_id,
            result,
        } => {
            let current = app
                .session
                .as_ref()
                .is_some_and(|current| current.id == session);
            match result {
                Ok(result) if result.accepted => {
                    let Some(core_id) = result.id else {
                        // 旧核心没有 SteeringApplied 与撤回 ID：退回一次乐观显示，
                        // 避免消息永久留在待发送区并挡住后续派发。
                        if let Some(pending) = crate::queue::take_steer(app, &session, prompt_id) {
                            if current {
                                let body = if pending.display_text.is_empty() {
                                    "[图片]".to_owned()
                                } else {
                                    pending.display_text
                                };
                                app.push_user(body);
                            }
                            if pending.withdrawal.is_some() {
                                app.set_notice("内核未返回撤回 ID，消息已加入且无法撤回");
                            }
                        }
                        crate::queue::dispatch(app, connection);
                        return;
                    };
                    // 事件可能先于 RPC 响应到达：已经写过转写，只清本地账。
                    if app
                        .resolved_steers
                        .get(&session)
                        .is_some_and(|seen| seen.contains(&core_id))
                    {
                        let withdrawn = crate::queue::take_steer(app, &session, prompt_id)
                            .is_some_and(|pending| pending.withdrawal.is_some());
                        if withdrawn {
                            app.set_notice("消息已进入本轮上下文，无法撤回");
                        }
                        crate::queue::dispatch(app, connection);
                        return;
                    }
                    let withdrawal = match crate::queue::steer_mut(app, &session, prompt_id) {
                        Some(pending) => {
                            pending.core_id = Some(core_id);
                            pending.withdrawal
                        }
                        None => return,
                    };
                    if let Some(action) = withdrawal {
                        crate::queue::request_unsteer(
                            app, connection, session, prompt_id, core_id, action,
                        );
                    }
                }
                Ok(_) => {
                    let Some(pending) = crate::queue::take_steer(app, &session, prompt_id) else {
                        return;
                    };
                    let prompt = crate::queue::QueuedPrompt {
                        id: pending.id,
                        draft: pending.draft,
                        mode: None,
                    };
                    if let Some(action) = pending.withdrawal {
                        if action == crate::queue::WithdrawAction::Edit {
                            if current {
                                crate::queue::restore_or_requeue(app, &session, prompt);
                            } else {
                                app.queues.entry(session).or_default().push_front(prompt);
                            }
                            app.set_notice("补充消息未送达，草稿已恢复");
                        }
                    } else {
                        // UI 发送与提示关闭相遇：消息没有进入当前提示，转为下一条正常请求。
                        app.queues.entry(session).or_default().push_front(prompt);
                        app.set_notice("补充消息未进入当前提示，已排到待发送");
                    }
                    crate::queue::dispatch(app, connection);
                }
                Err(error) => {
                    let Some(pending) = crate::queue::take_steer(app, &session, prompt_id) else {
                        return;
                    };
                    let prompt = crate::queue::QueuedPrompt {
                        id: pending.id,
                        draft: pending.draft,
                        mode: None,
                    };
                    if let Some(action) = pending.withdrawal {
                        if action == crate::queue::WithdrawAction::Edit {
                            if current {
                                crate::queue::restore_or_requeue(app, &session, prompt);
                            } else {
                                app.queues.entry(session).or_default().push_front(prompt);
                            }
                            app.set_notice(format!("撤回完成：消息未送达（{error}）"));
                        }
                    } else {
                        app.queues.entry(session).or_default().push_front(prompt);
                        app.set_notice(format!("未能送达，已排到待发送：{error}"));
                    }
                    crate::queue::dispatch(app, connection);
                }
            }
        }
        AppEvent::UnsteerReturned {
            session,
            prompt_id,
            action,
            result,
        } => {
            let current = app
                .session
                .as_ref()
                .is_some_and(|current| current.id == session);
            match result {
                Ok(true) => {
                    let Some(pending) = crate::queue::take_steer(app, &session, prompt_id) else {
                        return;
                    };
                    if action == crate::queue::WithdrawAction::Edit {
                        let prompt = crate::queue::QueuedPrompt {
                            id: pending.id,
                            draft: pending.draft,
                            mode: None,
                        };
                        if current {
                            crate::queue::restore_or_requeue(app, &session, prompt);
                        } else {
                            app.queues.entry(session).or_default().push_front(prompt);
                        }
                    }
                    app.set_notice("已撤回待发送的补充消息");
                    crate::queue::dispatch(app, connection);
                }
                Ok(false) => {
                    // steering 已被 drain：等 SteeringApplied 到达后由它移入转写。
                    app.set_notice("消息已进入本轮上下文，无法撤回");
                }
                Err(error) => {
                    if let Some(pending) = crate::queue::steer_mut(app, &session, prompt_id) {
                        pending.withdrawal = None;
                    }
                    app.set_notice(format!("撤回失败：{error}"));
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
