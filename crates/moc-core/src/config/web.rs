//! webui 前端状态：`config_home()/web.toml`。
//!
//! 与 `tui.toml` 同类：界面偏好与用户手动添加的工作区目录是前端自己的文件，
//! 不进入 core 配置合并与模型上下文。写走文件锁 + 版本戳比对 + 原子重命名，
//! 与 `editor::save` 同一套并发约定。

use super::editor::{contents, revision, write_atomic};
use crate::CoreError;
use crate::session::lock::SessionLockGuard;
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
        toml::from_str(&text)
            .map_err(|error| CoreError::Config(format!("web.toml 格式有误：{error}")))?
    };
    Ok(WebStateDocument {
        revision: revision(&text),
        exists: path.exists(),
        state,
    })
}

/// 版本戳不一致说明其他窗口或外部编辑器先动过文件，拒绝覆盖。
pub fn save(home: &Path, params: WebStateSaveParams) -> Result<WebStateDocument, CoreError> {
    std::fs::create_dir_all(home)?;
    let path = home.join(FILE_NAME);
    let _guard = SessionLockGuard::acquire(home.join("web-state.lock"), "web-state")?;
    let text = contents(&path)?;
    if revision(&text) != params.revision {
        return Err(CoreError::Config(
            "web.toml 已被其他窗口修改，请刷新后重试".into(),
        ));
    }
    let text = toml::to_string_pretty(&params.state)
        .map_err(|error| CoreError::Config(format!("web.toml 无法序列化：{error}")))?;
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
    use moc_protocol::{WebSubmitAction, WebTheme};

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
            workspaces: vec!["/repo".into()],
            ..WebState::default()
        };
        let saved = save(root.path(), save_params(&doc.revision, state)).unwrap();
        assert!(saved.exists);

        let reloaded = load(root.path()).unwrap();
        assert_eq!(reloaded.state.theme, WebTheme::Dark);
        assert_eq!(reloaded.state.active_submit, WebSubmitAction::Interrupt);
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
