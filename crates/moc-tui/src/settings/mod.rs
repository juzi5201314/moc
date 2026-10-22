//! /config：字段树、行内编辑与小弹窗编辑、模型候选勾选、维护预览与工作区管理。

mod forms;
mod render;

use crate::{
    app::{App, ConfigWrite},
    config::{KeyMap, TuiConfig},
    protocol::{AppEvent, EmbeddedConnection},
    widgets::prompt::PromptState,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use moc_protocol::{
    CatalogModel, ConfigAction, ConfigField, ConfigManage, ConfigReply, ConfigScope, FieldKind,
    MaintenanceReport, SettingsDocument, WorkspaceBinding,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub use render::draw;

/// 选项卡名称；字段归属由 core 的字段目录给出。
pub const TABS: &[&str] = &[
    "模型",
    "供应商",
    "指令",
    "技能",
    "工具",
    "沙盒",
    "上下文",
    "会话",
    "MCP",
    "OpenViking",
    "界面",
    "高级",
];

pub struct SettingsPanel {
    pub id: u64,
    pub cwd: String,
    pub scope: ConfigScope,
    pub data: Option<SettingsDocument>,
    pub tui: Value,
    pub effective: Value,
    pub fields: Vec<ConfigField>,
    /// 当前选项卡展开后的可见行：分组头 + 字段。
    pub rows: Vec<Row>,
    /// 用户折叠的顶层分组（顶层默认展开）。
    pub collapsed: BTreeSet<String>,
    /// 用户展开的嵌套分组（嵌套默认折叠）。
    pub expanded: BTreeSet<String>,
    pub tab: usize,
    pub selected: usize,
    pub offset: usize,
    pub advanced: bool,
    pub filter: String,
    /// 正在进行的编辑：行内单行输入或小弹窗。
    pub edit: Option<EditState>,
    pub error: Option<String>,
    pub message: Option<String>,
    pub busy: bool,
    pub hits: Vec<(ratatui::layout::Rect, Hit)>,
    pub pending: Option<tokio::task::AbortHandle>,
    pub view: View,
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
    Models,
    /// 工作区记忆清单。
    Workspaces,
    /// 存储维护的预览或执行结果。
    Maintenance,
    /// 改绑目录、删除工作区记忆等其他一次性写入。
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
}
/// 可折叠分组：供应商、模型、模型角色与按用途划分的设置组。
pub struct Group {
    pub key: String,
    pub title: String,
    pub detail: String,
    /// 删除整组时提交的配置路径；`None` 表示该分组不可删除。
    pub path: Option<Vec<String>>,
}
/// 编辑态：行内单行输入，或带标题与提示的小弹窗。
pub struct EditState {
    pub target: EditTarget,
    pub kind: FieldKind,
    pub input: PromptState,
    pub reveal: bool,
    /// 小弹窗编辑；`false` 为原地行内编辑。
    pub popup: bool,
    pub title: String,
    pub hint: String,
    pub choice_selected: usize,
}

pub enum View {
    Fields,
    Models {
        provider: String,
        rows: Vec<CatalogModel>,
        checked: BTreeSet<String>,
        selected: usize,
    },
    Workspaces {
        rows: Vec<WorkspaceBinding>,
        selected: usize,
    },
    Maintenance(MaintenanceReport),
    Confirm {
        text: String,
        action: ConfirmAction,
    },
}
pub enum EditTarget {
    Field(Vec<String>),
    Provider,
    Model(String),
    Mcp,
    ModelDefaults,
    Rebind(WorkspaceBinding),
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
    /// 模型候选或工作区列表行。
    Check(usize),
    /// 工具栏按钮：等价于按下某个按键。
    Key(KeyCode, KeyModifiers),
    /// 候选单选弹窗选项。
    Choice(usize),
    /// 点击弹窗外部或关闭按钮。
    DismissPopup,
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
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: if app.session.is_none() { 1 } else { 0 },
        selected: 0,
        offset: 0,
        advanced: false,
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
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

impl SettingsPanel {
    /// 改动前状态的快照，撤回时整体恢复。
    fn snapshot(&self, label: &str) -> Snapshot {
        Snapshot {
            scope: self.scope,
            document: self
                .data
                .as_ref()
                .map(|data| data.document.clone())
                .unwrap_or(Value::Null),
            tui: self.tui.clone(),
            label: label.into(),
        }
    }
    /// 一次用户改动：记录快照 → 改内存 → 立刻写入并生效。
    ///
    /// 闭包只负责内存修改，并说明改动落在哪份配置上；出错时不留历史记录，
    /// 界面偏好还会回滚到改动前的样子。
    fn commit(
        &mut self,
        label: &str,
        app: &mut App,
        connection: &EmbeddedConnection,
        apply: impl FnOnce(&mut Self) -> anyhow::Result<Store>,
    ) -> anyhow::Result<()> {
        if self.data.is_none() {
            self.error = Some("设置还没读取完，请稍候".into());
            anyhow::bail!("设置还没读取完，请稍候");
        }
        let restore = self.snapshot(label);
        let store = match apply(self) {
            Ok(store) => store,
            Err(error) => {
                self.error = Some(error.to_string());
                return Err(error);
            }
        };
        if matches!(store, Store::Tui)
            && let Err(error) = self.save_prefs(app)
        {
            self.tui = restore.tui;
            self.rebuild();
            self.error = Some(error.to_string());
            return Err(error);
        }
        self.history.put(restore, true);
        self.error = None;
        self.rebuild();
        match store {
            Store::Core => {
                self.message = None;
                self.outcome = None;
                self.flush(app, connection);
            }
            Store::Tui => {
                self.outcome = None;
                self.message = Some("界面设置已保存并生效".into());
            }
        }
        Ok(())
    }
    /// 把内存里的配置写入文件并立刻生效；有写入在途时先排队合并。
    fn flush(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(data) = &self.data else {
            return;
        };
        app.write_config(
            connection,
            ConfigWrite {
                scope: self.scope,
                cwd: self.cwd.clone(),
                document: data.document.clone(),
                revision: data.revision.clone(),
            },
        );
    }
    /// 写入界面偏好文件，并把新设置应用到当前界面。
    fn save_prefs(&mut self, app: &mut App) -> anyhow::Result<()> {
        let mut preferences: TuiConfig = serde_json::from_value(self.tui.clone())?;
        preferences.statusline = preferences
            .statusline
            .filter(|text| !text.trim().is_empty());
        let keys = KeyMap::compile(&preferences)?;
        app.tui_store
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("界面设置存储尚未初始化"))?
            .save(&preferences)?;
        app.thinking_detail = preferences.thinking_detail;
        app.tool_detail = preferences.tool_detail;
        app.statusline_template = preferences.statusline.clone();
        let scope_changed = app.preferences.history_scope != preferences.history_scope;
        self.tui = serde_json::to_value(&preferences)?;
        app.preferences = preferences;
        if scope_changed {
            app.refresh_input_history();
        }
        app.keymap = keys;
        app.view_gen = app.view_gen.wrapping_add(1);
        Ok(())
    }
    /// Ctrl+Z：撤回最近一次改动。
    fn undo(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(entry) = self.history.undo.pop() else {
            self.message = Some("没有可以撤回的改动".into());
            return;
        };
        self.step(entry, true, app, connection);
    }
    /// Ctrl+Y 或 Ctrl+Shift+Z：重做刚刚撤回的改动。
    fn redo(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(entry) = self.history.redo.pop() else {
            self.message = Some("没有可以重做的改动".into());
            return;
        };
        self.step(entry, false, app, connection);
    }
    /// 套用一条历史快照；改动发生在别的范围时先切回去。
    fn step(
        &mut self,
        entry: Snapshot,
        undo: bool,
        app: &mut App,
        connection: &EmbeddedConnection,
    ) {
        let label = entry.label.clone();
        let verb = if undo { "已撤回" } else { "已重做" };
        if entry.scope != self.scope {
            if app.saving_config() {
                self.history.restore(entry, undo);
                self.message = Some("正在保存改动，请稍候".into());
                return;
            }
            // 当前状态要等切过去读到磁盘内容才知道，那时再记进另一侧栈。
            let scope = entry.scope;
            self.restore = Some(Restore {
                entry,
                message: format!("{verb}：{label}"),
                undo,
            });
            self.set_scope(scope, app, connection);
            return;
        }
        let current = self.snapshot(&label);
        self.history.put(current, !undo);
        self.apply_snapshot(entry, app, connection);
        self.message = Some(format!("{verb}：{label}"));
        self.outcome = self.message.clone();
    }
    /// 把快照套回内存并立刻写入生效。
    fn apply_snapshot(&mut self, entry: Snapshot, app: &mut App, connection: &EmbeddedConnection) {
        let prefs = entry.tui != self.tui;
        let mut document = false;
        self.tui = entry.tui;
        if let Some(data) = &mut self.data
            && data.document != entry.document
        {
            data.document = entry.document;
            document = true;
        }
        self.error = None;
        self.rebuild();
        if prefs && let Err(error) = self.save_prefs(app) {
            self.error = Some(error.to_string());
        }
        if document {
            self.flush(app, connection);
        }
    }
    /// 切换全局／项目范围：重新读取该范围的配置文件。
    fn set_scope(&mut self, scope: ConfigScope, app: &mut App, connection: &EmbeddedConnection) {
        if self.busy {
            self.message = Some("正在读取设置，请稍候".into());
            return;
        }
        self.scope = scope;
        self.data = None;
        self.collapsed.clear();
        self.expanded.clear();
        self.selected = 0;
        self.offset = 0;
        self.edit = None;
        self.request(
            connection,
            &app.events,
            ConfigAction::Read {
                scope,
                cwd: self.cwd.clone(),
            },
            Job::Load,
        );
    }
    /// 读取、发现、维护等回执：并入面板视图。
    fn handle_response(
        &mut self,
        app: &mut App,
        connection: &EmbeddedConnection,
        result: anyhow::Result<ConfigReply>,
    ) {
        match result {
            Err(error) => {
                self.restore = None;
                self.message = None;
                self.error = Some(error.to_string());
            }
            Ok(ConfigReply::Settings(data)) => {
                self.data = Some(data);
                match self.restore.take() {
                    // 撤回／重做发生在别的范围：切回来之后先记下磁盘状态，再套用快照。
                    Some(restore) => {
                        let current = self.snapshot(&restore.entry.label);
                        self.history.put(current, !restore.undo);
                        self.apply_snapshot(restore.entry, app, connection);
                        self.message = Some(restore.message.clone());
                        self.outcome = Some(restore.message);
                    }
                    None => {
                        self.message = None;
                        self.rebuild();
                    }
                }
            }
            Ok(ConfigReply::Models(rows)) => {
                if let View::Models { rows: current, .. } = &mut self.view {
                    *current = rows;
                }
            }
            Ok(ConfigReply::Workspaces(rows)) => {
                self.view = View::Workspaces { rows, selected: 0 };
            }
            Ok(ConfigReply::Maintenance(report)) => self.view = View::Maintenance(report),
            Ok(ConfigReply::Updated(message)) => {
                self.view = View::Fields;
                self.message = Some(message);
            }
        }
    }
    /// 确保已经读到配置文件；仍有改动在写入时先等着，免得读到旧内容又覆盖回去。
    fn ensure_loaded(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        if self.data.is_some() || self.busy {
            return;
        }
        self.request(
            connection,
            &app.events,
            ConfigAction::Read {
                scope: self.scope,
                cwd: self.cwd.clone(),
            },
            Job::Load,
        );
    }
    pub fn rebuild(&mut self) {
        let Some(data) = &self.data else {
            return;
        };
        self.effective = data.inherited.clone();
        moc_core::config::editor::merge_json(&mut self.effective, data.document.clone());
        moc_core::config::catalog::complete_view(&mut self.effective);
        self.fields = moc_core::config::catalog::fields(&self.effective);
        self.fields.extend(crate::config::fields());
        self.effective["tui"] = self.tui.clone();
        self.rebuild_rows();
    }
    fn rebuild_rows(&mut self) {
        let filter = self.filter.to_lowercase();
        let searching = !filter.is_empty();
        let mut rows = Vec::new();
        let mut open: Vec<String> = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            if searching {
                if !matches_filter(field, &filter) {
                    continue;
                }
            } else if !self.matches_tab(field) {
                continue;
            }
            let chain = self.group_chain(&field.path);
            let common = chain
                .iter()
                .zip(&open)
                .take_while(|(group, key)| group.0 == **key)
                .count();
            for (depth, group) in chain.iter().enumerate().skip(common) {
                let detail = self.group_detail(&group.2);
                rows.push(Row {
                    depth: u8::try_from(depth).unwrap_or(u8::MAX),
                    kind: RowKind::Group(Group {
                        key: group.0.clone(),
                        title: group.1.clone(),
                        detail,
                        path: group.3.clone(),
                    }),
                });
            }
            open = chain.iter().map(|group| group.0.clone()).collect();
            let visible = searching
                || chain
                    .iter()
                    .enumerate()
                    .all(|(depth, group)| self.is_expanded(&group.0, depth));
            if visible {
                rows.push(Row {
                    depth: u8::try_from(chain.len()).unwrap_or(u8::MAX),
                    kind: RowKind::Field(index),
                });
            }
        }
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.offset = self.offset.min(self.rows.len().saturating_sub(1));
    }
    fn matches_tab(&self, field: &ConfigField) -> bool {
        let tab = TABS[self.tab];
        if !field.advanced {
            return field.tab == tab;
        }
        tab == "高级" || (self.advanced && field.tab == tab)
    }
    /// 字段所属分组链：`(分组键, 标题, 分组路径, 可删除路径)`。
    fn group_chain(&self, path: &[String]) -> Vec<GroupPath> {
        let mut chain = Vec::new();
        let second = path.get(1).map(String::as_str);
        match path.first().map(String::as_str) {
            Some("providers") => {
                let Some(provider) = path.get(1) else {
                    return chain;
                };
                chain.push((
                    format!("providers/{provider}"),
                    provider.clone(),
                    vec!["providers".into(), provider.clone()],
                    Some(vec!["providers".into(), provider.clone()]),
                ));
                if path.get(2).is_some_and(|part| part == "models")
                    && let Some(model) = path.get(3)
                {
                    chain.push((
                        format!("providers/{provider}/models/{model}"),
                        model.clone(),
                        vec![
                            "providers".into(),
                            provider.clone(),
                            "models".into(),
                            model.clone(),
                        ],
                        Some(vec![
                            "providers".into(),
                            provider.clone(),
                            "models".into(),
                            model.clone(),
                        ]),
                    ));
                }
            }
            Some("models") if second == Some("roles") => chain.push((
                "models/roles".into(),
                "模型角色".into(),
                vec!["models".into(), "roles".into()],
                None,
            )),
            Some("defaults") if path.len() >= 3 => {
                let Some(pattern) = path.get(2) else {
                    return chain;
                };
                chain.push((
                    format!("defaults/models/{pattern}"),
                    format!("默认 {pattern}"),
                    vec!["defaults".into(), "models".into(), pattern.clone()],
                    Some(vec!["defaults".into(), "models".into(), pattern.clone()]),
                ));
            }
            // 工具按子命令分组，避免同名设置（搜索超时、单文件上限）互相混淆。
            Some("tools") if path.len() >= 3 => {
                let tool = path[1].clone();
                if let Some(title) = tool_title(&tool) {
                    chain.push((
                        format!("tools/{tool}"),
                        title.into(),
                        vec!["tools".into(), tool.clone()],
                        None,
                    ));
                    if let Some(sub) = path.get(2)
                        && let Some(subtitle) = tool_subgroup_title(sub)
                    {
                        chain.push((
                            format!("tools/{tool}/{sub}"),
                            subtitle.into(),
                            vec!["tools".into(), tool.clone(), sub.clone()],
                            None,
                        ));
                    }
                }
            }
            Some("compaction") if path.len() >= 3 => {
                let title = match second {
                    Some("shake") => "本地修剪",
                    Some("remote") => "服务端压缩",
                    _ => return chain,
                };
                let method = path[1].clone();
                chain.push((
                    format!("compaction/{method}"),
                    title.into(),
                    vec!["compaction".into(), method],
                    None,
                ));
            }
            Some("session") if second == Some("retention") && path.len() >= 3 => chain.push((
                "session/retention".into(),
                "会话保留".into(),
                vec!["session".into(), "retention".into()],
                None,
            )),
            Some("mcp") if second == Some("servers") && path.len() >= 3 => {
                let Some(name) = path.get(2) else {
                    return chain;
                };
                chain.push((
                    format!("mcp/servers/{name}"),
                    name.clone(),
                    vec!["mcp".into(), "servers".into(), name.clone()],
                    Some(vec!["mcp".into(), "servers".into(), name.clone()]),
                ));
            }
            _ => {}
        }
        chain
    }
    /// 分组行右侧的摘要：供应商显示模型数量，模型显示关键设置，其余显示子项数量。
    fn group_detail(&self, group: &[String]) -> String {
        match group {
            [root, provider] if root == "providers" => {
                let count = self
                    .effective
                    .get("providers")
                    .and_then(|providers| providers.get(provider))
                    .and_then(|provider| provider.get("models"))
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len);
                if count > 0 {
                    format!("{count} 个模型")
                } else {
                    "尚未添加模型".into()
                }
            }
            [root, provider, models, model] if root == "providers" && models == "models" => {
                self.model_summary(provider, model)
            }
            [root, roles] if root == "models" && roles == "roles" => String::new(),
            [root, name] if root == "mcp" => match self.value(group).map(server_address) {
                Some(address) if !address.is_empty() => address,
                _ => String::new(),
            },
            [root, ..] if root == "defaults" => "模型默认值".into(),
            _ => String::new(),
        }
    }
    /// 模型行的摘要：上下文长度、回复上限、思考与主模型标记。
    fn model_summary(&self, provider: &str, model: &str) -> String {
        let Some(value) = self
            .effective
            .get("providers")
            .and_then(|providers| providers.get(provider))
            .and_then(|provider| provider.get("models"))
            .and_then(|models| models.get(model))
        else {
            return String::new();
        };
        let mut parts = Vec::new();
        if let Some(window) = value.get("context_window").and_then(Value::as_u64) {
            parts.push(format!("上下文 {}", crate::render::format_tokens(window)));
        }
        if let Some(output) = value.get("max_output_tokens").and_then(Value::as_u64) {
            parts.push(format!("回复 {}", crate::render::format_tokens(output)));
        }
        if value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            parts.push("支持思考".into());
        }
        let primary = self
            .effective
            .get("models")
            .and_then(|models| models.get("roles"))
            .and_then(|roles| roles.get("default"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if primary.split(':').next() == Some(&format!("{provider}/{model}")) {
            parts.push("主模型".into());
        }
        parts.join(" · ")
    }
    pub fn is_expanded(&self, key: &str, depth: usize) -> bool {
        if !self.filter.is_empty() {
            return true;
        }
        if depth == 0 {
            !self.collapsed.contains(key)
        } else {
            self.expanded.contains(key)
        }
    }
    fn toggle_group(&mut self, key: &str, depth: u8) {
        let key = key.to_owned();
        if depth == 0 {
            if !self.collapsed.remove(&key) {
                self.collapsed.insert(key);
            }
        } else if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.rebuild_rows();
    }
    pub fn expand_all(&mut self) {
        for row in &self.rows {
            if let RowKind::Group(group) = &row.kind {
                self.collapsed.remove(&group.key);
                self.expanded.insert(group.key.clone());
            }
        }
        self.rebuild_rows();
    }
    pub fn collapse_all(&mut self) {
        for row in &self.rows {
            if let RowKind::Group(group) = &row.kind {
                self.collapsed.insert(group.key.clone());
                self.expanded.remove(&group.key);
            }
        }
        self.rebuild_rows();
    }
    pub fn row(&self) -> Option<&RowKind> {
        self.rows.get(self.selected).map(|row| &row.kind)
    }
    pub fn field(&self) -> Option<&ConfigField> {
        match self.row()? {
            RowKind::Field(index) => self.fields.get(*index),
            RowKind::Group(_) => None,
        }
    }
    pub fn group(&self) -> Option<&Group> {
        match self.row()? {
            RowKind::Group(group) => Some(group),
            RowKind::Field(_) => None,
        }
    }
    pub fn value(&self, path: &[String]) -> Option<&Value> {
        get(&self.effective, path)
    }
    /// 字段是否由当前配置文件显式设置（界面偏好没有继承概念）。
    pub fn explicit(&self, path: &[String]) -> bool {
        self.data
            .as_ref()
            .is_some_and(|data| get(&data.document, path).is_some())
    }
    /// 在配置里写入一项；`tui` 开头的路径写界面偏好。
    fn set_value(&mut self, path: &[String], value: Value) {
        if path.first().is_some_and(|part| part == "tui") {
            set(&mut self.tui, &path[1..], value);
        } else if let Some(data) = &mut self.data {
            set(&mut data.document, path, value);
        }
    }
    /// 恢复默认：删掉配置文件里的这一项；界面偏好则写回默认值。
    fn remove_value(&mut self, path: &[String]) {
        if path.first().is_some_and(|part| part == "tui") {
            let defaults = serde_json::to_value(TuiConfig::default()).expect("默认设置");
            let value = get(&defaults, &path[1..]).cloned().unwrap_or(Value::Null);
            set(&mut self.tui, &path[1..], value);
        } else if let Some(data) = &mut self.data {
            remove(&mut data.document, path);
        }
    }
    fn request(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
        action: ConfigAction,
        job: Job,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let client = connection.client.clone();
        let events = events.clone();
        let panel = self.id;
        let task = tokio::spawn(async move {
            let result = client
                .call::<ConfigManage>(action)
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SettingsResponse { panel, job, result });
        });
        self.pending = Some(task.abort_handle());
    }
    fn select_tab(&mut self, index: usize) {
        self.tab = index;
        self.selected = 0;
        self.offset = 0;
        self.edit = None;
        self.rebuild_rows();
    }
    /// 选中指定路径对应的行，必要时展开其分组。
    fn select_path(&mut self, path: &[String]) {
        for (index, row) in self.rows.iter().enumerate() {
            let matched = match &row.kind {
                RowKind::Field(field) => self.fields[*field].path == path,
                RowKind::Group(group) => group.path.as_deref() == Some(path),
            };
            if matched {
                self.selected = index;
                return;
            }
        }
    }
    /// 开始编辑选中行：单行字段原地编辑，复杂字段用小弹窗。
    fn begin_edit(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(RowKind::Field(index)) = self.row() else {
            if let Some((key, depth)) =
                self.rows
                    .get(self.selected)
                    .and_then(|row| match &row.kind {
                        RowKind::Group(group) => Some((group.key.clone(), row.depth)),
                        RowKind::Field(_) => None,
                    })
            {
                self.toggle_group(&key, depth);
            }
            return;
        };
        let field = self.fields[*index].clone();
        let value = self.value(&field.path).cloned().unwrap_or(Value::Null);
        // 模型角色字段走共用的可搜索模型选择器（含思考等级）；一个候选都没有时
        // 才退回就地文本编辑，让用户手写 selector。
        if let Some(role) = model_role(&field.path)
            && !app.models.is_empty()
        {
            app.open_config_model_picker(
                field.path.clone(),
                field.label.clone(),
                forms::format_value(&value, &FieldKind::Text),
                role_specials(role),
            );
            return;
        }
        let kind = self.field_kind(&field);

        if let FieldKind::Bool = kind {
            let current = value.as_bool().unwrap_or(false);
            let label = field.label.clone();
            let path = field.path.clone();
            let _ = self.commit(&label, app, connection, |panel| {
                panel.set_value(&path, json!(!current));
                Ok(Store::of(&path))
            });
            return;
        }
        let (popup, choice_selected) = match &kind {
            FieldKind::Multiline | FieldKind::Strings | FieldKind::Object => (true, 0),
            FieldKind::Choice(options) => {
                if field.path.last().is_some_and(|k| k == "input")
                    && field.path.iter().any(|p| p == "models")
                {
                    let has_image = value
                        .as_array()
                        .is_some_and(|arr| arr.iter().any(|v| v == "image"))
                        || value.as_str().is_some_and(|s| s.contains("image"));
                    let pos = if has_image {
                        options
                            .iter()
                            .position(|opt| opt.value.contains("image"))
                            .unwrap_or(1)
                    } else {
                        0
                    };
                    (true, pos)
                } else {
                    let current_str = forms::format_value(&value, &kind);
                    let pos = options
                        .iter()
                        .position(|opt| opt.value == current_str || opt.label == current_str)
                        .unwrap_or(0);
                    (true, pos)
                }
            }
            _ => (false, 0),
        };
        self.edit = Some(EditState {
            target: EditTarget::Field(field.path),
            hint: forms::hint_for(&kind).into(),
            input: PromptState::from_text(forms::edit_text(&value, &kind)),
            kind,
            reveal: false,
            popup,
            title: field.label,
            choice_selected,
        });
    }
    /// 快速轮换单选项（按 Space 或方向键时调用，无需进入编辑态）。
    pub fn cycle_choice(
        &mut self,
        app: &mut App,
        connection: &EmbeddedConnection,
        forward: bool,
    ) -> bool {
        let Some(RowKind::Field(index)) = self.row() else {
            return false;
        };
        let field = self.fields[*index].clone();
        let value = self.value(&field.path).cloned().unwrap_or(Value::Null);
        if field.path.last().is_some_and(|k| k == "input")
            && field.path.iter().any(|p| p == "models")
        {
            let has_image = value
                .as_array()
                .is_some_and(|arr| arr.iter().any(|v| v == "image"))
                || value.as_str().is_some_and(|s| s.contains("image"));
            let (next_val, label_text) = if has_image {
                (json!(["text"]), "仅文本 (text)")
            } else {
                (
                    json!(["text", "image"]),
                    "文本与图片 / 多模态 (text, image)",
                )
            };
            let label = format!("{}: {}", field.label, label_text);
            let path = field.path.clone();
            let _ = self.commit(&label, app, connection, |panel| {
                panel.set_value(&path, next_val);
                Ok(Store::of(&path))
            });
            return true;
        }
        let kind = self.field_kind(&field);
        if let FieldKind::Choice(options) = kind {
            if options.is_empty() {
                return false;
            }
            let current_str = forms::format_value(&value, &FieldKind::Text);
            let pos = options
                .iter()
                .position(|opt| opt.value == current_str || opt.label == current_str)
                .unwrap_or(0);
            let next_pos = if forward {
                (pos + 1) % options.len()
            } else {
                pos.checked_sub(1).unwrap_or(options.len() - 1)
            };
            let next_opt = &options[next_pos];
            let label = format!("{}: {}", field.label, next_opt.label);
            let path = field.path.clone();
            let next_val = next_opt.value.clone();
            let _ = self.commit(&label, app, connection, |panel| {
                panel.set_value(&path, json!(next_val));
                Ok(Store::of(&path))
            });
            return true;
        }
        false
    }
    /// 字段的编辑类型：输入模态改为候选列表；模型角色由共用的模型选择器接管。
    fn field_kind(&self, field: &ConfigField) -> FieldKind {
        if field.path.last().is_some_and(|k| k == "input")
            && field.path.iter().any(|p| p == "models")
        {
            return FieldKind::Choice(vec![
                moc_protocol::ChoiceOption::new("text", "仅文本 (text)"),
                moc_protocol::ChoiceOption::new("text, image", "文本与图片 / 多模态 (text, image)"),
            ]);
        }

        field.kind.clone()
    }

    /// 打开一个只有一行输入的弹窗（新增条目、搜索等）。
    fn begin_named(&mut self, target: EditTarget, title: &str, hint: &str, initial: &str) {
        self.edit = Some(EditState {
            target,
            kind: FieldKind::Text,
            input: PromptState::from_text(initial.into()),
            reveal: false,
            popup: true,
            title: title.into(),
            hint: hint.into(),
            choice_selected: 0,
        });
    }
    /// 当前上下文对应的供应商：选中的分组或其字段，退化为第一个供应商。
    fn provider(&self) -> Option<String> {
        if let Some(RowKind::Group(group)) = self.row()
            && let Some(name) = group.path.as_ref().and_then(|path| path.get(1))
            && group
                .path
                .as_ref()
                .is_some_and(|path| path.first().is_some_and(|part| part == "providers"))
        {
            return Some(name.clone());
        }
        if let Some(path) = self.field().map(|field| &field.path)
            && path.first().is_some_and(|part| part == "providers")
        {
            return path.get(1).cloned();
        }
        self.effective
            .get("providers")?
            .as_object()?
            .keys()
            .next()
            .cloned()
    }
    fn add(&mut self) {
        match TABS[self.tab] {
            "供应商" => self.begin_named(
                EditTarget::Provider,
                "添加供应商",
                "填写供应商标识，例如 local",
                "",
            ),
            "模型" => {
                if let Some(provider) = self.provider() {
                    self.begin_named(
                        EditTarget::Model(provider.clone()),
                        &format!("为 {provider} 添加模型"),
                        "填写模型 ID，例如 gpt-4o",
                        "",
                    );
                } else {
                    self.error = Some("请先在“供应商”页添加供应商".into());
                }
            }
            "MCP" => self.begin_named(
                EditTarget::Mcp,
                "添加 MCP 服务",
                "填写服务标识，例如 filesystem",
                "",
            ),
            _ => {
                self.error = Some("“供应商”“模型”“MCP”页可以添加条目".into());
            }
        }
    }
    fn fetch(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(provider) = self.provider() else {
            self.error = Some("请先在“供应商”页填写服务地址与密钥".into());
            return;
        };
        let value = self.effective["providers"][&provider].clone();
        self.view = View::Models {
            provider: provider.clone(),
            rows: Vec::new(),
            checked: BTreeSet::new(),
            selected: 0,
        };
        self.request(
            connection,
            events,
            ConfigAction::Models {
                id: provider,
                provider: value,
            },
            Job::Models,
        );
    }
    /// 在配置里登记一个模型；主模型为空时顺带设为它。
    fn insert_model(&mut self, provider: &str, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!id.trim().is_empty(), "模型 ID 不能为空");
        let path = vec![
            "providers".into(),
            provider.into(),
            "models".into(),
            id.into(),
        ];
        anyhow::ensure!(self.value(&path).is_none(), "模型 {provider}/{id} 已存在");
        self.set_value(&path, json!({}));
        let primary = self
            .effective
            .pointer("/models/roles/default")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if primary.is_empty() {
            self.set_value(
                &["models".into(), "roles".into(), "default".into()],
                json!(format!("{provider}/{id}")),
            );
        }
        Ok(())
    }
}

