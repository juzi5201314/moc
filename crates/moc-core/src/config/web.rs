//! webui 前端状态：`config_home()/web.toml`。
//!
//! 与 `tui.toml` 同类：界面偏好与用户手动添加的工作区目录是前端自己的文件，
//! 不进入 core 项目配置合并；其中 `widgets` 同时决定顶层 webui 会话是否向模型
//! 暴露交互组件能力与 `tool://widget` 文档。写走文件锁 + 版本戳比对 + 原子重命名，
//! 与 `editor::save` 同一套并发约定。

use super::editor::{contents, revision, write_atomic};
use crate::CoreError;
use crate::session::lock::SessionLockGuard;
use moc_i18n::t;
use moc_protocol::{WebState, WebStateDocument, WebStateSaveParams};
use std::path::Path;

const FILE_NAME: &str = "web.toml";

/// 读取状态文档；文件缺失按默认状态处理，`exists` 让前端能做一次旧数据迁移。
pub fn load(home: &Path) -> Result<WebStateDocument, CoreError> {
    let path = home.join(FILE_NAME);
    let text = contents(&path)?;
    let state: WebState = if text.trim().is_empty() {
        WebState::default()
    } else {
        parse(&text).map_err(|error| {
            CoreError::Config(t!("core.err.web_toml_invalid", error = error).into_owned())
        })?
    };
    Ok(WebStateDocument {
        revision: revision(&text),
        exists: path.exists(),
        state,
    })
}

/// 解析状态文件：
/// - 旧版的 `expand_thinking = bool` 换算成 `thinking_detail`（展开 → `full`，折叠 → `brief`）；
/// - 已移除的 `diff_mode` 直接丢弃（界面固定为统一 diff）；
/// 下次保存时自然写成新格式。
fn parse(text: &str) -> Result<WebState, toml::de::Error> {
    let mut table: toml::Table = toml::from_str(text)?;
    if let Some(toml::Value::Boolean(expand)) = table.remove("expand_thinking") {
        let level = if expand { "full" } else { "brief" };
        table
            .entry("thinking_detail")
            .or_insert_with(|| level.into());
    }
    table.remove("diff_mode");
    toml::Value::Table(table).try_into()
}

/// 版本戳不一致说明其他窗口或外部编辑器先动过文件，拒绝覆盖。
pub fn save(home: &Path, params: WebStateSaveParams) -> Result<WebStateDocument, CoreError> {
    std::fs::create_dir_all(home)?;
    let path = home.join(FILE_NAME);
    let _guard = SessionLockGuard::acquire(home.join("web-state.lock"), "web-state")?;
    let text = contents(&path)?;
    if revision(&text) != params.revision {
        return Err(CoreError::Config(
            t!("core.err.web_toml_conflict").into_owned(),
        ));
    }
    let text = toml::to_string_pretty(&params.state).map_err(|error| {
        CoreError::Config(t!("core.err.web_toml_serialize", error = error).into_owned())
    })?;
    write_atomic(&path, &text)?;
    Ok(WebStateDocument {
        revision: revision(&text),
        exists: true,
        state: params.state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::{SessionPermission, WebDetailLevel, WebSubmitAction, WebTheme};

    fn save_params(revision: &str, state: WebState) -> WebStateSaveParams {
        WebStateSaveParams {
            revision: revision.to_owned(),
            state,
        }
    }

    #[test]
    fn missing_file_loads_defaults_and_reports_absent() {
        let root = tempfile::tempdir().unwrap();
        let doc = load(root.path()).unwrap();
        assert!(!doc.exists);
        assert_eq!(doc.state, WebState::default());
    }

    #[test]
    fn legacy_fields_migrate_or_ignore_cleanly() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("web.toml");
        std::fs::write(
            &path,
            "theme = \"dark\"\nexpand_thinking = true\ndiff_mode = \"split\"\n",
        )
        .unwrap();
        let state = load(root.path()).unwrap().state;
        assert_eq!(state.theme, WebTheme::Dark);
        assert_eq!(state.permission, SessionPermission::Full);
        assert_eq!(state.thinking_detail, WebDetailLevel::Full);
        assert_eq!(state.tool_detail, WebDetailLevel::Brief);

        std::fs::write(&path, "expand_thinking = false\n").unwrap();
        assert_eq!(
            load(root.path()).unwrap().state.thinking_detail,
            WebDetailLevel::Brief
        );
    }

    #[test]
    fn preferences_persist_without_touching_core_configuration() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "# core 不应被前端保存覆盖\n",
        )
        .unwrap();

        let doc = load(root.path()).unwrap();
        let state = WebState {
            theme: WebTheme::Dark,
            active_submit: WebSubmitAction::Interrupt,
            permission: SessionPermission::Workspace,
            workspaces: vec!["/repo".into()],
            ..WebState::default()
        };
        let saved = save(root.path(), save_params(&doc.revision, state)).unwrap();
        assert!(saved.exists);

        let reloaded = load(root.path()).unwrap();
        assert_eq!(reloaded.state.theme, WebTheme::Dark);
        assert_eq!(reloaded.state.active_submit, WebSubmitAction::Interrupt);
        assert_eq!(reloaded.state.permission, SessionPermission::Workspace);
        assert_eq!(reloaded.state.workspaces, ["/repo".to_owned()]);
        assert_eq!(
            std::fs::read_to_string(root.path().join("config.toml")).unwrap(),
            "# core 不应被前端保存覆盖\n"
        );
    }

    #[test]
    fn stale_revision_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let doc = load(root.path()).unwrap();
        save(root.path(), save_params(&doc.revision, WebState::default())).unwrap();
        // 拿旧 revision 再写：模拟另一个窗口先动过 web.toml。
        assert!(save(root.path(), save_params(&doc.revision, WebState::default())).is_err());
        // 重新读取后用最新 revision 可以写。
        let fresh = load(root.path()).unwrap();
        assert!(
            save(
                root.path(),
                save_params(&fresh.revision, WebState::default())
            )
            .is_ok()
        );
    }
}
