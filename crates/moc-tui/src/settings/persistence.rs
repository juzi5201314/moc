use super::Job;
use super::Store;
use crate::app::App;
use crate::protocol::EmbeddedConnection;
use moc_i18n::t;
use moc_protocol::ConfigReply;
use moc_protocol::SettingsDocument;
use serde_json::json;

pub fn response(
    app: &mut App,
    connection: &EmbeddedConnection,
    id: u64,
    job: Job,
    result: anyhow::Result<ConfigReply>,
) {
    if matches!(job, Job::Save) {
        finish_save(app, connection, result);
        return;
    }
    if !app.settings.as_ref().is_some_and(|panel| panel.id == id) {
        return;
    }
    let mut panel = app.settings.take().expect("回执属于当前设置页");
    if let Job::Models(generation) = job {
        panel.models_response(generation, result);
    } else {
        panel.pending = None;
        panel.busy = false;
        panel.handle_response(app, connection, result);
    }
    app.settings = Some(panel);
}

/// 写入回执：带着新版本先把排队的改动接着写完，再把结果并入面板。
pub fn finish_save(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<ConfigReply>,
) {
    app.config_flight = None;
    match result {
        Ok(ConfigReply::Settings(data)) => {
            let keep_local = match &mut app.config_queue {
                // 写入期间又有改动：只取回新版本号，等下一轮写入。
                Some(write) => {
                    write.revision.clone_from(&data.revision);
                    true
                }
                None => false,
            };
            if keep_local && let Some(write) = app.config_queue.take() {
                app.write_config(connection, write);
            }
            adopt(app, connection, *data, keep_local);
        }
        Err(error) => {
            app.config_queue = None;
            let text = error.to_string();
            match app.settings.as_mut() {
                Some(panel) => {
                    panel.message = None;
                    panel.outcome = None;
                    panel.error = Some(text);
                    if let Some(draft) = &mut panel.providers.draft {
                        draft.pending = None;
                    }
                }
                None => app.set_error_notice(t!("tui.persist.save_failed", err = text)),
            }
        }
        Ok(_) => {}
    }
    reload_if_waiting(app, connection);
}

/// 设置页若在等写入落地后才读取配置，这里补上。
pub(super) fn reload_if_waiting(app: &mut App, connection: &EmbeddedConnection) {
    if app.saving_config() {
        return;
    }
    if let Some(mut panel) = app.settings.take() {
        panel.ensure_loaded(app, connection);
        app.settings = Some(panel);
    }
}

/// 把写入结果并入设置面板，并在配置稳定后刷新模型信息。
pub(super) fn adopt(
    app: &mut App,
    connection: &EmbeddedConnection,
    data: SettingsDocument,
    keep_local: bool,
) {
    let Some(panel) = app.settings.as_mut() else {
        return;
    };
    let local = if keep_local {
        panel
            .data
            .as_ref()
            .map(|current| (current.document.clone(), current.effective.clone()))
    } else {
        None
    };
    panel.data = Some(data);
    if let Some((document, effective)) = local
        && let Some(current) = &mut panel.data
    {
        current.document = document;
        current.effective = effective;
    }
    panel.error = None;
    panel.rebuild();
    if keep_local {
        return;
    }
    panel.provider_saved(connection, &app.events);
    panel.message = Some(
        panel
            .outcome
            .take()
            .unwrap_or_else(|| t!("tui.persist.saved").into_owned()),
    );
    crate::interaction::reload_configuration(app, connection);
}

/// 把共用模型选择器选中的角色值写回配置：与就地编辑同一条提交路径，撤回栈一并记录。
pub fn apply_model_role(
    app: &mut App,
    connection: &EmbeddedConnection,
    path: Vec<String>,
    label: &str,
    value: String,
) {
    let Some(mut panel) = app.settings.take() else {
        return;
    };
    let target = path;
    let _ = panel.commit(label, app, connection, |panel| {
        panel.set_value(&target, json!(value));
        Ok(Store::of(&target))
    });
    app.settings = Some(panel);
}
