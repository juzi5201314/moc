//! 供应商与模型工作区：显式供应商上下文、连接表单与页内模型发现。

use super::*;

#[cfg(test)]
mod tests;

pub const TAB: usize = 1;
pub const APIS: [(&str, &str); 3] = [
    ("openai_chat", "OpenAI Chat"),
    ("openai_responses", "OpenAI Responses"),
    ("anthropic_messages", "Anthropic Messages"),
];

#[derive(Default)]
pub struct Providers {
    pub selected: Option<String>,
    pub sidebar: bool,
    pub cursor: usize,
    pub offset: usize,
    /// 侧栏的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    pub more: bool,
    pub draft: Option<ProviderDraft>,
    pub discovery: Option<Discovery>,
    pub(super) generation: u64,
}

pub struct ProviderDraft {
    pub id: TextEditor,
    pub api: usize,
    pub url: TextEditor,
    pub key: TextEditor,
    /// 四个字段与两个提交按钮，共六个控件。
    pub focus: usize,
    pub offset: usize,
    /// 表单的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    pub pending: Option<Snapshot>,
    pub discover: bool,
}

impl Default for ProviderDraft {
    fn default() -> Self {
        Self {
            id: TextEditor::new(FieldKind::Text, String::new()),
            api: 0,
            url: TextEditor::new(FieldKind::Text, String::new()),
            key: TextEditor::new(FieldKind::Secret, String::new()),
            focus: 0,
            offset: 0,
            scroll: Default::default(),
            pending: None,
            discover: true,
        }
    }
}

impl ProviderDraft {
    pub fn input(&mut self) -> Option<&mut TextEditor> {
        match self.focus {
            0 => Some(&mut self.id),
            2 => Some(&mut self.url),
            3 => Some(&mut self.key),
            _ => None,
        }
    }

    fn value(&self, panel: &SettingsPanel) -> anyhow::Result<(String, Value)> {
        let id = self.id.input.text.trim();
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "供应商标识只能使用字母、数字、- 和 _"
        );
        anyhow::ensure!(
            panel.effective["providers"].get(id).is_none(),
            "供应商 {id} 已存在"
        );
        let url = self.url.input.text.trim();
        anyhow::ensure!(!url.is_empty(), "请填写服务地址");
        Ok((
            id.into(),
            json!({
                "api": APIS[self.api].0, "base_url": url, "api_key": self.key.input.text,
                "models": {}
            }),
        ))
    }
}

