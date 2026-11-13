//! /config：字段树、按字段类型选用的编辑控件、模型候选勾选、维护预览与工作区管理。

mod controls;
mod editors;
mod forms;
mod providers;
mod render;

use providers::{ProviderAction, Providers};

use crate::{
    app::{App, ConfigWrite},
    modal::ModelScroll,
    protocol::{AppEvent, EmbeddedConnection},
    widgets::prompt::PromptState,
};
use controls::Control;
use crossterm::event::{KeyCode, KeyModifiers};
use editors::{Editor, Outcome, TextEditor};
use moc_protocol::{
    CatalogModel, ConfigAction, ConfigField, ConfigManage, ConfigReply, ConfigScope, FieldKind,
    MaintenanceReport, SettingsDocument,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub use render::draw;

/// 选项卡名称；字段归属由 core 的字段目录给出。
pub const TABS: &[&str] = &[
    "模型角色",
    "供应商与模型",
    "指令",
    "技能",
    "工具",
    "沙盒",
    "上下文",
    "会话",
    "MCP",
    "界面",
    "高级",
    "插件",
];

pub struct SettingsPanel {
    pub id: u64,
    pub cwd: String,
    pub scope: ConfigScope,
    pub data: Option<SettingsDocument>,
    pub tui: Value,
    pub effective: Value,
    pub fields: Vec<ConfigField>,
    /// 与 `fields` 一一对应的编辑控件；[`SettingsPanel::rebuild`] 同步生成。
    pub controls: Vec<Control>,
    /// 当前选项卡展开后的可见行：分组头 + 字段。
    pub rows: Vec<Row>,
    /// 用户折叠的顶层分组（顶层默认展开）。
    pub collapsed: BTreeSet<String>,
    /// 用户展开的嵌套分组（嵌套默认折叠）。
    pub expanded: BTreeSet<String>,
    pub tab: usize,
    pub selected: usize,
    pub offset: usize,
    /// 字段列表的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    /// 保留坐标，在最终布局上计算悬停，避免滚动后沿用旧命中。
    pub pointer: Option<ratatui::layout::Position>,
    pub advanced: bool,
    pub filter: String,
    /// 正在进行的编辑：行内单行输入或弹窗控件。
    pub edit: Option<EditState>,
    pub error: Option<String>,
    pub message: Option<String>,
    pub busy: bool,
    pub hits: Vec<(ratatui::layout::Rect, Hit)>,
    pub pending: Option<tokio::task::AbortHandle>,
    pub view: View,
    pub providers: Providers,
    /// 本次进程内的改动历史；只留在内存里，不落盘。
    pub history: History,
    /// 撤回／重做的提示语，写入完成后仍然保留。
    pub outcome: Option<String>,
    /// 切换范围后才能套用的历史快照。
    pub restore: Option<Restore>,
}

/// 请求类型：决定回执如何落到面板上。
#[derive(Clone, Copy)]
pub enum Job {
    /// 读取当前范围的配置文件。
    Load,
    /// 写入配置文件并让核心立即生效。
    Save,
    /// 获取供应商的模型候选。
    Models(u64),
    /// 存储维护的预览或执行结果。
    Maintenance,
    /// 确认卡片批准的一次性写入。
    Update,
}

/// 改动落在哪一份配置上。
#[derive(Clone, Copy)]
enum Store {
    /// 本进程的界面偏好（界面页）。
    Tui,
    /// 核心的配置文件（其余各页）。
    Core,
}

impl Store {
    /// 界面偏好的字段路径以 `tui` 开头。
    fn of(path: &[String]) -> Self {
        if path.first().is_some_and(|part| part == "tui") {
            Store::Tui
        } else {
            Store::Core
        }
    }
}

/// 一次改动的快照：撤回时整体恢复，含改动发生的范围。
#[derive(Clone)]
pub struct Snapshot {
    pub scope: ConfigScope,
    pub document: Value,
    pub tui: Value,
    /// 这次改动做了什么，用于选中项与提示文案。
    pub label: String,
}

/// 撤回／重做要套用的快照：改动发生在别的范围时，先切回去再套用。
pub struct Restore {
    pub entry: Snapshot,
    pub message: String,
    /// 这次是撤回（`true`）还是重做。
    pub undo: bool,
}

/// 进程内的撤回与重做栈。
#[derive(Default)]
pub struct History {
    pub undo: Vec<Snapshot>,
    pub redo: Vec<Snapshot>,
}

impl History {
    /// 记录一条快照：`undo` 为真时是新的改动，进撤回栈并清空重做栈；
    /// 否则是撤回/重做过程中把当前状态推回另一侧。
    fn put(&mut self, snapshot: Snapshot, undo: bool) {
        if undo {
            self.redo.clear();
        }
        let stack = if undo { &mut self.undo } else { &mut self.redo };
        if stack.len() >= HISTORY_LIMIT {
            stack.remove(0);
        }
        stack.push(snapshot);
    }

    /// 把刚取出、尚未应用的快照放回原来的栈。
    ///
    /// 撤回/重做因保存进行中而放弃时必须走这里：走 [`History::put`] 等于声明
    /// 「发生了一次新改动」，会顺手清空重做栈——撤回被放弃却把重做历史丢了。
    fn restore(&mut self, snapshot: Snapshot, undo: bool) {
        let stack = if undo { &mut self.undo } else { &mut self.redo };
        stack.push(snapshot);
    }
}

/// 撤回栈与重做栈共享的上限；超出后丢弃最早的记录。
const HISTORY_LIMIT: usize = 100;

/// 分组链的一项：`(分组键, 标题, 分组路径, 可删除路径)`。
type GroupPath = (String, String, Vec<String>, Option<Vec<String>>);

/// 字段树的一行。
pub struct Row {
    /// 缩进层级。
    pub depth: u8,
    pub kind: RowKind,
}
pub enum RowKind {
    Group(Group),
    /// 指向 [`SettingsPanel::fields`] 的下标。
    Field(usize),
    Action(ProviderAction),
}
/// 可折叠分组：供应商、模型、模型角色与按用途划分的设置组。
pub struct Group {
    pub key: String,
    pub title: String,
    pub detail: String,
    /// 分组在配置里的路径：决定说明文案与右侧摘要。
    pub grouping: Vec<String>,
    /// 删除整组时提交的配置路径；`None` 表示该分组不可删除。
    pub path: Option<Vec<String>>,
}
/// 编辑态：行内单行输入，或带标题的弹窗控件。
pub struct EditState {
    pub target: EditTarget,
    pub editor: Editor,
    /// 弹窗编辑；`false` 为原地行内编辑。
    pub popup: bool,
    pub title: String,
    /// 弹窗正文上方的说明；空串不显示。
    pub note: String,
}

pub enum View {
    Fields,
    /// 维护预览清单有自己的视口偏移，不与字段列表共用 `offset`。
    Maintenance {
        report: MaintenanceReport,
        offset: usize,
    },
    Confirm {
        text: String,
        action: ConfirmAction,
    },
}
pub enum EditTarget {
    Field(Vec<String>),
    /// 选中 Azure 端点后填写 `api_version`；路径指向 `providers.<p>.endpoint`。
    Azure(Vec<String>),
    ModelFilter,
    Model(String),
    Mcp,
    ModelDefaults,
    Filter,
}
pub enum ConfirmAction {
    Remove(Vec<String>),
    Run(ConfigAction),
}
#[derive(Clone)]
pub enum Hit {
    Tab(usize),
    /// 字段树行。
    Row(usize),
    /// 供应商列表（末行是新建入口）。
    Provider(usize),
    /// 新建供应商表单控件。
    ProviderInput(usize),
    ProviderAction(ProviderAction),
    /// 工具栏按钮：等价于按下某个按键。
    Key(KeyCode, KeyModifiers),
    /// 提示条/弹窗 footer 上的键位：等价于按下该键，编辑态下同样生效
    /// （提示条描述的正是当前编辑器接受的键）。
    Hint(KeyCode, KeyModifiers),
    /// 编辑弹窗里的第 N 行（候选、清单项、键值行）。
    Item(usize),
    /// 点击弹窗外部或关闭按钮。
    DismissPopup,
    /// 滚动条点击：目标是视口偏移，不动光标。
    Scroll(usize),
}

impl Drop for SettingsPanel {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }
}

