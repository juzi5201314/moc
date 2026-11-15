//! 供应商与模型工作区：显式供应商上下文、连接表单与页内模型发现。

use super::*;

#[cfg(test)]
mod tests;

pub const TAB: usize = 1;

fn request_parameter(field: &ConfigField) -> bool {
    field
        .path
        .iter()
        .position(|part| matches!(part.as_str(), "extra_body" | "headers"))
        .is_some_and(|index| index + 1 < field.path.len())
}

fn is_provider_connection_field(id: &str, field: &ConfigField) -> bool {
    field.path.first().is_some_and(|p| p == "providers")
        && field.path.get(1).is_some_and(|p| p == id)
        && field.path.get(2).is_some_and(|p| p != "models")
}

fn parameter_title(path: &[String]) -> String {
    match path.last().map_or("", String::as_str) {
        "headers" => t!("tui.param.headers"),
        "extra_body" => t!("tui.param.extra_body"),
        "reasoning" => t!("tui.param.reasoning"),
        "thinking" => t!("tui.param.thinking"),
        "text" => t!("tui.param.text"),
        "format" | "json_schema" => t!("tui.param.format"),
        "response_format" => t!("tui.param.response_format"),
        "output_config" => t!("tui.param.output_config"),
        "metadata" => t!("tui.param.metadata"),
        "prediction" => t!("tui.param.prediction"),
        "prompt" => t!("tui.param.prompt"),
        "prompt_cache_options" => t!("tui.param.prompt_cache_options"),
        "cache_control" => t!("tui.param.cache_control"),
        "stream_options" => t!("tui.param.stream_options"),
        "tool_choice" => t!("tui.param.tool_choice"),
        "moderation" => t!("tui.param.moderation"),
        "input" => t!("tui.param.input"),
        "output" => t!("tui.param.output"),
        "image" => t!("tui.param.image"),
        "access_programs" => t!("tui.param.access_programs"),
        "diagnostics" => t!("tui.param.diagnostics"),
        name => return name.to_owned(),
    }
    .into_owned()
}
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
            "{}", t!("tui.err.provider_id_invalid")
        );
        anyhow::ensure!(
            panel.effective["providers"].get(id).is_none(),
            "{}", t!("tui.err.provider_exists", id = id)
        );
        let url = self.url.input.text.trim();
        anyhow::ensure!(!url.is_empty(), "{}", t!("tui.err.url_required"));
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
    pub cursor: usize,
    pub offset: usize,
    pub scroll: ModelScroll,
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
        let mut secondary = Vec::new();
        let mut provider_params = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            if is_provider_connection_field(id, field) {
                if request_parameter(field) {
                    provider_params.push(index);
                } else if matches!(field.path[2].as_str(), "api" | "base_url" | "api_key") {
                    rows.push(Row {
                        depth: 0,
                        kind: RowKind::Field(index),
                    });
                } else {
                    secondary.push(index);
                }
            }
        }
        action(&mut rows, ProviderAction::More);
        if self.providers.more {
            for index in secondary {
                rows.push(Row {
                    depth: 1,
                    kind: RowKind::Field(index),
                });
            }
            self.parameter_rows(&mut rows, provider_params, 1);
        }
        action(&mut rows, ProviderAction::Fetch);
        action(&mut rows, ProviderAction::Manual);
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
                        key: key.clone(),
                        title: model.clone(),
                        detail: self.model_summary(id, model),
                        grouping: path.clone(),
                        path: Some(path.clone()),
                    }),
                });
                if expanded {
                    let mut adv_fields = Vec::new();
                    let mut adv_params = Vec::new();
                    for (index, field) in self.fields.iter().enumerate() {
                        if !field.path.starts_with(&path) {
                            continue;
                        }
                        if request_parameter(field) {
                            adv_params.push(index);
                        } else if field.advanced {
                            adv_fields.push(index);
                        } else {
                            rows.push(Row {
                                depth: 2,
                                kind: RowKind::Field(index),
                            });
                        }
                    }
                    let adv_count = adv_fields.len() + adv_params.len();
                    if adv_count > 0 {
                        let adv_key = format!("{key}/__advanced");
                        let mut adv_grouping = path.clone();
                        adv_grouping.push("__advanced".into());
                        let adv_expanded = self.is_expanded(&adv_key, 2);
                        rows.push(Row {
                            depth: 2,
                            kind: RowKind::Group(Group {
                                key: adv_key,
                                title: t!("tui.settings.advanced_title").into_owned(),
                                detail: t!("tui.settings.item_count", count = adv_count)
                                    .into_owned(),
                                grouping: adv_grouping,
                                path: None,
                            }),
                        });
                        if adv_expanded {
                            for index in adv_fields {
                                rows.push(Row {
                                    depth: 3,
                                    kind: RowKind::Field(index),
                                });
                            }
                            self.parameter_rows(&mut rows, adv_params, 3);
                        }
                    }
                }
            }
        }
        rows
    }

    pub(super) fn provider_more_count(&self) -> usize {
        let Some(id) = &self.providers.selected else {
            return 0;
        };
        self.fields
            .iter()
            .filter(|field| {
                is_provider_connection_field(id, field)
                    && (request_parameter(field)
                        || !matches!(field.path[2].as_str(), "api" | "base_url" | "api_key"))
            })
            .count()
    }

    /// 请求参数按原字段的父路径归组；与连接、高级协议兼容选项分开。
    fn parameter_rows(&self, rows: &mut Vec<Row>, indices: Vec<usize>, depth: u8) {
        let mut groups: Vec<(Vec<String>, Vec<usize>)> = Vec::new();
        for index in indices {
            let path = &self.fields[index].path;
            let parent = &path[..path.len() - 1];
            if let Some((_, indices)) = groups.iter_mut().find(|(path, _)| path == parent) {
                indices.push(index);
            } else {
                groups.push((parent.to_vec(), vec![index]));
            }
        }
        for (path, indices) in groups {
            let key = path.join("/");
            let expanded = self.is_expanded(&key, depth as usize);
            rows.push(Row {
                depth,
                kind: RowKind::Group(Group {
                    key,
                    title: parameter_title(&path),
                    detail: t!("tui.settings.item_count", count = indices.len())
                        .into_owned(),
                    grouping: path,
                    path: None,
                }),
            });
            if expanded {
                rows.extend(indices.into_iter().map(|index| Row {
                    depth: depth + 1,
                    kind: RowKind::Field(index),
                }));
            }
        }
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

    pub(super) fn candidate_added(&self, index: usize) -> bool {
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
            ProviderAction::New => t!("tui.providers.add").into_owned(),
            ProviderAction::More => t!(
                "tui.providers.more_fmt",
                mark = if self.providers.more { "▾" } else { "▸" }
            )
            .into_owned(),
            ProviderAction::Fetch => match &self.providers.discovery {
                Some(discovery) if discovery.pending.is_some() => {
                    t!("tui.providers.fetching_short").into_owned()
                }
                Some(discovery) if discovery.error.is_some() => {
                    t!("tui.providers.refetch").into_owned()
                }
                _ => t!("tui.providers.fetch").into_owned(),
            },
            ProviderAction::Manual => t!("tui.providers.manual").into_owned(),
            ProviderAction::Remove => t!("tui.providers.remove").into_owned(),
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

    pub fn toggle_candidate(&mut self, index: usize) {
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

    pub fn toggle_select_all_discovery(&mut self) {
        let candidates: Vec<usize> = self
            .visible_candidates()
            .into_iter()
            .filter(|index| !self.candidate_added(*index))
            .collect();
        if let Some(discovery) = &mut self.providers.discovery {
            let all = !candidates.is_empty()
                && candidates
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

    pub fn confirm_discovery(&mut self, app: &mut App, connection: &EmbeddedConnection) {
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
            self.error = Some(t!("tui.err.check_models").into_owned());
            return;
        }
        let result = self.commit(&t!("tui.snap.add_models"), app, connection, |panel| {
            for id in &ids {
                panel.insert_model(&provider, id)?;
            }
            Ok(Store::Core)
        });
        if result.is_ok() {
            self.providers.discovery = None;
            self.rebuild_rows();
            self.open_model(&provider, &ids[0]);
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
                        &t!("tui.providers.add_model_title", id = id),
                        &t!("tui.providers.model_id_note"),
                        "",
                    );
                    self.edit.as_mut().expect("刚打开输入").popup = false;
                }
            }
            ProviderAction::Remove => {
                if let Some(id) = &self.providers.selected {
                    self.view = View::Confirm {
                        text: t!("tui.providers.remove_confirm", id = id).into_owned(),
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
            self.error = Some(t!("tui.err.need_provider").into_owned());
            return;
        };
        let provider = self.effective["providers"][&id].clone();
        if !provider["base_url"]
            .as_str()
            .is_some_and(|url| !url.trim().is_empty())
        {
            self.error = Some(t!("tui.err.url_required").into_owned());
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
            cursor: 0,
            offset: 0,
            scroll: ModelScroll::default(),
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
            Ok(ConfigReply::Models(rows)) => {
                discovery.rows = rows;
                discovery.cursor = 0;
                discovery.offset = 0;
                discovery.scroll.reveal_selection = true;
            }
            Err(error) => discovery.error = Some(error.to_string()),
            Ok(_) => discovery.error = Some(t!("tui.err.bad_models_reply").into_owned()),
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
            self.error = Some(t!("tui.err.saving_wait").into_owned());
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
        let snapshot = self.snapshot(&t!("tui.snap.add_provider", id = id));
        let write = ConfigWrite {
            scope: ConfigScope::Global,
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
    if panel.providers.discovery.is_some() {
        discovery_key(panel, app, connection, mods, code);
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

fn discovery_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) {
    if code == KeyCode::Esc {
        panel.providers.discovery = None;
        panel.error = None;
        panel.rebuild_rows();
        panel.select_action(ProviderAction::Fetch);
        return;
    }
    if code == KeyCode::F(5) {
        panel.fetch(connection, &app.events);
        return;
    }
    if code == KeyCode::Char('a') && mods == KeyModifiers::CONTROL {
        panel.toggle_select_all_discovery();
        return;
    }
    let visible = panel.visible_candidates();
    let last = visible.len().saturating_sub(1);
    if code == KeyCode::Enter {
        let retry = panel
            .providers
            .discovery
            .as_ref()
            .is_some_and(|d| d.pending.is_none() && d.error.is_some() && d.rows.is_empty());
        if retry {
            panel.fetch(connection, &app.events);
        } else {
            panel.confirm_discovery(app, connection);
        }
        return;
    }
    if code == KeyCode::Char(' ') {
        let cursor = panel.providers.discovery.as_ref().map_or(0, |d| d.cursor);
        if let Some(&cand_idx) = visible.get(cursor) {
            panel.toggle_candidate(cand_idx);
            panel.error = None;
        }
        return;
    }
    let Some(discovery) = &mut panel.providers.discovery else {
        return;
    };
    let cursor_before = discovery.cursor;
    match code {
        KeyCode::Up => discovery.cursor = discovery.cursor.saturating_sub(1),
        KeyCode::Down => discovery.cursor = (discovery.cursor + 1).min(last),
        KeyCode::PageUp => discovery.cursor = discovery.cursor.saturating_sub(10),
        KeyCode::PageDown => discovery.cursor = (discovery.cursor + 10).min(last),
        KeyCode::Home => discovery.cursor = 0,
        KeyCode::End => discovery.cursor = last,
        KeyCode::Backspace => {
            discovery.query.pop();
            discovery.cursor = 0;
            discovery.offset = 0;
            discovery.scroll.reveal_selection = true;
            panel.error = None;
        }
        KeyCode::Char('u') if mods == KeyModifiers::CONTROL => {
            discovery.query.clear();
            discovery.cursor = 0;
            discovery.offset = 0;
            discovery.scroll.reveal_selection = true;
            panel.error = None;
        }
        KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            discovery.query.push(c);
            discovery.cursor = 0;
            discovery.offset = 0;
            discovery.scroll.reveal_selection = true;
            panel.error = None;
        }
        _ => {}
    }
    if let Some(discovery) = &mut panel.providers.discovery
        && discovery.cursor != cursor_before
    {
        discovery.scroll.reveal_selection = true;
    }
}