pub struct Discovery {
    pub generation: u64,
    pub rows: Vec<CatalogModel>,
    pub checked: BTreeSet<String>,
    pub query: String,
    pub error: Option<String>,
    pub pending: Option<tokio::task::AbortHandle>,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderAction {
    New,
    More,
    Fetch,
    Manual,
    Search,
    SelectAll,
    AddPicked,
    CloseDiscovery,
    Candidate(usize),
    Remove,
}

impl SettingsPanel {
    pub fn provider_page(&self) -> bool {
        self.tab == TAB && self.filter.is_empty()
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.effective
            .get("providers")
            .and_then(Value::as_object)
            .map(|values| values.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn sync_provider(&mut self) {
        let ids = self.provider_ids();
        if !self
            .providers
            .selected
            .as_ref()
            .is_some_and(|id| ids.contains(id))
        {
            self.providers.selected = ids.first().cloned();
            self.providers.discovery = None;
        }
        self.providers.cursor = if self.providers.draft.is_some() {
            ids.len()
        } else {
            self.providers
                .selected
                .as_ref()
                .and_then(|id| ids.iter().position(|item| item == id))
                .unwrap_or(ids.len())
        };
        self.providers.scroll.reveal_selection = true;
    }

    pub fn choose_provider(&mut self, index: usize) {
        if self
            .providers
            .draft
            .as_ref()
            .is_some_and(|draft| draft.pending.is_some())
        {
            return;
        }
        let ids = self.provider_ids();
        self.providers.cursor = index.min(ids.len());
        self.providers.scroll.reveal_selection = true;
        if let Some(id) = ids.get(index) {
            if self.providers.selected.as_ref() != Some(id) {
                self.providers.discovery = None;
            }
            self.providers.selected = Some(id.clone());
            self.providers.draft = None;
            self.selected = 0;
            self.offset = 0;
            self.scroll.reveal_selection = true;
            self.error = None;
            self.rebuild_rows();
        } else {
            self.new_provider();
        }
    }

    pub fn new_provider(&mut self) {
        self.providers
            .draft
            .get_or_insert_with(ProviderDraft::default);
        self.providers.sidebar = false;
        self.providers.cursor = self.provider_ids().len();
        self.providers.scroll.reveal_selection = true;
        self.edit = None;
        self.error = None;
    }

    /// 所有供应商（包括零模型）来自配置对象，详情字段只取明确选中的供应商。
    pub fn provider_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(id) = &self.providers.selected else {
            return rows;
        };
        let action = |rows: &mut Vec<Row>, action| {
            rows.push(Row {
                depth: 0,
                kind: RowKind::Action(action),
            })
        };
        let connection = self.fields.iter().enumerate().filter(|(_, field)| {
            field.path.first().is_some_and(|p| p == "providers")
                && field.path.get(1) == Some(id)
                && field.path.get(2).is_some_and(|p| p != "models")
        });
        for (index, field) in connection {
            let primary = matches!(field.path[2].as_str(), "api" | "base_url" | "api_key");
            if primary || self.providers.more {
                rows.push(Row {
                    depth: 0,
                    kind: RowKind::Field(index),
                });
            }
        }
        action(&mut rows, ProviderAction::More);
        action(&mut rows, ProviderAction::Fetch);
        action(&mut rows, ProviderAction::Manual);
        if let Some(discovery) = &self.providers.discovery {
            action(&mut rows, ProviderAction::Search);
            action(&mut rows, ProviderAction::AddPicked);
            action(&mut rows, ProviderAction::SelectAll);
            for index in self.visible_candidates() {
                action(&mut rows, ProviderAction::Candidate(index));
            }
            if discovery.pending.is_none() {
                action(&mut rows, ProviderAction::CloseDiscovery);
            }
        }
        if let Some(models) = self.effective["providers"][id]
            .get("models")
            .and_then(Value::as_object)
        {
            for model in models.keys() {
                let path = vec![
                    "providers".into(),
                    id.clone(),
                    "models".into(),
                    model.clone(),
                ];
                let key = format!("providers/{id}/models/{model}");
                let expanded = self.is_expanded(&key, 1);
                rows.push(Row {
                    depth: 1,
                    kind: RowKind::Group(Group {
                        key,
                        title: model.clone(),
                        detail: self.model_summary(id, model),
                        grouping: path.clone(),
                        path: Some(path.clone()),
                    }),
                });
                if expanded {
                    for (index, field) in self.fields.iter().enumerate() {
                        if field.path.starts_with(&path) && (!field.advanced || self.advanced) {
                            rows.push(Row {
                                depth: 2,
                                kind: RowKind::Field(index),
                            });
                        }
                    }
                }
            }
        }
        rows
    }

    pub fn visible_candidates(&self) -> Vec<usize> {
        let Some(discovery) = &self.providers.discovery else {
            return Vec::new();
        };
        let query = discovery.query.trim().to_lowercase();
        discovery
            .rows
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn candidate_added(&self, index: usize) -> bool {
        let Some(discovery) = &self.providers.discovery else {
            return false;
        };
        let Some(model) = discovery.rows.get(index) else {
            return false;
        };
        self.providers.selected.as_ref().is_some_and(|id| {
            self.effective["providers"][id]["models"]
                .get(&model.id)
                .is_some()
        })
    }

    pub fn provider_action_text(&self, action: &ProviderAction) -> String {
        match action {
            ProviderAction::New => "+ 添加供应商".into(),
            ProviderAction::More => format!(
                "{} 更多连接选项",
                if self.providers.more { "▾" } else { "▸" }
            ),
            ProviderAction::Fetch => match &self.providers.discovery {
                Some(discovery) if discovery.pending.is_some() => "正在获取模型…".into(),
                Some(discovery) if discovery.error.is_some() => "重新获取模型".into(),
                _ => "从供应商获取模型".into(),
            },
            ProviderAction::Manual => "+ 手动添加模型 ID".into(),
            ProviderAction::Search => format!(
                "搜索模型：{}",
                self.providers
                    .discovery
                    .as_ref()
                    .map_or("", |d| d.query.as_str())
            ),
            ProviderAction::SelectAll => "全选 / 取消当前搜索结果".into(),
            ProviderAction::AddPicked => format!(
                "添加勾选的 {} 个模型",
                self.providers
                    .discovery
                    .as_ref()
                    .map_or(0, |d| d.checked.len())
            ),
            ProviderAction::CloseDiscovery => "收起获取结果".into(),
            ProviderAction::Remove => "删除供应商".into(),
            ProviderAction::Candidate(index) => {
                let discovery = self.providers.discovery.as_ref().expect("候选属于发现列表");
                let model = &discovery.rows[*index];
                let mark = if self.candidate_added(*index) {
                    "已添加 "
                } else if discovery.checked.contains(&model.id) {
                    "☑      "
                } else {
                    "☐      "
                };
                if model.id == model.name {
                    format!("{mark}{}", model.id)
                } else {
                    format!("{mark}{} · {}", model.id, model.name)
                }
            }
        }
    }

    /// 侧栏里的高亮行：导航态看光标，详情态看已选供应商。
    pub fn provider_selected(&self, ids: &[String], index: usize) -> bool {
        if self.providers.sidebar {
            index == self.providers.cursor
        } else {
            ids.get(index) == self.providers.selected.as_ref() && self.providers.draft.is_none()
        }
    }

    pub fn select_action(&mut self, action: ProviderAction) {
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| matches!(&row.kind, RowKind::Action(a) if *a == action))
        {
            self.selected = index;
            self.scroll.reveal_selection = true;
        }
    }

    pub fn provider_action(
        &mut self,
        action: ProviderAction,
        app: &mut App,
        connection: &EmbeddedConnection,
    ) {
        match action {
            ProviderAction::New => self.new_provider(),
            ProviderAction::More => {
                self.providers.more = !self.providers.more;
                self.rebuild_rows();
                self.select_action(ProviderAction::More);
            }
            ProviderAction::Fetch => self.fetch(connection, &app.events),
            ProviderAction::Manual => {
                if let Some(id) = self.providers.selected.clone() {
                    self.select_action(ProviderAction::Manual);
                    self.begin_named(
                        EditTarget::Model(id.clone()),
                        &format!("为 {id} 添加模型"),
                        "模型 ID 原样保存，可包含 /",
                        "",
                    );
                    self.edit.as_mut().expect("刚打开输入").popup = false;
                }
            }
            ProviderAction::Search => {
                let query = self
                    .providers
                    .discovery
                    .as_ref()
                    .map(|d| d.query.clone())
                    .unwrap_or_default();
                self.select_action(ProviderAction::Search);
                self.begin_named(
                    EditTarget::ModelFilter,
                    "搜索模型",
                    "按模型 ID 或名称过滤",
                    &query,
                );
                self.edit.as_mut().expect("刚打开输入").popup = false;
            }
            ProviderAction::Candidate(index) => {
                if self.candidate_added(index) {
                    return;
                }
                if let Some(discovery) = &mut self.providers.discovery
                    && let Some(model) = discovery.rows.get(index)
                    && !discovery.checked.remove(&model.id)
                {
                    discovery.checked.insert(model.id.clone());
                }
            }
            ProviderAction::SelectAll => {
                let candidates: Vec<usize> = self
                    .visible_candidates()
                    .into_iter()
                    .filter(|index| !self.candidate_added(*index))
                    .collect();
                if let Some(discovery) = &mut self.providers.discovery {
                    let all = candidates
                        .iter()
                        .all(|index| discovery.checked.contains(&discovery.rows[*index].id));
                    for index in candidates {
                        let id = discovery.rows[index].id.clone();
                        if all {
                            discovery.checked.remove(&id);
                        } else {
                            discovery.checked.insert(id);
                        }
                    }
                }
            }
            ProviderAction::AddPicked => {
                let Some(provider) = self.providers.selected.clone() else {
                    return;
                };
                let ids: Vec<String> = self
                    .providers
                    .discovery
                    .as_ref()
                    .map(|d| {
                        d.checked
                            .iter()
                            .filter(|id| {
                                self.effective["providers"][&provider]["models"]
                                    .get(id.as_str())
                                    .is_none()
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    self.error = Some("请先勾选需要添加的模型".into());
                    return;
                }
                let result = self.commit("添加勾选模型", app, connection, |panel| {
                    for id in &ids {
                        panel.insert_model(&provider, id)?;
                    }
                    Ok(Store::Core)
                });
                if result.is_ok() {
                    if let Some(discovery) = &mut self.providers.discovery {
                        discovery.checked.clear();
                    }
                    self.rebuild_rows();
                    self.open_model(&provider, &ids[0]);
                }
            }
            ProviderAction::CloseDiscovery => {
                self.providers.discovery = None;
                self.rebuild_rows();
                self.select_action(ProviderAction::Fetch);
            }
            ProviderAction::Remove => {
                if let Some(id) = &self.providers.selected {
                    self.view = View::Confirm {
                        text: format!("删除供应商“{id}”及其在当前配置中的模型？Ctrl+Z 可以撤回。"),
                        action: ConfirmAction::Remove(vec!["providers".into(), id.clone()]),
                    };
                }
            }
        }
    }

    pub fn fetch(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) {
        let Some(id) = self.providers.selected.clone() else {
            self.error = Some("请先添加供应商并填写连接参数".into());
            return;
        };
        let provider = self.effective["providers"][&id].clone();
        if !provider["base_url"]
            .as_str()
            .is_some_and(|url| !url.trim().is_empty())
        {
            self.error = Some("请先填写服务地址".into());
            return;
        }
        self.providers.generation += 1;
        let generation = self.providers.generation;
        let client = connection.client.clone();
        let events = events.clone();
        let panel = self.id;
        let task = tokio::spawn(async move {
            let result = client
                .call::<ConfigManage>(ConfigAction::Models { id, provider })
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SettingsResponse {
                panel,
                job: Job::Models(generation),
                result,
            });
        });
        self.providers.discovery = Some(Discovery {
            generation,
            rows: Vec::new(),
            checked: BTreeSet::new(),
            query: String::new(),
            error: None,
            pending: Some(task.abort_handle()),
        });
        self.error = None;
        self.rebuild_rows();
    }

    pub fn models_response(&mut self, generation: u64, result: anyhow::Result<ConfigReply>) {
        let Some(discovery) = &mut self.providers.discovery else {
            return;
        };
        if discovery.generation != generation {
            return;
        }
        discovery.pending = None;
        match result {
            Ok(ConfigReply::Models(rows)) => discovery.rows = rows,
            Err(error) => discovery.error = Some(error.to_string()),
            Ok(_) => discovery.error = Some("获取模型返回了非模型响应".into()),
        }
        self.rebuild_rows();
    }

    pub fn open_model(&mut self, provider: &str, model: &str) {
        self.expanded
            .insert(format!("providers/{provider}/models/{model}"));
        self.rebuild_rows();
        self.select_path(&[
            "providers".into(),
            provider.into(),
            "models".into(),
            model.into(),
        ]);
    }

    fn submit_provider(&mut self, app: &mut App, connection: &EmbeddedConnection, discover: bool) {
        if app.saving_config() {
            self.error = Some("正在保存配置，请稍候".into());
            return;
        }
        let Some(draft) = &self.providers.draft else {
            return;
        };
        let (id, value) = match draft.value(self) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        let Some(data) = &self.data else { return };
        let mut document = data.document.clone();
        set(&mut document, &["providers".into(), id.clone()], value);
        let snapshot = self.snapshot(&format!("添加供应商 {id}"));
        let write = ConfigWrite {
            scope: self.scope,
            cwd: self.cwd.clone(),
            document,
            revision: data.revision.clone(),
        };
        let draft = self.providers.draft.as_mut().expect("表单存在");
        draft.pending = Some(snapshot);
        draft.discover = discover;
        self.error = None;
        app.write_config(connection, write);
    }

    pub fn provider_saved(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) {
        if !self
            .providers
            .draft
            .as_ref()
            .is_some_and(|draft| draft.pending.is_some())
        {
            return;
        }
        let mut draft = self.providers.draft.take().expect("新建供应商已保存");
        self.history
            .put(draft.pending.take().expect("提交前快照"), true);
        self.providers.selected = Some(draft.id.input.text.trim().into());
        self.providers.sidebar = false;
        self.selected = 0;
        self.offset = 0;
        self.scroll.reveal_selection = true;
        self.sync_provider();
        self.rebuild_rows();
        if draft.discover {
            self.fetch(connection, events);
        }
    }
}

/// 专属页面先处理焦点与表单输入，其余字段编辑仍走共用控件。
pub fn key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
    if panel.data.is_none() {
        return false;
    }
    if panel.providers.draft.is_some() {
        if panel
            .providers
            .draft
            .as_ref()
            .is_some_and(|draft| draft.pending.is_some())
        {
            return true;
        }
        if code == KeyCode::Esc {
            panel.providers.draft = None;
            panel.error = None;
            return true;
        }
        let draft = panel.providers.draft.as_mut().expect("表单存在");
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('s') {
            panel.submit_provider(app, connection, true);
            return true;
        }
        let focus_before = draft.focus;
        match code {
            KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
                draft.focus = (draft.focus + 5) % 6
            }
            KeyCode::Tab | KeyCode::Down => draft.focus = (draft.focus + 1) % 6,
            KeyCode::BackTab | KeyCode::Up => draft.focus = (draft.focus + 5) % 6,
            KeyCode::Enter if draft.focus >= 4 => {
                let discover = draft.focus == 4;
                panel.submit_provider(app, connection, discover);
            }
            KeyCode::Enter if draft.focus != 1 => draft.focus += 1,
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right if draft.focus == 1 => {
                draft.api = (draft.api + 1) % APIS.len()
            }
            KeyCode::Left if draft.focus == 1 => {
                draft.api = (draft.api + APIS.len() - 1) % APIS.len()
            }
            KeyCode::F(6) => draft.key.reveal = !draft.key.reveal,
            _ => {
                if let Some(input) = draft.input() {
                    crate::widgets::prompt::edit_input(&mut input.input, mods, code);
                }
            }
        }
        if let Some(draft) = panel.providers.draft.as_mut()
            && draft.focus != focus_before
        {
            draft.scroll.reveal_selection = true;
        }
        return true;
    }
    if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
        let previous = code == KeyCode::BackTab || mods.contains(KeyModifiers::SHIFT);
        let selected_before = panel.selected;
        let sidebar_before = panel.providers.sidebar;
        if panel.providers.sidebar {
            panel.choose_provider(panel.providers.cursor);
            panel.providers.sidebar = false;
        } else if previous && panel.selected > 0 {
            panel.selected -= 1;
        } else if !previous && panel.selected + 1 < panel.rows.len() {
            panel.selected += 1;
        } else {
            panel.providers.sidebar = true;
        }
        if panel.selected != selected_before {
            panel.scroll.reveal_selection = true;
        }
        if !sidebar_before && panel.providers.sidebar {
            panel.providers.scroll.reveal_selection = true;
        }
        return true;
    }
    if code == KeyCode::Esc && panel.providers.sidebar {
        panel.providers.sidebar = false;
        return true;
    }
    if code == KeyCode::Left && !matches!(panel.row(), Some(RowKind::Group(_))) {
        panel.providers.sidebar = true;
        panel.providers.scroll.reveal_selection = true;
        return true;
    }
    if code == KeyCode::Char('x') && mods == KeyModifiers::CONTROL {
        if panel.providers.sidebar {
            panel.choose_provider(panel.providers.cursor);
            panel.providers.sidebar = false;
        }
        if panel.providers.draft.is_none() {
            panel.provider_action(ProviderAction::Remove, app, connection);
        }
        return true;
    }
    if code == KeyCode::Char('n') && mods == KeyModifiers::NONE {
        panel.new_provider();
        return true;
    }
    let action = match code {
        KeyCode::Char('a') if mods == KeyModifiers::NONE => Some(ProviderAction::Manual),
        KeyCode::Char('f') if mods == KeyModifiers::NONE => Some(ProviderAction::Fetch),
        KeyCode::Char('/') if mods == KeyModifiers::NONE && panel.providers.discovery.is_some() => {
            Some(ProviderAction::Search)
        }
        _ => None,
    };
    if let Some(action) = action {
        if panel.providers.sidebar {
            panel.choose_provider(panel.providers.cursor);
            panel.providers.sidebar = false;
        }
        if panel.providers.draft.is_none() {
            panel.provider_action(action, app, connection);
        }
        return true;
    }
    if panel.providers.sidebar {
        let last = panel.provider_ids().len();
        let cursor_before = panel.providers.cursor;
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                panel.providers.cursor = panel.providers.cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                panel.providers.cursor = (panel.providers.cursor + 1).min(last)
            }
            KeyCode::Home => panel.providers.cursor = 0,
            KeyCode::End => panel.providers.cursor = last,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                panel.choose_provider(panel.providers.cursor);
                panel.providers.sidebar = false;
            }
            _ => return false,
        }
        if panel.providers.cursor != cursor_before {
            panel.providers.scroll.reveal_selection = true;
        }
        return true;
    }
    match code {
        KeyCode::Enter | KeyCode::Char(' ') => {
            let Some(RowKind::Action(action)) = panel.row() else {
                return false;
            };
            panel.provider_action(action.clone(), app, connection);
        }
        _ => return false,
    }
    true
}