pub fn open(app: &mut App, connection: &EmbeddedConnection) {
    if app.settings.is_some() {
        return;
    }
    app.next_config_id += 1;
    let tui = serde_json::to_value(&app.preferences).expect("TUI 配置可序列化");
    let cwd = app
        .session
        .as_ref()
        .map(|session| session.cwd.clone())
        .unwrap_or_else(|| app.startup_cwd.clone());
    let mut panel = SettingsPanel {
        id: app.next_config_id,
        cwd: cwd.clone(),
        scope: ConfigScope::Global,
        data: None,
        tui: tui.clone(),
        effective: json!({}),
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: if app.session.is_none() { 1 } else { 0 },
        selected: 0,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
        advanced: false,
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Providers::default(),
        history: History::default(),
        outcome: None,
        restore: None,
    };
    if !app.saving_config() {
        panel.ensure_loaded(app, connection);
    }
    app.settings = Some(panel);
    app.completion = None;
}

mod editing;
mod history;
mod navigation;

impl SettingsPanel {}

/// 走共用模型选择器的字段（`models.roles.*` 与 `models.fusion.*`）的名字；
/// 其余路径返回 `None`。`models.fusion.*` 没有角色伪值，只用于识别字段类型。
fn model_role(path: &[String]) -> Option<&str> {
    match path {
        [root, roles, role]
            if root == "models"
                && ((roles == "roles" && role == "explore") || roles == "fusion") =>
        {
            Some(role)
        }
        _ => None,
    }
}

