//! 子代理名册与会话焦点切换。

use rust_i18n::t;

use super::bootstrap::refresh_git_branch;
use super::context::refresh_context;
use super::usage::refresh_usage;
use crate::app::App;
use crate::history_page::HistoryClient;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use moc_protocol::ClientHandle;
use moc_protocol::SessionLoadPageParams;

/// 刷新代理面：名册与侧栏始终挂靠 [`App::agents_root_id`]（聚焦时是主会话）。
pub(super) fn load_subagents(app: &App, connection: &EmbeddedConnection) {
    let Some(parent_session_id) = app.agents_root_id() else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                session_id: parent_session_id,
            })
            .await
            .map(|result| result.agents)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentsLoaded(result));
    });
}

/// 唤出子代理侧栏：手动覆盖显示（名册曾以居中模态呈现，已收敛为右侧停靠）。
///
/// 顺手向 core 刷新一次名册；迟到的回执照常并入 `app.subagents`，
/// 有代理时侧栏随之现身。名册为空时不强开面板，只提示。
pub(super) fn open_subagents(app: &mut App, connection: &EmbeddedConnection) {
    app.subagent_sidebar = Some(true);
    load_subagents(app, connection);
    if app.subagents.is_empty() {
        app.set_notice(t!("tui.event.no_subagents").into_owned());
    }
}

/// 停止一个子代理：聚焦时的 `Ctrl+S` 用它。
pub(super) fn stop_agent_operation(app: &App, connection: &EmbeddedConnection, agent_id: String) {
    // 停止请求的父会话永远是代理面挂靠的那个：聚焦子代理时 `app.session` 是子代理自己。
    let Some(parent_session_id) = app.agents_root_id() else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentStop>(moc_protocol::methods::AgentStopParams {
                session_id: parent_session_id,
                agent_id: agent_id.clone(),
                reason: None,
            })
            .await
            .map(|result| crate::subagent::AgentOperationNotice {
                target: agent_id,
                message: t!("tui.event.stopped_fmt", n = result.stopped).into_owned(),
            })
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentOperation(result));
    });
}

/// 聚焦进入或返回：按 `/resume` 同路加载目标会话。
///
/// 聚焦期间另一侧的更新被忽略（见 `apply_update_and_refresh` 的代理面分支），
/// 所以返回主会话时同样以磁盘真相重载，而不是自己缓存一份转写。
pub(super) fn load_focus(
    app: &mut App,
    connection: &EmbeddedConnection,
    session_id: String,
    returning: bool,
) {
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .history_page(SessionLoadPageParams {
                full_messages: true,
                plan_page: Some(Default::default()),
                session_id,
                before_entry_id: None,
                max_entries: 128,
                max_message_bytes: Some(64 * 1024),
                redo_page: Some(Default::default()),
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::FocusLoaded { returning, result });
    });
}

/// 聚焦一个子代理：主 TUI 整体切到它的会话。
///
/// 已聚焦时再聚焦另一个代理只换焦点：主会话仍是原来那一个。
pub(super) fn focus_agent(app: &mut App, connection: &EmbeddedConnection, agent_id: String) {
    let Some(view) = app.subagents.get(&agent_id) else {
        return;
    };
    let target = view.info.clone();
    if app.focused.is_none() {
        let Some(home) = app.session.clone() else {
            return;
        };
        if home.id == target.id {
            return;
        }
        app.focused = Some(crate::app::FocusedAgent {
            home,
            home_draft: app.prompt_text().to_owned(),
            home_scroll: app.scroll_offset,
        });
    } else if app
        .session
        .as_ref()
        .is_some_and(|session| session.id == target.id)
    {
        return;
    }
    app.subagent_selected = Some(target.id.clone());
    app.modal = None;
    load_focus(app, connection, target.id, false);
}

/// 返回主会话：还原草稿与滚动位置，并按磁盘真相重载。
pub(super) fn leave_focus(app: &mut App, connection: &EmbeddedConnection) {
    let Some(focused) = app.focused.take() else {
        return;
    };
    app.focus_restore = Some((focused.home_draft, focused.home_scroll));
    load_focus(app, connection, focused.home.id, true);
}

/// 刷新子代理名册；`parent_session_id` 为 `None` 时清空名册。
pub(super) fn load_subagents_for_parent(
    app: &mut App,
    client: &ClientHandle,
    parent_session_id: Option<String>,
) {
    let Some(parent_session_id) = parent_session_id else {
        app.subagents.clear();
        app.sync_subagent_sidebar();
        return;
    };
    let client = client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<moc_protocol::methods::AgentList>(moc_protocol::methods::AgentListParams {
                session_id: parent_session_id,
            })
            .await
            .map(|result| result.agents)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::SubagentsLoaded(result));
    });
}

/// 应用一次聚焦加载结果：`returning` 时还原草稿与滚动位置。
pub(super) fn apply_focus_loaded(
    app: &mut App,
    connection: &EmbeddedConnection,
    returning: bool,
    result: anyhow::Result<moc_protocol::methods::SessionLoadPageResult>,
) {
    let page = match result {
        Ok(page) => page,
        Err(error) => {
            app.focus_restore = None;
            app.focused = None;
            app.set_error_notice(
                t!("tui.event.agent_load_failed_fmt", err = error.to_string()).into_owned(),
            );
            return;
        }
    };
    let blocks = crate::transcript_restore::blocks_from_entries_with_children(
        &page.entries,
        &page.child_calls,
    );
    let has_more = page.has_more;
    let before_entry_id = page.entries.first().map(|entry| entry.id.clone());
    let cwd = page.session.cwd.clone();
    let todos = page.todos;
    app.switch_session(page.session, blocks);
    app.todos = todos;
    app.goal = page.goal;
    if returning && let Some((draft, scroll)) = app.focus_restore.take() {
        if !draft.is_empty() {
            app.prompt.insert_str_raw(&draft);
            app.prompt_changed();
        }
        app.scroll_offset = scroll;
    }
    app.set_history_cursor(has_more, before_entry_id);
    refresh_git_branch(app, &cwd);
    refresh_context(app, connection, false);
    refresh_usage(app, connection, false);
    load_subagents(app, connection);
}
