//! 将异步管理操作的回执投影到 UI：配置、投递与模型切换回执；队列以 RPC 收尾而非
//! PromptFinished 通知为边界。

use moc_i18n::t;

use crate::{
    app::App,
    modal::Modal,
    protocol::{AppEvent, EmbeddedConnection},
};

/// 后台重读 `config/get`，结果经 `ConfigurationLoaded` 投回（模型目录、最近使用、Fusion 默认组合）。
pub fn reload_configuration(app: &App, connection: &EmbeddedConnection) {
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<moc_protocol::ConfigGet>(moc_protocol::ConfigGetParams {})
            .await
        {
            Ok(config) => {
                let _ = events.send(AppEvent::ConfigurationLoaded(config));
            }
            Err(error) => {
                let _ = events.send(AppEvent::Notice(
                    t!("tui.persist.models_refresh_failed", err = error.to_string()).into_owned(),
                ));
            }
        }
    });
}

pub fn handle_event(app: &mut App, connection: &EmbeddedConnection, event: AppEvent) {
    match event {
        AppEvent::SettingsResponse { panel, job, result } => {
            crate::settings::response(app, connection, panel, job, result)
        }
        AppEvent::ConfigurationLoaded(config) => {
            app.models = config.models;
            app.recent_models =
                crate::model_history::RecentModels::from_selectors(&config.recent_models);
            app.fusion_defaults = config.fusion;
            if app.session.is_none() && config.default_model.is_some() {
                if app.settings.is_some() && !app.saving_config() {
                    app.settings = None;
                }
                let client = connection.client.clone();
                let events = app.events.clone();
                let cwd = app.startup_cwd.clone();
                let engine = app.startup_engine;
                let plan_active = app.startup_plan;
                tokio::spawn(async move {
                    match client
                        .call::<moc_protocol::SessionNew>(moc_protocol::SessionNewParams {
                            cwd: Some(cwd),
                            model: None,
                            title: None,
                            engine,
                            plan_active,
                            fast_mode: false,
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
                                None,
                            ));
                        }
                        Err(error) => {
                            let _ = events.send(AppEvent::Notice(
                                t!("tui.steer.model_saved_failed_fmt", err = error.to_string())
                                    .into_owned(),
                            ));
                        }
                    }
                });
            } else if app.session.is_none() && !app.models.is_empty() && !app.saving_config() {
                app.settings = None;
                app.open_model_picker();
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
            source_session_id,
        } => crate::event::apply_plan_execution_loaded(
            app,
            connection,
            result,
            auto_prompt,
            source_session_id,
        ),
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
                    Err(error) => app.set_error_notice(
                        t!("tui.steer.deliver_failed_fmt", err = error.to_string()).into_owned(),
                    ),
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
                                    t!("tui.steer.image_placeholder").into_owned()
                                } else {
                                    pending.display_text
                                };
                                app.push_user(body);
                            }
                            if pending.withdrawal.is_some() {
                                app.set_notice(t!("tui.steer.no_withdraw_id").into_owned());
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
                            app.set_notice(t!("tui.steer.already_in_context").into_owned());
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
                            app.set_error_notice(t!("tui.steer.undelivered_draft").into_owned());
                        }
                    } else {
                        // UI 发送与提示关闭相遇：消息没有进入当前提示，转为下一条正常请求。
                        app.queues.entry(session).or_default().push_front(prompt);
                        app.set_notice(t!("tui.steer.queued_instead").into_owned());
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
                            app.set_error_notice(
                                t!(
                                    "tui.steer.withdrawn_undelivered_fmt",
                                    err = error.to_string()
                                )
                                .into_owned(),
                            );
                        }
                    } else {
                        app.queues.entry(session).or_default().push_front(prompt);
                        app.set_error_notice(
                            t!("tui.steer.requeued_fmt", err = error.to_string()).into_owned(),
                        );
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
                    app.set_notice(t!("tui.steer.withdrawn").into_owned());
                    crate::queue::dispatch(app, connection);
                }
                Ok(false) => {
                    // steering 已被 drain：等 SteeringApplied 到达后由它移入转写。
                    app.set_notice(t!("tui.steer.already_in_context").into_owned());
                }
                Err(error) => {
                    if let Some(pending) = crate::queue::steer_mut(app, &session, prompt_id) {
                        pending.withdrawal = None;
                    }
                    app.set_error_notice(
                        t!("tui.steer.withdraw_failed_fmt", err = error.to_string()).into_owned(),
                    );
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
            // 切换由模型选择器发起：只把它换成确认卡，不碰其他模态。
            if matches!(app.modal, Some(Modal::Model { .. })) {
                app.set_modal(Some(Modal::SwitchModel {
                    session_id,
                    selector,
                    overflow,
                    option: 0,
                    loading: false,
                    error: None,
                }));
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
                    // 选择器内联切换（改等级立即生效）：回执只写回 current，
                    // 选择器保持打开；其余发起方照旧关闭。
                    let mut inline = false;
                    if let Some(Modal::Model {
                        current,
                        picker,
                        detail,
                        ..
                    }) = app.modal.as_mut()
                        && let Some(pending) = detail.pending.take()
                    {
                        *current = pending;
                        picker.loading = false;
                        inline = true;
                    }
                    if !inline
                        && matches!(
                            app.modal,
                            Some(Modal::Model { .. } | Modal::SwitchModel { .. })
                        )
                    {
                        // 只关闭这次操作打开的选择器与确认卡片，不碰用户后来打开的模态。
                        app.set_modal(None);
                    }
                    if let Some(error) = compaction_error {
                        app.push_note(
                            t!("tui.steer.compact_failed_fmt", err = error.as_str()).into_owned(),
                            crate::transcript::NoteTone::Error,
                        );
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
                    if let Some(Modal::Model { detail, .. }) = app.modal.as_mut() {
                        // 内联切换失败：清掉待写回的目标 selector。
                        detail.pending = None;
                    }
                    if !shown {
                        app.set_error_notice(error);
                    }
                }
            }
            crate::queue::dispatch(app, connection);
        }
        _ => unreachable!("只接收配置、投递与模型切换回执"),
    }
    // 模型选择/切换确认等回执可能刚把模态关掉：此时还原停放的计划审阅。
    crate::event::restore_parked_plan(app);
}