/// 模型角色字段（`models.roles.*`）的角色名；其余路径返回 `None`。
fn model_role(path: &[String]) -> Option<&str> {
    match path {
        [root, roles, role] if root == "models" && roles == "roles" => Some(role),
        _ => None,
    }
}

/// 角色字段在模型选择器里的伪值：explore 可沿用主模型，plan 还可沿用当前会话模型。
fn role_specials(role: &str) -> &'static [(&'static str, &'static str)] {
    const INHERIT: [(&str, &str); 1] = [("inherit", "沿用主模型")];
    const INHERIT_OR_NONE: [(&str, &str); 2] =
        [("inherit", "沿用主模型"), ("none", "沿用当前会话模型")];
    match role {
        "explore" => &INHERIT,
        "plan" => &INHERIT_OR_NONE,
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
    let Some(mut panel) = app.settings.take().filter(|panel| panel.id == id) else {
        return;
    };
    panel.pending = None;
    panel.busy = false;
    panel.handle_response(app, connection, result);
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
            adopt(app, connection, data, keep_local);
        }
        Err(error) => {
            app.config_queue = None;
            let text = error.to_string();
            match app.settings.as_mut() {
                Some(panel) => {
                    panel.message = None;
                    panel.outcome = None;
                    panel.error = Some(text);
                }
                None => app.set_notice(format!("配置保存失败：{text}")),
            }
        }
        Ok(_) => {}
    }
    reload_if_waiting(app, connection);
}

