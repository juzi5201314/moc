//! 将异步管理操作的回执投影到 UI；队列以 RPC 收尾而非 TurnFinished 通知为边界。

use crate::{
    app::App,
    protocol::{AppEvent, EmbeddedConnection},
};

pub fn handle_event(app: &mut App, connection: &EmbeddedConnection, event: AppEvent) {
    match event {
        AppEvent::SettingsResponse {
            panel,
            result,
            saved,
        } => crate::settings::response(app, connection, panel, result, saved),
        AppEvent::ConfigurationLoaded(config) => {
            app.models = config.models;
            if app.session.is_none() && config.default_model.is_some() {
                if app.settings.as_ref().is_some_and(|panel| !panel.dirty()) {
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
                            let _ = events.send(AppEvent::Loaded(session, Vec::new()));
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
        AppEvent::TurnReturned { session, result } => {
            app.inflight_turns.remove(&session);
            let interrupted = app.interrupt_pending.remove(&session);
            if app
                .session
                .as_ref()
                .is_some_and(|current| current.id == session)
            {
                if app.turn_running {
                    app.finish_turn();
                }
                match result {
                    Ok(()) => crate::queue::dispatch(app, connection),
                    Err(_) if interrupted => crate::queue::dispatch(app, connection),
                    Err(error) => {
                        app.set_notice(format!("轮次结束：{error}；待发队列保留，可编辑后重新发送"))
                    }
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
                        app.set_notice("补充消息已接收，将在安全工具边界交给模型");
                    }
                }
                Ok(false) => {
                    // UI 发送与轮次关闭相遇：消息没有进入当前轮次，转为下一条正常请求。
                    app.queues.entry(session).or_default().push_front(prompt);
                    crate::queue::dispatch(app, connection);
                }
                Err(error) => {
                    app.queues.entry(session).or_default().push_front(prompt);
                    app.set_notice(format!("补充消息未送达，已保留在队列中：{error}"));
                }
            }
        }
        _ => unreachable!("只接收配置及投递回执"),
    }
}