/// Explorer 可继承父模型或彻底禁用；Fusion 选择不受影响。
fn role_specials(role: &str) -> &'static [(&'static str, &'static str)] {
    match role {
        "explore" => &[
            ("inherit", "沿用父会话当前模型"),
            ("none", "禁用 Explorer 子代理"),
        ],
        _ => &[],
    }
}

fn matches_filter(field: &ConfigField, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    field.path.join(".").to_lowercase().contains(&filter)
        || field.label.to_lowercase().contains(&filter)
        || field.description.to_lowercase().contains(&filter)
        || field.tab.to_lowercase().contains(&filter)
}

/// 工具分组标题：配置键 → 用户能看懂的用途。
fn tool_title(key: &str) -> Option<&'static str> {
    Some(match key {
        "output" => "工具输出",
        "bash" => "命令执行",
        "read" => "读取文件",
        "edit" => "编辑文件",
        "write" => "写入文件",
        "grep" => "搜索内容",
        "find" => "查找文件",
        "todo" => "任务清单",
        "job" => "后台任务",
        _ => return None,
    })
}

/// 工具内的二级分组；目前只有 bash 的资源限制需要和超时、日志等字段分开。
fn tool_subgroup_title(key: &str) -> Option<&'static str> {
    match key {
        "limits" => Some("资源限制"),
        _ => None,
    }
}

/// MCP 服务的连接摘要：本地命令或远程地址。
fn server_address(server: &Value) -> String {
    if let Some(command) = server
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
    {
        return format!("本地程序：{command}");
    }
    match server
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
    {
        Some(url) => format!("远程服务：{url}"),
        None => String::new(),
    }
}

mod interaction;
pub use interaction::handle_key;
pub use interaction::mouse;
pub use interaction::paste;
mod persistence;
pub use persistence::apply_model_role;
pub use persistence::finish_save;
pub use persistence::response;

pub fn get<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut value = value;
    for part in path {
        value = value.get(part)?;
    }
    Some(value)
}
pub fn set(value: &mut Value, path: &[String], new: Value) {
    if path.is_empty() {
        *value = new;
        return;
    }
    if !value.is_object() {
        *value = json!({});
    }
    let object = value.as_object_mut().expect("对象");
    if path.len() == 1 {
        object.insert(path[0].clone(), new);
    } else {
        set(
            object.entry(path[0].clone()).or_insert_with(|| json!({})),
            &path[1..],
            new,
        );
    }
}
pub fn remove(value: &mut Value, path: &[String]) {
    if let Some((last, parents)) = path.split_last() {
        let mut value = value;
        for parent in parents {
            let Some(next) = value.get_mut(parent) else {
                return;
            };
            value = next;
        }
        if let Some(object) = value.as_object_mut() {
            object.remove(last);
        }
    }
}

#[cfg(test)]
mod tests;
