//! webui 前端状态：界面偏好与用户手动添加的工作区目录。
//!
//! 状态保存在 core 的 `config_home()/web.toml`，与 `tui.toml` 同类：是前端
//! 自己的文件，不进入 core 配置合并与模型上下文。多浏览器经 `web/state` /
//! `web/state_save` 读写同一份文件，写以 `revision` 乐观锁互斥。
//!
//! 布局、标签页这类设备态仍在浏览器侧，不走这两个方法。
//!
//! `web/state_save` 成功后 core 向其余读过 `web/state` 的连接广播
//! `web/state_updated`，负载就是新文档——收到即采用，无需回读。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method, ServerNotification};

/// `web/state`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebStateGet;

impl Method for WebStateGet {
    const NAME: &'static str = "web/state";
    type Params = WebStateGetParams;
    type Result = WebStateDocument;
}

impl ClientRequest for WebStateGet {}

/// `web/state` 的参数：空对象。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebStateGetParams {}

/// `web/state_save`：整文档写回。保存其他窗口同时改动的字段，请先重读再提交。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebStateSave;

impl Method for WebStateSave {
    const NAME: &'static str = "web/state_save";
    type Params = WebStateSaveParams;
    type Result = WebStateDocument;
}

impl ClientRequest for WebStateSave {}

/// `web/state_save` 的参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebStateSaveParams {
    /// 最近一次 `web/state` 返回的 `revision`；不一致时 core 拒绝写入。
    pub revision: String,
    pub state: WebState,
}

/// `web.toml` 的内容：字段名与 `tui.toml` 的对应项保持同一套词汇。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebState {
    /// 界面主题；`system` 跟随系统明暗。
    pub theme: WebTheme,
    /// 会话运行中按 Enter：`steering` 并入当前提示，`interrupt` 先停下再提问。
    pub active_submit: WebSubmitAction,
    /// 后台中弹出需要处理的授权/提问时，发一条系统通知。
    pub notify: bool,
    /// 内联 diff 的展示方式。
    pub diff_mode: WebDiffMode,
    /// 思考过程默认展开而不是折叠。
    pub expand_thinking: bool,
    /// 侧栏「工作区」里用户手动添加的目录，按添加先后排列。
    pub workspaces: Vec<String>,
}

impl Default for WebState {
    fn default() -> Self {
        Self {
            theme: WebTheme::System,
            active_submit: WebSubmitAction::Steering,
            notify: false,
            diff_mode: WebDiffMode::Unified,
            expand_thinking: false,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTheme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSubmitAction {
    #[default]
    Steering,
    Interrupt,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebDiffMode {
    #[default]
    Unified,
    Split,
}

/// 一份带版本戳的状态文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebStateDocument {
    /// 文件内容的版本戳；`web/state_save` 必须带上最新读到的值。
    pub revision: String,
    /// `web.toml` 是否已存在；`false` 时前端可以把旧版 localStorage 数据迁上来。
    pub exists: bool,
    pub state: WebState,
}

/// `web/state_updated` 的方法标记：保存成功后 core 扇出给其余连接。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebStateUpdated;

impl Method for WebStateUpdated {
    const NAME: &'static str = "web/state_updated";
    type Params = WebStateDocument;
    type Result = crate::methods::Empty;
}

// 通知不返回结果；`Result` 只是 `Method` 的形状要求。
impl ServerNotification for WebStateUpdated {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes() {
        assert_eq!(<WebStateGet as Method>::NAME, "web/state");
        assert_eq!(<WebStateSave as Method>::NAME, "web/state_save");
        assert_eq!(<WebStateUpdated as Method>::NAME, "web/state_updated");
        assert_eq!(
            serde_json::to_value(WebStateGetParams {}).expect("可序列化"),
            json!({})
        );

        let document = WebStateDocument {
            revision: "ab".repeat(16),
            exists: true,
            state: WebState {
                theme: WebTheme::Dark,
                active_submit: WebSubmitAction::Interrupt,
                notify: true,
                diff_mode: WebDiffMode::Split,
                expand_thinking: true,
                workspaces: vec!["/repo".into()],
            },
        };
        assert_eq!(
            serde_json::to_value(&document).expect("可序列化"),
            json!({
                "revision": "ab".repeat(16),
                "exists": true,
                "state": {
                    "theme": "dark",
                    "active_submit": "interrupt",
                    "notify": true,
                    "diff_mode": "split",
                    "expand_thinking": true,
                    "workspaces": ["/repo"],
                },
            })
        );
        let round_trip: WebStateDocument =
            serde_json::from_value(serde_json::to_value(&document).expect("可序列化"))
                .expect("可反序列化");
        assert_eq!(round_trip, document);

        // 文件缺字段按默认值补齐；未知字段按写错的键名报错。
        let sparse: WebState =
            serde_json::from_value(json!({"theme": "light"})).expect("可反序列化");
        assert_eq!(
            sparse,
            WebState {
                theme: WebTheme::Light,
                ..WebState::default()
            }
        );
        assert!(serde_json::from_value::<WebState>(json!({"thme": "light"})).is_err());
    }
}