/// 设置页若在等写入落地后才读取配置，这里补上。
fn reload_if_waiting(app: &mut App, connection: &EmbeddedConnection) {
    if app.saving_config() {
        return;
    }
    if let Some(mut panel) = app.settings.take() {
        panel.ensure_loaded(app, connection);
        app.settings = Some(panel);
    }
}

/// 把写入结果并入设置面板，并在配置稳定后刷新模型信息。
fn adopt(app: &mut App, connection: &EmbeddedConnection, data: SettingsDocument, keep_local: bool) {
    let Some(panel) = app.settings.as_mut() else {
        return;
    };
    let document = if keep_local {
        panel.data.as_ref().map(|current| current.document.clone())
    } else {
        None
    };
    panel.data = Some(data);
    if let Some(document) = document
        && let Some(current) = &mut panel.data
    {
        current.document = document;
    }
    panel.error = None;
    panel.rebuild();
    if keep_local {
        return;
    }
    panel.message = Some(
        panel
            .outcome
            .take()
            .unwrap_or_else(|| "已保存并生效".into()),
    );
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<moc_protocol::ConfigGet>(moc_protocol::ConfigGetParams {})
            .await
        {
            Ok(config) => {
                let _ = events.send(AppEvent::ConfigurationLoaded(config));
            }
            Err(error) => {
                let _ = events.send(AppEvent::Notice(format!("刷新模型信息失败：{error}")));
            }
        }
    });
}

