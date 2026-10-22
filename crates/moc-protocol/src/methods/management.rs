//! 配置编辑、供应商模型发现、存储维护与工作区知识管理。

use super::InputPart;
use crate::method::{ClientRequest, Method};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    #[default]
    Global,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigAction {
    Read {
        scope: ConfigScope,
        cwd: String,
    },
    Save {
        scope: ConfigScope,
        cwd: String,
        document: Value,
        revision: String,
    },
    Models {
        id: String,
        provider: Value,
    },
    RebindWorkspace {
        url: String,
        path: String,
        new_path: String,
    },
    DeleteWorkspace {
        url: String,
        path: String,
        confirmed_peer: String,
    },
    Maintenance {
        apply: bool,
    },
    Workspaces,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsDocument {
    pub path: String,
    pub revision: String,
    /// 目标文件的显式覆盖项，保存时只提交此对象，不回填所有默认值。
    pub document: Value,
    pub effective: Value,
    pub inherited: Value,
    pub defaults: Value,
    pub fields: Vec<ConfigField>,
    pub provider_template: Value,
    pub model_template: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigField {
    /// 路径按段保存，供应商与模型名称中的点号不会被误拆。
    pub path: Vec<String>,
    pub tab: String,
    pub label: String,
    pub description: String,
    pub kind: FieldKind,
    pub advanced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "options", rename_all = "snake_case")]
pub enum FieldKind {
    Bool,
    Integer,
    Number,
    Text,
    Secret,
    Multiline,
    Strings,
    Object,
    Choice(Vec<ChoiceOption>),
}

/// 枚举候选：`value` 是写入配置的原文，`label` 是界面展示的说明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub value: String,
    pub label: String,
}

impl ChoiceOption {
    pub fn new(value: &str, label: &str) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
    /// 展示文本：标签与取值相同时只显示一次。
    pub fn display(&self) -> String {
        if self.label == self.value {
            self.value.clone()
        } else {
            format!("{}（{}）", self.label, self.value)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceBinding {
    pub path: String,
    pub peer_id: String,
    pub url: String,
    pub exists: bool,
    pub shared: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaintenanceReport {
    pub removed: Vec<String>,
    pub protected: usize,
    pub bytes: u64,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ConfigReply {
    Settings(SettingsDocument),
    Models(Vec<CatalogModel>),
    Workspaces(Vec<WorkspaceBinding>),
    Maintenance(MaintenanceReport),
    Updated(String),
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigManage;
impl Method for ConfigManage {
    const NAME: &'static str = "config/manage";
    type Params = ConfigAction;
    type Result = ConfigReply;
}
impl ClientRequest for ConfigManage {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerResult {
    pub accepted: bool,
    /// 接受后由 core 分配的稳定 ID；前端据此撤回仍在 steering 队列里的消息。
    ///
    /// 旧核心不返回该字段，新前端仍可正常处理；新核心的 accepted 结果总是带 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
}

/// Steering 在下一安全工具边界加入当前提示；队列由前端等待前一次提示完成后提交。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerParams {
    pub session_id: String,
    pub input: Vec<InputPart>,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionSteer;
impl Method for SessionSteer {
    const NAME: &'static str = "session/steer";
    type Params = SteerParams;
    type Result = SteerResult;
}
impl ClientRequest for SessionSteer {}

/// 撤回一条尚未被 core 写入会话的 steering 消息。
///
/// 消息只有在下一轮请求前 drain 时才算真正写入；写入之后撤回返回
/// `removed: false`，前端应保留该消息在转写中的位置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsteerParams {
    pub session_id: String,
    pub id: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsteerResult {
    pub removed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionUnsteer;
impl Method for SessionUnsteer {
    const NAME: &'static str = "session/unsteer";
    type Params = UnsteerParams;
    type Result = UnsteerResult;
}
impl ClientRequest for SessionUnsteer {}
