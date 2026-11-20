//! 模型选择与思考等级切换。

use rust_i18n::t;

use crate::app::App;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use moc_protocol::SessionModelOverflowAction;
use moc_protocol::SessionNew;
use moc_protocol::SessionNewParams;
use moc_protocol::SessionSetFastMode;
use moc_protocol::SessionSetFastModeParams;
use moc_protocol::SessionSetModel;
use moc_protocol::SessionSetModelParams;

fn refuse_read_only(app: &mut App) -> bool {
    if !app.read_only {
        return false;
    }
    if let Some(crate::modal::Modal::Model { picker, detail, .. }) = app.modal.as_mut() {
        picker.loading = false;
        picker.error = Some(t!("tui.status.read_only").into_owned());
        detail.pending = None;
    }
    app.set_notice(t!("tui.queue.read_only"));
    true
}

/// Fusion 组合的应用：两侧已齐时，规范组合串等于 `current` 就关闭选择器（返回 `true`），
/// 否则置 loading 并延后发起 `session/set_model`。调用方先处理两侧有空的情况。
pub(super) fn apply_fusion_pick(
    picker: &mut crate::modal::PickerState,
    fusion: &crate::modal::FusionDraft,
    current: &str,
    session_id: &Option<String>,
    session_apply: &mut Option<(Option<String>, String)>,
) -> bool {
    let selector = crate::modal::fusion_selector(&fusion.lead, &fusion.sidekick);
    if selector == current {
        true
    } else {
        picker.loading = true;
        picker.error = None;
        *session_apply = Some((session_id.clone(), selector));
        false
    }
}

/// `session/set_model` 的异步发起：普通模型与 Fusion 组合的应用共用；
/// 需要压缩时照旧走 `ModelSwitchConfirm` 确认流。
pub(super) fn request_session_model(
    app: &mut App,
    connection: &EmbeddedConnection,
    session_id: Option<String>,
    selector: String,
) {
    if refuse_read_only(app) {
        return;
    }
    let Some(session_id) = session_id else {
        let client = connection.client.clone();
        let events = app.events.clone();
        let cwd = app.startup_cwd.clone();
        let engine = app.startup_engine;
        let plan_active = app.startup_plan;
        tokio::spawn(async move {
            match client
                .call::<SessionNew>(SessionNewParams {
                    cwd: Some(cwd),
                    model: Some(selector),
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
                        t!("tui.event.create_failed_fmt", err = error.to_string()).into_owned(),
                    ));
                }
            }
        });
        app.set_modal(None);
        return;
    };
    app.inflight_session_ops.insert(session_id.clone());
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let outcome = client
            .call::<SessionSetModel>(SessionSetModelParams {
                session_id: session_id.clone(),
                selector: selector.clone(),
                overflow: SessionModelOverflowAction::Ask,
            })
            .await;
        let _ = events.send(match outcome {
            Ok(result) => match result.overflow {
                Some(overflow) => AppEvent::ModelSwitchConfirm {
                    session_id,
                    selector,
                    overflow,
                },
                None => AppEvent::SessionControlSettled {
                    session_id,
                    notice: None,
                    result: Ok(result.compaction_error),
                },
            },
            Err(error) => AppEvent::SessionControlSettled {
                session_id,
                notice: None,
                result: Err(
                    t!("tui.event.model_switch_failed_fmt", err = error.to_string()).into_owned(),
                ),
            },
        });
    });
}

/// 按当前模型声明的思考等级循环切换。
pub(super) fn cycle_thinking_level(app: &mut App, connection: &EmbeddedConnection) {
    if refuse_read_only(app) {
        return;
    }
    let Some(session) = app.session.as_ref() else {
        app.set_notice(t!("tui.common.no_session").into_owned());
        return;
    };
    // Fusion 会话切换的是 lead 侧的思考等级，sidekick 侧原样保留。
    let (base, current, sidekick) = match crate::modal::parse_fusion(&session.model) {
        Some((lead, sidekick)) => {
            let (base, level) = lead.rsplit_once(':').unwrap_or((lead, "off"));
            (base.to_owned(), level.to_owned(), Some(sidekick.to_owned()))
        }
        None => {
            let (base, level) = session
                .model
                .rsplit_once(':')
                .unwrap_or((session.model.as_str(), "off"));
            (base.to_owned(), level.to_owned(), None)
        }
    };
    let levels = app
        .models
        .iter()
        .find(|model| model.id == base)
        .and_then(|model| model.thinking_levels.clone())
        .unwrap_or_else(|| {
            ["off", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        });
    let index = levels
        .iter()
        .position(|level| level == &current)
        .unwrap_or(0);
    let next = levels[(index + 1) % levels.len()].clone();
    let selector = match &sidekick {
        Some(sidekick) => crate::modal::fusion_selector(&format!("{base}:{next}"), sidekick),
        None => format!("{base}:{next}"),
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    app.inflight_session_ops.insert(session_id.clone());
    tokio::spawn(async move {
        let outcome = client
            .call::<SessionSetModel>(SessionSetModelParams {
                session_id: session_id.clone(),
                selector,
                overflow: SessionModelOverflowAction::Switch,
            })
            .await;
        let _ = events.send(match outcome {
            Ok(_) => AppEvent::SessionControlSettled {
                session_id,
                notice: Some(t!("tui.event.thinking_level_fmt", level = next).into_owned()),
                result: Ok(None),
            },
            Err(error) => AppEvent::SessionControlSettled {
                session_id,
                notice: None,
                result: Err(
                    t!("tui.event.thinking_failed_fmt", err = error.to_string()).into_owned()
                ),
            },
        });
    });
}

/// 请求 core 切换会话 Fast Mode；生效后由 `SessionUpdate::FastModeChanged` 写回。
pub(super) fn set_fast_mode(app: &mut App, connection: &EmbeddedConnection, enabled: bool) {
    if refuse_read_only(app) {
        return;
    }
    let Some(session) = app.session.as_ref() else {
        app.set_notice(t!("tui.common.no_session").into_owned());
        return;
    };
    let client = connection.client.clone();
    let session_id = session.id.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        if let Err(error) = client
            .call::<SessionSetFastMode>(SessionSetFastModeParams {
                session_id,
                enabled,
            })
            .await
        {
            let _ = events.send(AppEvent::Notice(
                t!("tui.event.fast_mode_failed_fmt", err = error.to_string()).into_owned(),
            ));
        }
    });
}