pub fn handle_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let Some(mut panel) = app.settings.take() else {
        return;
    };
    let close = forms::key(&mut panel, app, connection, modifiers, code);
    if !close {
        app.settings = Some(panel);
    } else {
        crate::queue::dispatch(app, connection);
    }
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

pub fn paste(app: &mut App, text: &str) {
    if let Some(panel) = &mut app.settings
        && let Some(edit) = &mut panel.edit
    {
        edit.input.insert_str_raw(text);
    }
}
pub fn mouse(app: &mut App, connection: &EmbeddedConnection, event: MouseEvent) {
    let Some(panel) = &mut app.settings else {
        return;
    };
    match event.kind {
        MouseEventKind::ScrollDown => {
            if let Some(edit) = &mut panel.edit {
                if let FieldKind::Choice(options) = &edit.kind {
                    if !options.is_empty() {
                        edit.choice_selected = (edit.choice_selected + 1) % options.len();
                        return;
                    }
                }
                return;
            }
            match &mut panel.view {
                View::Fields => {
                    panel.selected = panel
                        .selected
                        .saturating_add(3)
                        .min(panel.rows.len().saturating_sub(1));
                }
                View::Models { rows, selected, .. } => {
                    *selected = (*selected + 3).min(rows.len().saturating_sub(1));
                }
                View::Workspaces { rows, selected } => {
                    *selected = (*selected + 3).min(rows.len().saturating_sub(1));
                }
                View::Maintenance(report) => {
                    let total = report.removed.len();
                    panel.offset = (panel.offset + 3).min(total.saturating_sub(1));
                }
                _ => {}
            }
            return;
        }
        MouseEventKind::ScrollUp => {
            if let Some(edit) = &mut panel.edit {
                if let FieldKind::Choice(options) = &edit.kind {
                    if !options.is_empty() {
                        edit.choice_selected = edit
                            .choice_selected
                            .checked_sub(1)
                            .unwrap_or(options.len() - 1);
                        return;
                    }
                }
                return;
            }
            match &mut panel.view {
                View::Fields => {
                    panel.selected = panel.selected.saturating_sub(3);
                }
                View::Models { selected, .. } => {
                    *selected = selected.saturating_sub(3);
                }
                View::Workspaces { selected, .. } => {
                    *selected = selected.saturating_sub(3);
                }
                View::Maintenance(_) => {
                    panel.offset = panel.offset.saturating_sub(3);
                }
                _ => {}
            }
            return;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return,
    }
    let clicked = (event.column, event.row).into();
    let hit = panel
        .hits
        .iter()
        .find(|(area, _)| area.contains(clicked))
        .map(|(_, hit)| hit.clone());
    if panel.edit.is_some() {
        match hit {
            Some(Hit::Choice(index)) => {
                if let Some(edit) = &mut panel.edit {
                    edit.choice_selected = index;
                }
                handle_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
            }
            Some(Hit::DismissPopup) | Some(Hit::Key(KeyCode::Esc, _)) => {
                handle_key(app, connection, KeyModifiers::NONE, KeyCode::Esc);
            }
            _ => {}
        }
        return;
    }
    let Some(hit) = hit else {
        return;
    };
    match hit {
        Hit::Tab(index) => panel.select_tab(index),
        Hit::Row(index) => {
            if panel.selected == index {
                handle_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
            } else {
                panel.selected = index;
            }
        }
        Hit::Check(index) => {
            match &mut panel.view {
                View::Models { selected, .. } => *selected = index,
                View::Workspaces { selected, .. } => *selected = index,
                _ => {}
            }
            if matches!(panel.view, View::Models { .. }) {
                // 与 Space 同一条勾选路径：已添加的模型保持不可勾选，否则会攒出
                // 一条不含任何新增的空改动，并提示「已添加 0 个模型」。
                handle_key(app, connection, KeyModifiers::NONE, KeyCode::Char(' '));
            }
        }
        Hit::Key(code, modifiers) => handle_key(app, connection, modifiers, code),
        _ => {}
    }
}

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
mod tests {
    use super::*;

    /// 小6：放弃未应用的撤回只把快照放回原栈，重做历史必须留着。
    ///
    /// 走 `History::put` 会按「新改动」语义清空重做栈——撤回没执行，重做却没了。
    #[test]
    fn restore_keeps_the_other_stack() {
        let snapshot = |label: &str| Snapshot {
            scope: ConfigScope::Global,
            document: Value::Null,
            tui: Value::Null,
            label: label.into(),
        };
        let mut history = History {
            undo: vec![snapshot("改 A")],
            redo: vec![snapshot("被撤回的 B")],
        };

        // 撤回被保存阻塞：快照放回撤回栈，重做栈不动
        let entry = history.undo.pop().expect("撤回栈有快照");
        history.restore(entry, true);
        assert_eq!(history.undo.len(), 1, "快照回到原栈");
        assert_eq!(history.redo.len(), 1, "重做历史不受影响");
        assert_eq!(history.undo[0].label, "改 A");

        // 对照：put 的语义是「发生新改动」，会清空重做栈
        let entry = history.undo.pop().expect("撤回栈有快照");
        history.put(entry, true);
        assert!(history.redo.is_empty(), "put 作为新改动清空重做栈");
    }

    /// nit7：鼠标点已添加的模型不产生空改动，也不提示「已添加 0 个模型」。
    ///
    /// 旧的鼠标路径直接翻转 `checked`，绕过了「已添加即不可勾选」的判定：勾不出
    /// 任何新增，Enter 却照常写入一条空改动。
    #[tokio::test]
    async fn clicking_an_added_model_keeps_the_selection_empty() {
        use crate::settings::{Hit, View};
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use moc_core::{Core, CoreConfig};
        use moc_protocol::methods::CatalogModel;
        use ratatui::layout::Rect;
        use std::collections::BTreeSet;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        )
        .unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(crate::theme::Theme::global(), events);

        app.settings = Some(SettingsPanel {
            id: 1,
            cwd: "/tmp".into(),
            scope: ConfigScope::Global,
            data: None,
            tui: Value::Null,
            effective: serde_json::json!({
                "providers": { "p": { "models": { "m1": {} } } }
            }),
            fields: Vec::new(),
            rows: Vec::new(),
            collapsed: BTreeSet::new(),
            expanded: BTreeSet::new(),
            tab: 0,
            selected: 0,
            offset: 0,
            advanced: false,
            filter: String::new(),
            edit: None,
            error: None,
            message: None,
            busy: false,
            hits: vec![(Rect::new(10, 5, 20, 1), Hit::Check(0))],
            pending: None,
            view: View::Models {
                provider: "p".into(),
                rows: vec![CatalogModel {
                    id: "m1".into(),
                    name: "m1".into(),
                }],
                checked: BTreeSet::new(),
                selected: 0,
            },
            history: History::default(),
            outcome: None,
            restore: None,
        });

        mouse(
            &mut app,
            &connection,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 5,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );

        let panel = app.settings.as_ref().expect("面板仍在");
        match &panel.view {
            View::Models { checked, .. } => {
                assert!(checked.is_empty(), "已添加的模型不得进入勾选集合");
            }
            _ => unreachable!("仍在模型候选视图"),
        }
        assert!(panel.message.is_none(), "不得产生任何提示");

        connection.shutdown().await;
        core.shutdown().await;
    }

    #[test]
    fn test_matches_filter_multidimensional() {
        let field = ConfigField {
            path: vec!["providers".into(), "openai".into(), "api_key".into()],
            tab: "供应商".into(),
            label: "API 密钥".into(),
            description: "访问供应商的鉴权密钥".into(),
            kind: FieldKind::Secret,
            advanced: false,
        };
        assert!(matches_filter(&field, "api_key"));
        assert!(matches_filter(&field, "密钥"));
        assert!(matches_filter(&field, "鉴权"));
        assert!(matches_filter(&field, "供应商"));
        assert!(matches_filter(&field, "API"));
        assert!(!matches_filter(&field, "notfound"));
    }

    #[test]
    fn test_format_number_commas() {
        assert_eq!(render::format_number(0), "0");
        assert_eq!(render::format_number(999), "999");
        assert_eq!(render::format_number(1000), "1,000");
        assert_eq!(render::format_number(32000), "32,000");
        assert_eq!(render::format_number(200000), "200,000");
        assert_eq!(render::format_number(10000000), "10,000,000");
    }

    #[test]
    fn test_parse_friendly_inputs() {
        assert_eq!(forms::parse("开", &FieldKind::Bool).unwrap(), json!(true));
        assert_eq!(forms::parse("开启", &FieldKind::Bool).unwrap(), json!(true));
        assert_eq!(forms::parse("关", &FieldKind::Bool).unwrap(), json!(false));
        assert_eq!(
            forms::parse("关闭", &FieldKind::Bool).unwrap(),
            json!(false)
        );
        assert_eq!(forms::parse("true", &FieldKind::Bool).unwrap(), json!(true));
        assert_eq!(
            forms::parse("false", &FieldKind::Bool).unwrap(),
            json!(false)
        );
        assert_eq!(forms::parse("y", &FieldKind::Bool).unwrap(), json!(true));
        assert_eq!(forms::parse("n", &FieldKind::Bool).unwrap(), json!(false));
        assert!(forms::parse("notabool", &FieldKind::Bool).is_err());

        assert_eq!(
            forms::parse("123", &FieldKind::Integer).unwrap(),
            json!(123)
        );
        assert_eq!(
            forms::parse("200k", &FieldKind::Integer).unwrap(),
            json!(200_000)
        );
        assert_eq!(
            forms::parse("200K", &FieldKind::Integer).unwrap(),
            json!(200_000)
        );
        assert_eq!(
            forms::parse("1m", &FieldKind::Integer).unwrap(),
            json!(1_000_000)
        );
        assert_eq!(
            forms::parse("200,000", &FieldKind::Integer).unwrap(),
            json!(200_000)
        );
        assert!(forms::parse("invalid", &FieldKind::Integer).is_err());

        assert_eq!(
            forms::parse("1.5k", &FieldKind::Number).unwrap(),
            json!(1500.0)
        );
        assert_eq!(
            forms::parse("10,000.5", &FieldKind::Number).unwrap(),
            json!(10000.5)
        );

        assert_eq!(
            forms::parse("   ", &FieldKind::Integer).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_parse_thinking_levels() {
        assert_eq!(
            forms::parse_thinking_levels("low, medium, high").unwrap(),
            json!(["low", "medium", "high"])
        );
        assert_eq!(
            forms::parse_thinking_levels("低, 中, 高").unwrap(),
            json!(["low", "medium", "high"])
        );
        assert_eq!(
            forms::parse_thinking_levels("低、中、高、超高、最大").unwrap(),
            json!(["low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(forms::parse_thinking_levels("关").unwrap(), json!(["off"]));
        assert_eq!(forms::parse_thinking_levels("  ").unwrap(), Value::Null);
        assert!(forms::parse_thinking_levels("unknown_level").is_err());
    }

    /// 模型角色字段的路径判定与伪值：主模型没有伪值，explore 只能沿用主模型，plan 两个都有。
    #[test]
    fn model_role_fields_carry_their_own_specials() {
        let path = |segments: [&str; 3]| {
            segments
                .iter()
                .map(|part| (*part).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(model_role(&path(["models", "roles", "plan"])), Some("plan"));
        assert_eq!(
            model_role(&path(["models", "roles", "default"])),
            Some("default")
        );
        assert_eq!(
            model_role(&path(["providers", "test", "models"])),
            None,
            "只有模型角色字段走共用选择器"
        );
        assert!(role_specials("default").is_empty());
        assert_eq!(role_specials("explore"), [("inherit", "沿用主模型")]);
        assert_eq!(
            role_specials("plan"),
            [("inherit", "沿用主模型"), ("none", "沿用当前会话模型")]
        );
    }

    #[test]
    fn test_display_value_features() {
        let field_input = ConfigField {
            path: vec![
                "providers".into(),
                "openai".into(),
                "models".into(),
                "gpt-4o".into(),
                "input".into(),
            ],
            tab: "模型".into(),
            label: "输入模态".into(),
            description: "模型支持接收的内容类型".into(),
            kind: FieldKind::Choice(vec![]),
            advanced: false,
        };
        assert_eq!(
            render::display_value(&json!(["text"]), &field_input),
            "‹ 仅文本 (text) ›"
        );
        assert_eq!(
            render::display_value(&json!(["text", "image"]), &field_input),
            "‹ 文本与图片 / 多模态 (text, image) ›"
        );

        let field_thinking = ConfigField {
            path: vec![
                "providers".into(),
                "openai".into(),
                "models".into(),
                "o1".into(),
                "thinking_levels".into(),
            ],
            tab: "模型".into(),
            label: "可选思考等级".into(),
            description: "思考等级".into(),
            kind: FieldKind::Strings,
            advanced: false,
        };
        assert_eq!(
            render::display_value(&Value::Null, &field_thinking),
            "默认 (low, medium, high, xhigh, max)"
        );
        assert_eq!(
            render::display_value(&json!(["low", "high"]), &field_thinking),
            "low、high"
        );
    }
}
