//! 终端日志、初始会话与环境加载。

use moc_i18n::t;

use super::TuiOptions;
use super::agents::load_subagents_for_parent;
use crate::app::App;
use crate::history_page::HistoryClient;
use crate::protocol::AppEvent;
use anyhow::Context;
use moc_protocol::ClientHandle;
use moc_protocol::ConfigGet;
use moc_protocol::ConfigGetParams;
use moc_protocol::SessionContext;
use moc_protocol::SessionInfo;
use moc_protocol::SessionList;
use moc_protocol::SessionListParams;
use moc_protocol::SessionLoad;
use moc_protocol::SessionLoadPageParams;
use moc_protocol::SessionLoadParams;
use moc_protocol::SessionLoadResult;
use moc_protocol::SessionNew;
use moc_protocol::SessionNewParams;

/// TUI 独占 stdout / stderr，日志只能落文件；初始化失败时静默（日志非关键路径）。
pub(super) fn init_logging() {
    const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
    let home = std::env::var("MOC_HOME")
        .or_else(|_| std::env::var("HOME").map(|home| format!("{home}/.moc")))
        .unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&home).join("tui.log");
    if std::fs::metadata(&path)
        .ok()
        .is_some_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
    {
        let _ = std::fs::remove_file(&path);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Arc::new(file))
        .with_ansi(false)
        .with_target(true)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
    // alt screen 下 panic 消息不可见：同时落到日志
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "TUI panic");
        previous(info);
    }));
}

/// `bootstrap` 成功时恢复的会话与转写块；`bool` 是 `has_more`，末项是首条历史 ID。
pub(super) type BootstrapLoad = (
    SessionInfo,
    Vec<crate::transcript::Entry>,
    bool,
    Option<String>,
);

/// 拉取模型目录并建立/恢复初始会话；失败给出可见错误但继续运行（用户可退出）。
pub(super) async fn bootstrap(client: &ClientHandle, app: &mut App, opts: &TuiOptions) {
    let outcome: anyhow::Result<Option<BootstrapLoad>> = async {
        let config = client.call::<ConfigGet>(ConfigGetParams {}).await?;
        // core 的 cwd 已规范化，会话 header 里存的也是同一形式；用它比较才不受
        // `--cwd` 的相对路径或符号链接影响。
        app.startup_cwd = config.cwd.clone();
        app.models = config.models;
        app.recent_models =
            crate::model_history::RecentModels::from_selectors(&config.recent_models);
        app.fusion_defaults = config.fusion.clone();
        app.features = config.features;
        if opts.resume.is_none() && config.default_model.is_none() && opts.model.is_none() {
            app.set_notice(t!("tui.event.pick_model_once").into_owned());
            if !app.models.is_empty() {
                app.open_model_picker();
            }
            return Ok(None);
        }
        let loaded = match &opts.resume {
            None => {
                let info = client
                    .call::<SessionNew>(SessionNewParams {
                        cwd: opts.cwd.clone(),
                        model: opts.model.clone(),
                        title: None,
                        engine: opts.engine,
                        plan_active: opts.plan,
                        fast_mode: false,
                    })
                    .await?;
                (info, Vec::new(), false, None)
            }
            Some(session_id) => {
                let session_id = match session_id {
                    Some(id) => id.clone(),
                    None => {
                        let list = client.call::<SessionList>(SessionListParams {}).await?;
                        list.sessions
                            .into_iter()
                            .find(|session| session.cwd == config.cwd)
                            .map(|session| session.id)
                            .context(t!("tui.event.no_restorable"))?
                    }
                };
                let loaded = client
                    .history_page(SessionLoadPageParams {
                        full_messages: true,
                        plan_page: Some(Default::default()),
                        session_id,
                        before_entry_id: None,
                        max_entries: 64,
                        max_message_bytes: Some(64 * 1024),
                        redo_page: Some(Default::default()),
                    })
                    .await?;
                let before_entry_id = loaded.entries.first().map(|entry| entry.id.clone());
                let blocks = crate::transcript_restore::blocks_from_entries_with_children(
                    &loaded.entries,
                    &loaded.child_calls,
                );
                (loaded.session, blocks, loaded.has_more, before_entry_id)
            }
        };
        Ok(Some(loaded))
    }
    .await;
    match outcome {
        Ok(Some((info, blocks, _has_more, _before_entry_id))) => {
            refresh_git_branch(app, &info.cwd);
            app.switch_session(info, blocks);
            load_context(client, app).await;
            load_subagents_for_parent(
                app,
                client,
                app.session.as_ref().map(|session| session.id.clone()),
            );
        }
        Ok(None) => {}
        Err(error) => app.set_error_notice(
            t!("tui.event.boot_failed_fmt", err = error.to_string()).into_owned(),
        ),
    }
}

/// 显示会话只取一页正文；租约请求不携带可能超帧的消息。
pub(super) async fn load_display_session(
    client: &ClientHandle,
    session_id: String,
    read_only: bool,
) -> Result<SessionLoadResult, moc_protocol::RpcCallError> {
    client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: true,
            session_id: session_id.clone(),
            max_entries: Some(0),
            read_only,
        })
        .await?;
    load_display_page(client, session_id).await
}

pub(super) async fn load_display_page(
    client: &ClientHandle,
    session_id: String,
) -> Result<SessionLoadResult, moc_protocol::RpcCallError> {
    use moc_protocol::methods::session::PlanPageParams;
    let mut page = client
        .history_page(SessionLoadPageParams {
            full_messages: true,
            plan_page: Some(PlanPageParams {
                before: None,
                max_entries: 32,
            }),
            session_id: session_id.clone(),
            before_entry_id: None,
            max_entries: 64,
            max_message_bytes: Some(64 * 1024),
            redo_page: Some(Default::default()),
        })
        .await?;
    let mut before = page.plans_before;
    let mut plan_pages = vec![std::mem::take(&mut page.plans)];
    while let Some(cursor) = before {
        let older = client
            .history_page(SessionLoadPageParams {
                full_messages: true,
                session_id: session_id.clone(),
                before_entry_id: None,
                max_entries: 0,
                max_message_bytes: None,
                redo_page: Some(Default::default()),
                plan_page: Some(PlanPageParams {
                    before: Some(cursor),
                    max_entries: 32,
                }),
            })
            .await?;
        before = older.plans_before;
        plan_pages.push(older.plans);
    }
    Ok(SessionLoadResult {
        session: page.session,
        entries: page.entries,
        truncated: page.has_more,
        todos: page.todos,
        plans: plan_pages.into_iter().rev().flatten().collect(),
        child_calls: page.child_calls,
        goal: page.goal,
    })
}

pub(super) async fn load_mutation_page(
    client: &ClientHandle,
    result: Result<SessionLoadResult, moc_protocol::RpcCallError>,
) -> anyhow::Result<SessionLoadResult> {
    let loaded = result?;
    Ok(load_display_page(client, loaded.session.id).await?)
}

pub(super) async fn load_context(client: &ClientHandle, app: &mut App) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if let Ok(usage) = client
        .call::<SessionContext>(moc_protocol::SessionContextParams { session_id })
        .await
    {
        app.context_usage = Some(usage);
    }
}

/// 异步刷新 git 分支（冷路径：启动与工作区变化时）。
pub(super) fn refresh_git_branch(app: &App, cwd: &str) {
    let events = app.events.clone();
    let cwd = cwd.to_owned();
    tokio::spawn(async move {
        let Ok(output) = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&cwd)
            .output()
            .await
        else {
            return;
        };
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !branch.is_empty() {
                let _ = events.send(AppEvent::GitBranch(branch));
            }
        }
    });
}
