//! /config：普通字段表单、模型候选勾选、维护预览与工作区管理。

mod forms;
mod render;

use crate::{
    app::App,
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
pub const TABS: &[&str] = &[
    "模型",
    "供应商",
    "来源",
    "工具",
    "上下文",
    "会话",
    "Skills",
    "MCP",
    "OpenViking",
    "TUI",
    "高级",
];

pub struct SettingsPanel {
    pub id: u64,
    pub cwd: String,
    pub scope: ConfigScope,
    pub data: Option<SettingsDocument>,
    pub tui: Value,
    pub tui_original: Value,
    pub core_original: Value,
    pub effective: Value,
    pub fields: Vec<ConfigField>,
    pub visible: Vec<usize>,
    pub tab: usize,
    pub selected: usize,
    pub offset: usize,
    pub advanced: bool,
    pub filter: String,
    pub view: View,
    pub error: Option<String>,
    pub message: Option<String>,
    pub busy: bool,
    pub hits: Vec<(ratatui::layout::Rect, Hit)>,
    pub pending: Option<tokio::task::AbortHandle>,
}

pub enum View {
    Fields,
    Edit {
        target: EditTarget,
        kind: FieldKind,
        input: PromptState,
        reveal: bool,
    },
    Models {
        provider: String,
        rows: Vec<CatalogModel>,
        checked: BTreeSet<String>,
        selected: usize,
        offset: usize,
    },
    Workspaces {
        rows: Vec<WorkspaceBinding>,
        selected: usize,
        offset: usize,
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
    Close,
    Remove(Vec<String>),
    Run(ConfigAction),
}
#[derive(Clone)]
pub enum Hit {
    Tab(usize),
    Row(usize),
    Edit,
    Save,
    Add,
    Fetch,
    Advanced,
    Scope,
    Back,
    Workspace,
    Maintenance,
    Delete,
    Apply,
    Check(usize),
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
        tui_original: tui,
        core_original: json!({}),
        effective: json!({}),
        fields: Vec::new(),
        visible: Vec::new(),
        tab: if app.session.is_none() { 1 } else { 0 },
        selected: 0,
        offset: 0,
        advanced: false,
        filter: String::new(),
        view: View::Fields,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
    };
    panel.request(
        connection,
        &app.events,
        ConfigAction::Read {
            scope: panel.scope,
            cwd,
        },
        false,
    );
    app.settings = Some(panel);
    app.completion = None;
}

impl SettingsPanel {
    pub fn dirty(&self) -> bool {
        self.data
            .as_ref()
            .is_some_and(|data| data.document != self.core_original)
            || self.tui != self.tui_original
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
        self.filter_fields();
    }
    fn filter_fields(&mut self) {
        let filter = self.filter.to_lowercase();
        self.visible = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| {
                let tab = if field.advanced {
                    "高级"
                } else {
                    field.tab.as_str()
                };
                (tab == TABS[self.tab] || (self.advanced && field.tab == TABS[self.tab]))
                    && (filter.is_empty()
                        || field.path.join(".").to_lowercase().contains(&filter)
                        || field.label.to_lowercase().contains(&filter))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }
    pub fn field(&self) -> Option<&ConfigField> {
        self.visible
            .get(self.selected)
            .map(|index| &self.fields[*index])
    }
    pub fn value(&self, path: &[String]) -> Option<&Value> {
        get(&self.effective, path)
    }
    fn change(&mut self, path: &[String], value: Value) {
        if path.first().is_some_and(|part| part == "tui") {
            set(&mut self.tui, &path[1..], value);
        } else if let Some(data) = &mut self.data {
            set(&mut data.document, path, value);
        }
        self.error = None;
        self.rebuild();
    }
    fn request(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
        action: ConfigAction,
        saved: bool,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        self.message = Some(
            if saved {
                "正在保存；运行中的轮次结束后应用"
            } else {
                "正在加载"
            }
            .into(),
        );
        let client = connection.client.clone();
        let events = events.clone();
        let panel = self.id;
        let task = tokio::spawn(async move {
            let result = client
                .call::<ConfigManage>(action)
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SettingsResponse {
                panel,
                result,
                saved,
            });
        });
        self.pending = Some(task.abort_handle());
    }
    fn select_tab(&mut self, index: usize) {
        self.tab = index;
        self.selected = 0;
        self.offset = 0;
        self.view = View::Fields;
        self.filter_fields();
    }
    fn begin_edit(&mut self) {
        let Some(field) = self.field().cloned() else {
            return;
        };
        let value = self.value(&field.path).cloned().unwrap_or(Value::Null);
        let mut kind = field.kind;
        if field.path.starts_with(&["models".into(), "roles".into()]) {
            let mut choices = Vec::new();
            if field.path.last().is_some_and(|name| name != "default") {
                choices.push("inherit".into());
            }
            if let Some(providers) = self.effective.get("providers").and_then(Value::as_object) {
                for (provider, value) in providers {
                    if let Some(models) = value.get("models").and_then(Value::as_object) {
                        choices.extend(models.keys().map(|model| format!("{provider}/{model}")));
                    }
                }
            }
            if let Some(current) = value.as_str().filter(|current| !current.is_empty())
                && !choices.iter().any(|choice| choice == current)
            {
                choices.push(current.into());
            }
            kind = FieldKind::Choice(choices);
        }
        let text = forms::format_value(&value, &kind);
        self.view = View::Edit {
            target: EditTarget::Field(field.path),
            kind,
            input: PromptState::from_text(text),
            reveal: false,
        };
    }
    fn begin_named(&mut self, target: EditTarget) {
        self.view = View::Edit {
            target,
            kind: FieldKind::Text,
            input: PromptState::default(),
            reveal: false,
        };
    }
    fn provider(&self) -> Option<String> {
        self.field()
            .filter(|field| field.path.first().is_some_and(|part| part == "providers"))
            .and_then(|field| field.path.get(1))
            .cloned()
            .or_else(|| {
                self.effective
                    .get("providers")?
                    .as_object()?
                    .keys()
                    .next()
                    .cloned()
            })
    }
    fn add(&mut self) {
        match TABS[self.tab] {
            "供应商" => self.begin_named(EditTarget::Provider),
            "模型" => {
                if let Some(provider) = self.provider() {
                    self.begin_named(EditTarget::Model(provider));
                } else {
                    self.error = Some("请先在供应商选项卡添加供应商".into());
                }
            }
            "MCP" => self.begin_named(EditTarget::Mcp),
            _ => {
                self.error =
                    Some("可在供应商、模型、MCP 选项卡添加条目；模型默认模式使用 Ctrl+D".into())
            }
        }
    }
    fn fetch(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(provider) = self.provider() else {
            self.error = Some("请先添加并选中一个供应商，填写 API 地址与密钥".into());
            return;
        };
        let value = self.effective["providers"][&provider].clone();
        self.view = View::Models {
            provider,
            rows: Vec::new(),
            checked: BTreeSet::new(),
            selected: 0,
            offset: 0,
        };
        self.request(
            connection,
            events,
            ConfigAction::Models { provider: value },
            false,
        );
    }
    fn add_model(&mut self, provider: &str, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!id.trim().is_empty(), "模型 ID 不能为空");
        let path = vec![
            "providers".into(),
            provider.into(),
            "models".into(),
            id.into(),
        ];
        anyhow::ensure!(self.value(&path).is_none(), "模型 {provider}/{id} 已存在");
        self.change(&path, json!({}));
        let primary = self
            .effective
            .pointer("/models/roles/default")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if primary.is_empty() {
            self.change(
                &["models".into(), "roles".into(), "default".into()],
                json!(format!("{provider}/{id}")),
            );
        }
        Ok(())
    }
}

pub fn response(
    app: &mut App,
    connection: &EmbeddedConnection,
    id: u64,
    result: anyhow::Result<ConfigReply>,
    saved: bool,
) {
    let Some(panel) = app.settings.as_mut().filter(|panel| panel.id == id) else {
        return;
    };
    panel.busy = false;
    panel.pending = None;
    panel.message = None;
    match result {
        Err(error) => panel.error = Some(error.to_string()),
        Ok(ConfigReply::Settings(data)) => {
            panel.core_original = data.document.clone();
            panel.data = Some(data);
            panel.rebuild();
            if saved {
                panel.message = Some("配置已保存并应用；会话目录变更重启生效".into());
            }
        }
        Ok(ConfigReply::Models(rows)) => {
            if let View::Models { rows: current, .. } = &mut panel.view {
                *current = rows;
            }
        }
        Ok(ConfigReply::Workspaces(rows)) => {
            panel.view = View::Workspaces {
                rows,
                selected: 0,
                offset: 0,
            }
        }
        Ok(ConfigReply::Maintenance(report)) => panel.view = View::Maintenance(report),
        Ok(ConfigReply::Updated(message)) => {
            panel.message = Some(message);
            panel.view = View::Fields;
        }
    }
    if saved && panel.error.is_none() {
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
                    let _ = events.send(AppEvent::Notice(format!("刷新模型列表失败：{error}")));
                }
            }
        });
    }
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
pub fn paste(app: &mut App, text: &str) {
    if let Some(panel) = &mut app.settings
        && let View::Edit { input, .. } = &mut panel.view
    {
        input.insert_str_raw(text);
    }
}
pub fn mouse(app: &mut App, connection: &EmbeddedConnection, event: MouseEvent) {
    let Some(panel) = &mut app.settings else {
        return;
    };
    if event.kind == MouseEventKind::ScrollDown {
        panel.selected = panel
            .selected
            .saturating_add(3)
            .min(panel.visible.len().saturating_sub(1));
        return;
    }
    if event.kind == MouseEventKind::ScrollUp {
        panel.selected = panel.selected.saturating_sub(3);
        return;
    }
    if event.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    let Some(hit) = panel
        .hits
        .iter()
        .find(|(area, _)| area.contains((event.column, event.row).into()))
        .map(|(_, hit)| hit.clone())
    else {
        return;
    };
    match hit {
        Hit::Tab(index) => panel.select_tab(index),
        Hit::Row(index) => {
            panel.selected = index;
            panel.begin_edit();
        }
        Hit::Check(index) => match &mut panel.view {
            View::Models {
                rows,
                checked,
                selected,
                ..
            } => {
                *selected = index;
                if let Some(row) = rows.get(index)
                    && !checked.remove(&row.id)
                {
                    checked.insert(row.id.clone());
                }
            }
            View::Workspaces { selected, .. } => *selected = index,
            _ => {}
        },
        hit => {
            let key = match hit {
                Hit::Edit => KeyCode::Enter,
                Hit::Save => KeyCode::Char('s'),
                Hit::Add => KeyCode::Char('a'),
                Hit::Fetch => KeyCode::Char('f'),
                Hit::Advanced => KeyCode::F(4),
                Hit::Scope => KeyCode::F(2),
                Hit::Back => KeyCode::Esc,
                Hit::Workspace => KeyCode::Char('w'),
                Hit::Maintenance => KeyCode::Char('g'),
                Hit::Delete => KeyCode::Delete,
                Hit::Apply => KeyCode::Enter,
                _ => return,
            };
            let modifiers = if matches!(hit, Hit::Save) {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            handle_key(app, connection, modifiers, key);
        }
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
