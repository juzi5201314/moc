//! 供应商与模型工作区：侧栏实时预览、子分区（Models / Connection / Params）、
//! 模型钻取编辑、按需添加请求参数、预设新建、连通性测试、模型发现与批量管理。

use super::*;
use moc_core::config::presets::{PROVIDER_PRESETS, model_entry_from_catalog};
use moc_protocol::ProviderTestResult;

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

fn is_raw_param_container(field: &ConfigField) -> bool {
    matches!(
        field.path.as_slice(),
        [p, _, k] if p == "providers" && matches!(k.as_str(), "extra_body" | "headers")
    ) || matches!(
        field.path.as_slice(),
        [p, _, m, _, c, k]
            if p == "providers"
                && m == "models"
                && c == "compat"
                && matches!(k.as_str(), "extra_body" | "headers")
    )
}

fn is_provider_connection_field(id: &str, field: &ConfigField) -> bool {
    field.path.first().is_some_and(|p| p == "providers")
        && field.path.get(1).is_some_and(|p| p == id)
        && field.path.get(2).is_some_and(|p| p != "models")
}

pub const APIS: [(&str, &str); 3] = [
    ("openai_chat", "OpenAI Chat"),
    ("openai_responses", "OpenAI Responses"),
    ("anthropic_messages", "Anthropic Messages"),
];

pub fn api_short_label(api: &str) -> &'static str {
    match api {
        "openai_chat" => "chat",
        "openai_responses" => "resp",
        "anthropic_messages" => "anth",
        _ => "",
    }
}

/// 供应商详情子页签。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderSubTab {
    #[default]
    Models,
    Connection,
    Params,
}

impl ProviderSubTab {
    pub const ALL: [Self; 3] = [Self::Models, Self::Connection, Self::Params];

    pub fn cycle(self, reverse: bool) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        if reverse {
            Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
        } else {
            Self::ALL[(idx + 1) % Self::ALL.len()]
        }
    }
}

/// 模型详情子页签。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelSubTab {
    #[default]
    General,
    Compat,
    Params,
}

impl ModelSubTab {
    pub const ALL: [Self; 3] = [Self::General, Self::Compat, Self::Params];

    pub fn cycle(self, reverse: bool) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        if reverse {
            Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
        } else {
            Self::ALL[(idx + 1) % Self::ALL.len()]
        }
    }
}

/// 异步连通性测试状态。
pub struct TestState {
    pub generation: u64,
    pub pending: Option<tokio::task::AbortHandle>,
    pub result: Option<ProviderTestResult>,
}

impl Drop for TestState {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }
}

/// 参数页按需添加 API 请求参数的搜索弹窗。
pub struct ParamPicker {
    pub indices: Vec<usize>,
    pub query: String,
    pub cursor: usize,
    pub offset: usize,
    pub scroll: ModelScroll,
}

#[derive(Default)]
pub struct Providers {
    pub selected: Option<String>,
    pub sidebar: bool,
    pub cursor: usize,
    pub offset: usize,
    /// 侧栏的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    pub subtab: ProviderSubTab,
    /// 钻取编辑中的模型 ID；`None` 时展示供应商详情。
    pub editing_model: Option<String>,
    pub model_subtab: ModelSubTab,
    /// Models 子页的模型 ID 过滤词。
    pub model_filter: String,
    /// Models 子页中勾选的待批量操作模型 ID。
    pub batch_models: BTreeSet<String>,
    /// 用户通过「+ Add parameter」手动展开的参数路径键（`path.join("/")`）。
    pub pinned_params: BTreeSet<String>,
    pub draft: Option<ProviderDraft>,
    pub discovery: Option<Discovery>,
    pub param_picker: Option<ParamPicker>,
    pub test: Option<TestState>,
    pub(super) generation: u64,
}

/// 新建供应商表单：预设、ID、API、Base URL、API Key，加测试 / 添加并拉取 / 仅添加三个动作。
pub struct ProviderDraft {
    pub preset: usize,
    pub id: TextEditor,
    pub api: usize,
    pub url: TextEditor,
    pub key: TextEditor,
    pub azure_version: Option<String>,
    /// 8 个控件：0 Preset, 1 ID, 2 API, 3 Base URL, 4 API Key, 5 Test, 6 Add & Fetch, 7 Add。
    pub focus: usize,
    pub offset: usize,
    /// 表单的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    pub pending: Option<Snapshot>,
    pub discover: bool,
}

pub const DRAFT_CONTROLS: usize = 8;

impl Default for ProviderDraft {
    fn default() -> Self {
        Self {
            preset: 0,
            id: TextEditor::new(FieldKind::Text, String::new()),
            api: 0,
            url: TextEditor::new(FieldKind::Text, String::new()),
            key: TextEditor::new(FieldKind::Secret, String::new()),
            azure_version: None,
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
            1 => Some(&mut self.id),
            3 => Some(&mut self.url),
            4 => Some(&mut self.key),
            _ => None,
        }
    }

    /// 切换预设并自动填入推荐的 ID、协议、Base URL 与 API Key 环境变量提示。
    pub fn apply_preset(&mut self, index: usize, existing: &[String]) {
        let index = index % PROVIDER_PRESETS.len();
        self.preset = index;
        let preset = PROVIDER_PRESETS[index];
        self.azure_version = preset.azure_api_version.map(str::to_owned);
        if preset.key == "custom" {
            return;
        }
        let suggested = if existing.iter().any(|id| id == preset.id) {
            next_copy_id(preset.id, existing)
        } else {
            preset.id.to_owned()
        };
        self.id.input = PromptState::from_text(suggested);
        if let Some(pos) = APIS.iter().position(|(k, _)| *k == preset.api) {
            self.api = pos;
        }
        self.url.input = PromptState::from_text(preset.base_url.to_owned());
        self.key.input = PromptState::from_text(preset.api_key_hint.to_owned());
    }

    pub fn preview_payload(&self) -> anyhow::Result<(String, Value)> {
        let url = self.url.input.text.trim();
        anyhow::ensure!(!url.is_empty(), "{}", t!("tui.err.url_required"));
        anyhow::ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "{}",
            t!("tui.err.url_scheme")
        );
        let id = self.id.input.text.trim();
        let mut obj = json!({
            "api": APIS[self.api].0,
            "base_url": url,
            "api_key": self.key.input.text,
            "models": {}
        });
        if let Some(version) = &self.azure_version {
            obj["endpoint"] = json!({ "Azure": { "api_version": version } });
        }
        Ok((if id.is_empty() { "preview".into() } else { id.into() }, obj))
    }

    pub fn value(&self, panel: &SettingsPanel) -> anyhow::Result<(String, Value)> {
        let id = self.id.input.text.trim();
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "{}",
            t!("tui.err.provider_id_invalid")
        );
        anyhow::ensure!(
            panel.effective["providers"].get(id).is_none(),
            "{}",
            t!("tui.err.provider_exists", id = id)
        );
        let (_, payload) = self.preview_payload()?;
        Ok((id.into(), payload))
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
    /// 添加完一批模型后保留弹窗并展示反馈，方便继续搜索添加。
    pub status: Option<String>,
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
    Test,
    Fetch,
    Manual,
    FilterModels,
    AddParam,
    BatchReasoning,
    BatchVision,
    BatchToggleEnabled,
    BatchDelete,
    BatchClear,
    Clone,
    Remove,
}

pub fn next_copy_id(base: &str, existing: &[String]) -> String {
    let first = format!("{base}-copy");
    if !existing.iter().any(|item| item == &first) {
        return first;
    }
    for n in 2..1000 {
        let candidate = format!("{first}-{n}");
        if !existing.iter().any(|item| item == &candidate) {
            return candidate;
        }
    }
    format!("{first}-new")
}

impl SettingsPanel {
    pub fn provider_page(&self) -> bool {
        self.tab == TAB && self.filter.is_empty()
    }

    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .effective
            .get("providers")
            .and_then(Value::as_object)
            .map(|values| values.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    pub fn provider_model_ids(&self, id: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.effective["providers"][id]
            .get("models")
            .and_then(Value::as_object)
            .map(|models| models.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
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
            self.providers.param_picker = None;
            self.providers.editing_model = None;
            self.providers.test = None;
            self.providers.batch_models.clear();
            self.providers.model_filter.clear();
        }
        if let (Some(pid), Some(mid)) = (
            self.providers.selected.as_deref(),
            self.providers.editing_model.as_deref(),
        ) && self.effective["providers"][pid]["models"].get(mid).is_none()
        {
            self.providers.editing_model = None;
            self.providers.param_picker = None;
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

    /// 侧栏移动光标时实时预览对应供应商（不离开侧栏焦点），停在末行 `+ Add provider` 时不破坏当前详情。
    pub fn preview_provider(&mut self, index: usize) {
        let ids = self.provider_ids();
        self.providers.cursor = index.min(ids.len());
        self.providers.scroll.reveal_selection = true;
        if let Some(id) = ids.get(index)
            && self.providers.selected.as_ref() != Some(id)
        {
            self.providers.selected = Some(id.clone());
            self.providers.draft = None;
            self.providers.discovery = None;
            self.providers.param_picker = None;
            self.providers.editing_model = None;
            self.providers.test = None;
            self.providers.batch_models.clear();
            self.providers.model_filter.clear();
            self.selected = 0;
            self.offset = 0;
            self.scroll.reveal_selection = true;
            self.error = None;
            self.rebuild_rows();
        }
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
            let changed = self.providers.selected.as_ref() != Some(id);
            if changed {
                self.providers.discovery = None;
                self.providers.param_picker = None;
                self.providers.editing_model = None;
                self.providers.test = None;
                self.providers.batch_models.clear();
                self.providers.model_filter.clear();
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
        self.providers.editing_model = None;
        self.providers.param_picker = None;
        self.providers.test = None;
        self.providers.cursor = self.provider_ids().len();
        self.providers.scroll.reveal_selection = true;
        self.edit = None;
        self.error = None;
    }

    pub fn set_provider_subtab(&mut self, subtab: ProviderSubTab) {
        if self.providers.subtab != subtab {
            self.providers.subtab = subtab;
            self.providers.param_picker = None;
            self.selected = 0;
            self.offset = 0;
            self.scroll.reveal_selection = true;
            self.rebuild_rows();
        }
    }

    pub fn set_model_subtab(&mut self, subtab: ModelSubTab) {
        if self.providers.model_subtab != subtab {
            self.providers.model_subtab = subtab;
            self.providers.param_picker = None;
            self.selected = 0;
            self.offset = 0;
            self.scroll.reveal_selection = true;
            self.rebuild_rows();
        }
    }

    pub fn open_model(&mut self, provider: &str, model: &str) {
        self.providers.selected = Some(provider.to_owned());
        self.providers.editing_model = Some(model.to_owned());
        self.providers.model_subtab = ModelSubTab::General;
        self.providers.sidebar = false;
        self.providers.param_picker = None;
        self.selected = 0;
        self.offset = 0;
        self.scroll.reveal_selection = true;
        self.rebuild_rows();
    }

    pub fn close_model(&mut self) {
        let model = self.providers.editing_model.take();
        self.providers.param_picker = None;
        self.providers.subtab = ProviderSubTab::Models;
        self.rebuild_rows();
        if let (Some(pid), Some(mid)) = (self.providers.selected.clone(), model) {
            self.select_path(&["providers".into(), pid, "models".into(), mid]);
        }
    }

    /// 搜索结果退出时，将选中字段对应的供应商与模型上下文准确恢复到 Providers 视图。
    pub fn focus_provider_field(&mut self, path: &[String]) {
        if path.first().map(String::as_str) != Some("providers") {
            return;
        }
        let Some(pid) = path.get(1) else {
            return;
        };
        self.providers.selected = Some(pid.clone());
        self.providers.sidebar = false;
        if path.get(2).map(String::as_str) == Some("models")
            && let Some(mid) = path.get(3)
        {
            self.providers.editing_model = Some(mid.clone());
            self.providers.model_subtab = if path
                .iter()
                .any(|part| matches!(part.as_str(), "extra_body" | "headers"))
            {
                self.providers.pinned_params.insert(path.join("/"));
                ModelSubTab::Params
            } else if path.get(4).map(String::as_str) == Some("compat")
                || path.get(4).map(String::as_str) == Some("max_request_bytes")
            {
                ModelSubTab::Compat
            } else {
                ModelSubTab::General
            };
        } else {
            self.providers.editing_model = None;
            self.providers.subtab = if path
                .iter()
                .any(|part| matches!(part.as_str(), "extra_body" | "headers"))
            {
                self.providers.pinned_params.insert(path.join("/"));
                ProviderSubTab::Params
            } else {
                ProviderSubTab::Connection
            };
        }
    }

    fn is_param_active(&self, field: &ConfigField) -> bool {
        let key = field.path.join("/");
        if self.providers.pinned_params.contains(&key) {
            return true;
        }
        if self.explicit(&field.path) {
            return true;
        }
        if let Some(data) = &self.data
            && get(&data.inherited, &field.path).is_some()
        {
            return true;
        }
        if let Some(compat_idx) = field.path.iter().position(|part| part == "compat") {
            let mut provider_path = field.path[..2].to_vec();
            provider_path.extend_from_slice(&field.path[compat_idx + 1..]);
            if get(&self.effective, &provider_path).is_some() {
                return true;
            }
        }
        false
    }

    /// 当前供应商或模型下的全部可用请求参数（供 Params 子页计数与弹窗搜索）。
    pub fn available_param_indices(&self) -> Vec<usize> {
        let Some(pid) = &self.providers.selected else {
            return Vec::new();
        };
        if let Some(mid) = &self.providers.editing_model {
            let prefix = ["providers", pid.as_str(), "models", mid.as_str()];
            self.fields
                .iter()
                .enumerate()
                .filter(|(_, field)| {
                    field.path.len() > 4
                        && prefix
                            .iter()
                            .enumerate()
                            .all(|(i, seg)| field.path[i] == *seg)
                        && request_parameter(field)
                })
                .map(|(i, _)| i)
                .collect()
        } else {
            self.fields
                .iter()
                .enumerate()
                .filter(|(_, field)| {
                    is_provider_connection_field(pid, field) && request_parameter(field)
                })
                .map(|(i, _)| i)
                .collect()
        }
    }

    pub fn configured_param_count(&self) -> usize {
        self.available_param_indices()
            .into_iter()
            .filter(|&idx| self.is_param_active(&self.fields[idx]))
            .count()
    }

    /// 构造当前视图的行集：模型钻取视图或供应商三子页（Models / Connection / Params）。
    pub fn provider_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(id) = &self.providers.selected else {
            return rows;
        };
        let push_action = |rows: &mut Vec<Row>, action| {
            rows.push(Row {
                depth: 0,
                kind: RowKind::Action(action),
            });
        };

        if let Some(model) = &self.providers.editing_model {
            let prefix = vec![
                "providers".into(),
                id.clone(),
                "models".into(),
                model.clone(),
            ];
            match self.providers.model_subtab {
                ModelSubTab::General => {
                    for (index, field) in self.fields.iter().enumerate() {
                        if !field.path.starts_with(&prefix) {
                            continue;
                        }
                        if field.path.get(4).map(String::as_str) == Some("compat") || field.advanced
                        {
                            continue;
                        }
                        rows.push(Row {
                            depth: 0,
                            kind: RowKind::Field(index),
                        });
                    }
                }
                ModelSubTab::Compat => {
                    for (index, field) in self.fields.iter().enumerate() {
                        if !field.path.starts_with(&prefix) {
                            continue;
                        }
                        if request_parameter(field) || is_raw_param_container(field) {
                            continue;
                        }
                        if field.path.get(4).map(String::as_str) == Some("compat") || field.advanced
                        {
                            rows.push(Row {
                                depth: 0,
                                kind: RowKind::Field(index),
                            });
                        }
                    }
                }
                ModelSubTab::Params => {
                    push_action(&mut rows, ProviderAction::AddParam);
                    for (index, field) in self.fields.iter().enumerate() {
                        if !field.path.starts_with(&prefix) {
                            continue;
                        }
                        if request_parameter(field) && self.is_param_active(field) {
                            rows.push(Row {
                                depth: 0,
                                kind: RowKind::Field(index),
                            });
                        }
                    }
                    for (index, field) in self.fields.iter().enumerate() {
                        if field.path.starts_with(&prefix) && is_raw_param_container(field) {
                            rows.push(Row {
                                depth: 0,
                                kind: RowKind::Field(index),
                            });
                        }
                    }
                }
            }
            return rows;
        }

        match self.providers.subtab {
            ProviderSubTab::Models => {
                push_action(&mut rows, ProviderAction::Fetch);
                push_action(&mut rows, ProviderAction::Manual);
                let all_models = self.provider_model_ids(id);
                if !all_models.is_empty() {
                    push_action(&mut rows, ProviderAction::FilterModels);
                }
                if !self.providers.batch_models.is_empty() {
                    push_action(&mut rows, ProviderAction::BatchReasoning);
                    push_action(&mut rows, ProviderAction::BatchVision);
                    push_action(&mut rows, ProviderAction::BatchToggleEnabled);
                    push_action(&mut rows, ProviderAction::BatchDelete);
                    push_action(&mut rows, ProviderAction::BatchClear);
                }
                let filter = self.providers.model_filter.trim().to_lowercase();
                for model in all_models {
                    if !filter.is_empty() && !model.to_lowercase().contains(&filter) {
                        continue;
                    }
                    let path = vec![
                        "providers".into(),
                        id.clone(),
                        "models".into(),
                        model.clone(),
                    ];
                    let key = format!("providers/{id}/models/{model}");
                    let mut detail = self.model_summary(id, &model);
                    if self.inherited_from_global(&path) {
                        if !detail.is_empty() {
                            detail.push_str(" · ");
                        }
                        detail.push_str("[Global]");
                    }
                    let title = if self.providers.batch_models.contains(&model) {
                        format!("☑ {model}")
                    } else {
                        model.clone()
                    };
                    rows.push(Row {
                        depth: 0,
                        kind: RowKind::Group(Group {
                            key,
                            title,
                            detail,
                            grouping: path.clone(),
                            path: Some(path),
                        }),
                    });
                }
            }
            ProviderSubTab::Connection => {
                push_action(&mut rows, ProviderAction::Test);
                for (index, field) in self.fields.iter().enumerate() {
                    if !is_provider_connection_field(id, field) {
                        continue;
                    }
                    if request_parameter(field) || is_raw_param_container(field) {
                        continue;
                    }
                    rows.push(Row {
                        depth: 0,
                        kind: RowKind::Field(index),
                    });
                }
            }
            ProviderSubTab::Params => {
                push_action(&mut rows, ProviderAction::AddParam);
                for (index, field) in self.fields.iter().enumerate() {
                    if !is_provider_connection_field(id, field) {
                        continue;
                    }
                    if request_parameter(field) && self.is_param_active(field) {
                        rows.push(Row {
                            depth: 0,
                            kind: RowKind::Field(index),
                        });
                    }
                }
                for (index, field) in self.fields.iter().enumerate() {
                    if is_provider_connection_field(id, field) && is_raw_param_container(field) {
                        rows.push(Row {
                            depth: 0,
                            kind: RowKind::Field(index),
                        });
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

    pub fn visible_param_picks(&self) -> Vec<usize> {
        let Some(picker) = &self.providers.param_picker else {
            return Vec::new();
        };
        let query = picker.query.trim().to_lowercase();
        picker
            .indices
            .iter()
            .copied()
            .filter(|&idx| {
                let field = &self.fields[idx];
                query.is_empty()
                    || field.label.to_lowercase().contains(&query)
                    || field.description.to_lowercase().contains(&query)
            })
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
            ProviderAction::Test => match &self.providers.test {
                Some(state) if state.pending.is_some() => t!("tui.providers.testing").into_owned(),
                _ => t!("tui.providers.test").into_owned(),
            },
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
            ProviderAction::FilterModels => {
                if self.providers.model_filter.is_empty() {
                    t!("tui.providers.filter_models").into_owned()
                } else {
                    t!(
                        "tui.providers.filter_models_active",
                        query = self.providers.model_filter.as_str()
                    )
                    .into_owned()
                }
            }
            ProviderAction::AddParam => t!("tui.providers.add_param").into_owned(),
            ProviderAction::BatchReasoning => t!(
                "tui.providers.batch_reasoning",
                count = self.providers.batch_models.len()
            )
            .into_owned(),
            ProviderAction::BatchVision => t!(
                "tui.providers.batch_vision",
                count = self.providers.batch_models.len()
            )
            .into_owned(),
            ProviderAction::BatchToggleEnabled => t!(
                "tui.providers.batch_toggle_enabled",
                count = self.providers.batch_models.len()
            )
            .into_owned(),
            ProviderAction::BatchDelete => t!(
                "tui.providers.batch_delete",
                count = self.providers.batch_models.len()
            )
            .into_owned(),
            ProviderAction::BatchClear => t!("tui.providers.batch_clear").into_owned(),
            ProviderAction::Clone => t!("tui.providers.clone").into_owned(),
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
        let visible = self.visible_candidates();
        let chosen: Vec<CatalogModel> = {
            let Some(d) = &self.providers.discovery else {
                return;
            };
            let mut list: Vec<CatalogModel> = d
                .rows
                .iter()
                .filter(|model| {
                    d.checked.contains(&model.id)
                        && self.effective["providers"][&provider]["models"]
                            .get(model.id.as_str())
                            .is_none()
                })
                .cloned()
                .collect();
            // 未手动勾选时按 Enter 直接添加当前高亮项，减少一次 Space 操作。
            if list.is_empty()
                && let Some(&cand_idx) = visible.get(d.cursor)
                && let Some(model) = d.rows.get(cand_idx)
                && self.effective["providers"][&provider]["models"]
                    .get(model.id.as_str())
                    .is_none()
            {
                list.push(model.clone());
            }
            list
        };
        if chosen.is_empty() {
            self.error = Some(t!("tui.err.check_models").into_owned());
            return;
        }
        let count = chosen.len();
        let result = self.commit(&t!("tui.snap.add_models"), app, connection, |panel| {
            for model in &chosen {
                let entry = model_entry_from_catalog(model);
                panel.insert_model_entry(&provider, &model.id, entry)?;
            }
            Ok(Store::Core)
        });
        if result.is_ok() {
            // 保持发现弹窗打开并清空勾选，允许用户继续输入其他关键词添加下一批模型。
            if let Some(d) = &mut self.providers.discovery {
                d.checked.clear();
                d.status = Some(t!("tui.providers.added_keep_open", count = count).into_owned());
            }
            self.rebuild_rows();
        }
    }

    pub fn open_param_picker(&mut self) {
        let indices: Vec<usize> = self
            .available_param_indices()
            .into_iter()
            .filter(|&idx| !self.is_param_active(&self.fields[idx]))
            .collect();
        self.providers.param_picker = Some(ParamPicker {
            indices,
            query: String::new(),
            cursor: 0,
            offset: 0,
            scroll: ModelScroll::default(),
        });
        self.error = None;
    }

    pub fn confirm_param_pick(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let visible = self.visible_param_picks();
        let cursor = self.providers.param_picker.as_ref().map_or(0, |p| p.cursor);
        let Some(&field_idx) = visible.get(cursor) else {
            return;
        };
        let path = self.fields[field_idx].path.clone();
        self.providers.pinned_params.insert(path.join("/"));
        self.providers.param_picker = None;
        self.rebuild_rows();
        self.select_path(&path);
        self.begin_edit(app, connection);
    }

    fn current_model_id(&self) -> Option<String> {
        if let Some(mid) = &self.providers.editing_model {
            return Some(mid.clone());
        }
        match self.row() {
            Some(RowKind::Group(group)) => match group.grouping.as_slice() {
                [p, _, m, mid] if p == "providers" && m == "models" => Some(mid.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn toggle_selected_enabled(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(pid) = self.providers.selected.clone() else {
            return;
        };
        if !self.providers.sidebar
            && let Some(mid) = self.current_model_id()
        {
            let path = vec![
                "providers".into(),
                pid.clone(),
                "models".into(),
                mid.clone(),
                "enabled".into(),
            ];
            let current = self.value(&path).and_then(Value::as_bool).unwrap_or(true);
            let next = if current { Some(json!(false)) } else { None };
            let _ = self.commit(&format!("{mid}: enabled"), app, connection, |panel| {
                panel.put(&path, next);
                Ok(Store::Core)
            });
            return;
        }
        let path = vec!["providers".into(), pid.clone(), "enabled".into()];
        let current = self.value(&path).and_then(Value::as_bool).unwrap_or(true);
        let next = if current { Some(json!(false)) } else { None };
        let _ = self.commit(&format!("{pid}: enabled"), app, connection, |panel| {
            panel.put(&path, next);
            Ok(Store::Core)
        });
    }

    pub fn start_clone(&mut self) {
        let Some(pid) = self.providers.selected.clone() else {
            return;
        };
        if !self.providers.sidebar
            && let Some(mid) = self.current_model_id()
        {
            let suggested = next_copy_id(&mid, &self.provider_model_ids(&pid));
            self.begin_named(
                EditTarget::CloneModel(pid, mid.clone()),
                &t!("tui.providers.clone_model_title", id = mid),
                &t!("tui.providers.clone_model_note"),
                &suggested,
            );
            return;
        }
        let suggested = next_copy_id(&pid, &self.provider_ids());
        self.begin_named(
            EditTarget::CloneProvider(pid.clone()),
            &t!("tui.providers.clone_provider_title", id = pid),
            &t!("tui.providers.field_id_desc"),
            &suggested,
        );
    }

    pub fn provider_action(
        &mut self,
        action: ProviderAction,
        app: &mut App,
        connection: &EmbeddedConnection,
    ) {
        match action {
            ProviderAction::New => self.new_provider(),
            ProviderAction::Test => self.test_connection(connection, &app.events),
            ProviderAction::Fetch => self.fetch(connection, &app.events),
            ProviderAction::Manual => {
                if let Some(id) = self.providers.selected.clone() {
                    self.providers.subtab = ProviderSubTab::Models;
                    self.providers.editing_model = None;
                    self.rebuild_rows();
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
            ProviderAction::FilterModels => {
                let current = self.providers.model_filter.clone();
                self.begin_named(
                    EditTarget::ModelFilter,
                    &t!("tui.providers.filter_models"),
                    &t!("tui.providers.filter_models_note"),
                    &current,
                );
            }
            ProviderAction::AddParam => self.open_param_picker(),
            ProviderAction::BatchReasoning => {
                self.batch_update_models(app, connection, "reasoning", Some(json!(true)));
            }
            ProviderAction::BatchVision => {
                self.batch_update_models(app, connection, "input", Some(json!(["text", "image"])));
            }
            ProviderAction::BatchToggleEnabled => {
                let Some(pid) = self.providers.selected.clone() else {
                    return;
                };
                let any_enabled = self.providers.batch_models.iter().any(|mid| {
                    self.effective["providers"][&pid]["models"][mid]
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true)
                });
                let next = if any_enabled { Some(json!(false)) } else { None };
                self.batch_update_models(app, connection, "enabled", next);
            }
            ProviderAction::BatchDelete => {
                let Some(pid) = self.providers.selected.clone() else {
                    return;
                };
                let ids: Vec<String> = self
                    .providers
                    .batch_models
                    .iter()
                    .filter(|mid| {
                        self.explicit(&[
                            "providers".into(),
                            pid.clone(),
                            "models".into(),
                            (*mid).clone(),
                        ])
                    })
                    .cloned()
                    .collect();
                if ids.is_empty() {
                    self.error = Some(t!("tui.providers.batch_no_explicit").into_owned());
                    return;
                }
                let _ = self.commit(&t!("tui.providers.batch_delete_snap"), app, connection, |panel| {
                    for mid in &ids {
                        panel.remove_value(&[
                            "providers".into(),
                            pid.clone(),
                            "models".into(),
                            mid.clone(),
                        ]);
                    }
                    Ok(Store::Core)
                });
                self.providers.batch_models.clear();
                self.rebuild_rows();
            }
            ProviderAction::BatchClear => {
                self.providers.batch_models.clear();
                self.rebuild_rows();
            }
            ProviderAction::Clone => self.start_clone(),
            ProviderAction::Remove => {
                if let Some(id) = &self.providers.selected {
                    if self.inherited_from_global(&["providers".into(), id.clone()]) {
                        self.error = Some(t!("tui.providers.inherited_no_delete").into_owned());
                        return;
                    }
                    self.view = View::Confirm {
                        text: t!("tui.providers.remove_confirm", id = id).into_owned(),
                        action: ConfirmAction::Remove(vec!["providers".into(), id.clone()]),
                    };
                }
            }
        }
    }

    fn batch_update_models(
        &mut self,
        app: &mut App,
        connection: &EmbeddedConnection,
        key: &str,
        value: Option<Value>,
    ) {
        let Some(pid) = self.providers.selected.clone() else {
            return;
        };
        let ids: Vec<String> = self.providers.batch_models.iter().cloned().collect();
        if ids.is_empty() {
            return;
        }
        let label = format!("Batch {key} ({})", ids.len());
        let _ = self.commit(&label, app, connection, |panel| {
            for mid in &ids {
                let path = vec![
                    "providers".into(),
                    pid.clone(),
                    "models".into(),
                    mid.clone(),
                    key.into(),
                ];
                panel.put(&path, value.clone());
            }
            Ok(Store::Core)
        });
        self.providers.batch_models.clear();
        self.rebuild_rows();
    }

    pub fn test_connection(
        &mut self,
        connection: &EmbeddedConnection,
        events: &tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) {
        let (id, provider) = if let Some(draft) = &self.providers.draft {
            match draft.preview_payload() {
                Ok(pair) => pair,
                Err(error) => {
                    self.error = Some(error.to_string());
                    return;
                }
            }
        } else {
            let Some(id) = self.providers.selected.clone() else {
                self.error = Some(t!("tui.err.need_provider").into_owned());
                return;
            };
            (id.clone(), self.effective["providers"][&id].clone())
        };
        self.providers.generation += 1;
        let generation = self.providers.generation;
        let client = connection.client.clone();
        let events = events.clone();
        let panel = self.id;
        let task = tokio::spawn(async move {
            let result = client
                .call::<ConfigManage>(ConfigAction::TestProvider { id, provider })
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SettingsResponse {
                panel,
                job: Job::TestProvider(generation),
                result,
            });
        });
        self.providers.test = Some(TestState {
            generation,
            pending: Some(task.abort_handle()),
            result: None,
        });
        self.error = None;
    }

    pub fn test_provider_response(&mut self, generation: u64, result: anyhow::Result<ConfigReply>) {
        let Some(state) = &mut self.providers.test else {
            return;
        };
        if state.generation != generation {
            return;
        }
        state.pending = None;
        match result {
            Ok(ConfigReply::ProviderTest(report)) => {
                state.result = Some(report);
            }
            Err(error) => {
                state.result = Some(ProviderTestResult {
                    ok: false,
                    latency_ms: 0,
                    endpoint: String::new(),
                    status: None,
                    models_count: None,
                    message: error.to_string(),
                });
            }
            Ok(_) => {}
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
            status: None,
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
        self.providers.subtab = ProviderSubTab::Models;
        self.providers.editing_model = None;
        self.providers.test = None;
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

/// 专属页面先处理弹窗、表单、子页签与侧栏焦点，其余字段编辑仍走共用控件。
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
        return draft_key(panel, app, connection, mods, code);
    }
    if panel.providers.discovery.is_some() {
        discovery_key(panel, app, connection, mods, code);
        return true;
    }
    if panel.providers.param_picker.is_some() {
        param_picker_key(panel, app, connection, mods, code);
        return true;
    }
    // `Tab` / `Shift+Tab` 在当前视图的子页签间循环（Models ↔ Connection ↔ Params / General ↔ Compat ↔ Params）。
    if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
        let previous = code == KeyCode::BackTab || mods.contains(KeyModifiers::SHIFT);
        if panel.providers.sidebar {
            panel.choose_provider(panel.providers.cursor);
            panel.providers.sidebar = false;
        } else if panel.providers.editing_model.is_some() {
            let next = panel.providers.model_subtab.cycle(previous);
            panel.set_model_subtab(next);
        } else {
            let next = panel.providers.subtab.cycle(previous);
            panel.set_provider_subtab(next);
        }
        return true;
    }
    if code == KeyCode::Esc {
        if panel.providers.editing_model.is_some() {
            panel.close_model();
            return true;
        }
        if panel.providers.sidebar {
            panel.providers.sidebar = false;
            return true;
        }
        if !panel.providers.batch_models.is_empty() {
            panel.providers.batch_models.clear();
            panel.rebuild_rows();
            return true;
        }
        if !panel.providers.model_filter.is_empty() {
            panel.providers.model_filter.clear();
            panel.rebuild_rows();
            return true;
        }
        return false;
    }
    if matches!(code, KeyCode::Left | KeyCode::Char('h'))
        && mods == KeyModifiers::NONE
        && !panel.providers.sidebar
    {
        if panel.providers.editing_model.is_some() {
            panel.close_model();
        } else {
            panel.providers.sidebar = true;
            panel.providers.scroll.reveal_selection = true;
        }
        return true;
    }
    if code == KeyCode::Char('s') && mods == KeyModifiers::NONE {
        panel.toggle_scope(app, connection);
        return true;
    }
    if code == KeyCode::Char('n') && mods == KeyModifiers::NONE {
        panel.new_provider();
        return true;
    }
    if code == KeyCode::Char('t') && mods == KeyModifiers::NONE {
        if panel.providers.sidebar {
            panel.preview_provider(panel.providers.cursor);
        }
        if panel.providers.draft.is_none() {
            panel.test_connection(connection, &app.events);
        }
        return true;
    }
    if code == KeyCode::Char('c') && mods == KeyModifiers::NONE {
        panel.start_clone();
        return true;
    }
    if code == KeyCode::Char('x') && mods == KeyModifiers::NONE {
        panel.toggle_selected_enabled(app, connection);
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
    let quick_action = match code {
        KeyCode::Char('a') if mods == KeyModifiers::NONE => Some(ProviderAction::Manual),
        KeyCode::Char('f') if mods == KeyModifiers::NONE => Some(ProviderAction::Fetch),
        _ => None,
    };
    if let Some(action) = quick_action {
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
                panel.providers.cursor = panel.providers.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                panel.providers.cursor = (panel.providers.cursor + 1).min(last);
            }
            KeyCode::Home | KeyCode::Char('g') => panel.providers.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => panel.providers.cursor = last,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                panel.choose_provider(panel.providers.cursor);
                panel.providers.sidebar = false;
                return true;
            }
            KeyCode::Delete => {
                if panel.providers.cursor < last {
                    panel.preview_provider(panel.providers.cursor);
                    panel.provider_action(ProviderAction::Remove, app, connection);
                }
                return true;
            }
            _ => return false,
        }
        if panel.providers.cursor != cursor_before {
            panel.preview_provider(panel.providers.cursor);
        }
        return true;
    }
    // Models 子页的模型行：Enter 钻取模型编辑，Space 切换批量勾选，Del 删除模型。
    if panel.providers.editing_model.is_none()
        && panel.providers.subtab == ProviderSubTab::Models
        && let Some(RowKind::Group(group)) = panel.row()
        && let [p, pid, m, mid] = group.grouping.as_slice()
        && p == "providers"
        && m == "models"
    {
        let (pid, mid) = (pid.clone(), mid.clone());
        match code {
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                panel.open_model(&pid, &mid);
                return true;
            }
            KeyCode::Char(' ') => {
                if !panel.providers.batch_models.remove(&mid) {
                    panel.providers.batch_models.insert(mid);
                }
                panel.rebuild_rows();
                return true;
            }
            _ => {}
        }
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

fn draft_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) -> bool {
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
        panel.providers.test = None;
        panel.error = None;
        return true;
    }
    if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('s') {
        panel.submit_provider(app, connection, true);
        return true;
    }
    let existing = panel.provider_ids();
    let focus_before = panel.providers.draft.as_ref().map_or(0, |d| d.focus);
    match code {
        KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
            if let Some(d) = &mut panel.providers.draft {
                d.focus = (d.focus + DRAFT_CONTROLS - 1) % DRAFT_CONTROLS;
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            if let Some(d) = &mut panel.providers.draft {
                d.focus = (d.focus + 1) % DRAFT_CONTROLS;
            }
        }
        KeyCode::BackTab | KeyCode::Up => {
            if let Some(d) = &mut panel.providers.draft {
                d.focus = (d.focus + DRAFT_CONTROLS - 1) % DRAFT_CONTROLS;
            }
        }
        KeyCode::Enter if focus_before == 5 => {
            panel.test_connection(connection, &app.events);
        }
        KeyCode::Enter if focus_before >= 6 => {
            panel.submit_provider(app, connection, focus_before == 6);
        }
        // Preset 与 API 行按 Enter 同样前进到下一字段，避免填表卡死。
        KeyCode::Enter => {
            if let Some(d) = &mut panel.providers.draft {
                d.focus = (d.focus + 1).min(DRAFT_CONTROLS - 1);
            }
        }
        KeyCode::Right | KeyCode::Char(' ') if focus_before == 0 => {
            if let Some(d) = &mut panel.providers.draft {
                let next = (d.preset + 1) % PROVIDER_PRESETS.len();
                d.apply_preset(next, &existing);
            }
            panel.error = None;
        }
        KeyCode::Left if focus_before == 0 => {
            if let Some(d) = &mut panel.providers.draft {
                let prev = (d.preset + PROVIDER_PRESETS.len() - 1) % PROVIDER_PRESETS.len();
                d.apply_preset(prev, &existing);
            }
            panel.error = None;
        }
        KeyCode::Right | KeyCode::Char(' ') if focus_before == 2 => {
            if let Some(d) = &mut panel.providers.draft {
                d.api = (d.api + 1) % APIS.len();
            }
        }
        KeyCode::Left if focus_before == 2 => {
            if let Some(d) = &mut panel.providers.draft {
                d.api = (d.api + APIS.len() - 1) % APIS.len();
            }
        }
        KeyCode::F(6) => {
            if let Some(d) = &mut panel.providers.draft {
                d.key.reveal = !d.key.reveal;
            }
        }
        _ => {
            if let Some(d) = &mut panel.providers.draft
                && let Some(input) = d.input()
            {
                crate::widgets::prompt::edit_input(&mut input.input, mods, code);
            }
        }
    }
    if let Some(draft) = panel.providers.draft.as_mut()
        && draft.focus != focus_before
    {
        draft.scroll.reveal_selection = true;
    }
    true
}

fn param_picker_key(
    panel: &mut SettingsPanel,
    app: &mut App,
    connection: &EmbeddedConnection,
    mods: KeyModifiers,
    code: KeyCode,
) {
    if code == KeyCode::Esc {
        panel.providers.param_picker = None;
        return;
    }
    if code == KeyCode::Enter {
        panel.confirm_param_pick(app, connection);
        return;
    }
    let visible = panel.visible_param_picks();
    let last = visible.len().saturating_sub(1);
    let Some(picker) = &mut panel.providers.param_picker else {
        return;
    };
    let before = picker.cursor;
    match code {
        KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
        KeyCode::Down => picker.cursor = (picker.cursor + 1).min(last),
        KeyCode::Char('k') if mods == KeyModifiers::CONTROL => {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
        KeyCode::Char('j') if mods == KeyModifiers::CONTROL => {
            picker.cursor = (picker.cursor + 1).min(last);
        }
        KeyCode::PageUp => picker.cursor = picker.cursor.saturating_sub(10),
        KeyCode::PageDown => picker.cursor = (picker.cursor + 10).min(last),
        KeyCode::Home => picker.cursor = 0,
        KeyCode::End => picker.cursor = last,
        KeyCode::Backspace => {
            picker.query.pop();
            picker.cursor = 0;
            picker.offset = 0;
            picker.scroll.reveal_selection = true;
        }
        KeyCode::Char('u') if mods == KeyModifiers::CONTROL => {
            picker.query.clear();
            picker.cursor = 0;
            picker.offset = 0;
            picker.scroll.reveal_selection = true;
        }
        KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            picker.query.push(c);
            picker.cursor = 0;
            picker.offset = 0;
            picker.scroll.reveal_selection = true;
        }
        _ => {}
    }
    if picker.cursor != before {
        picker.scroll.reveal_selection = true;
    }
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
        KeyCode::Char('k') if mods == KeyModifiers::CONTROL => {
            discovery.cursor = discovery.cursor.saturating_sub(1);
        }
        KeyCode::Char('j') if mods == KeyModifiers::CONTROL => {
            discovery.cursor = (discovery.cursor + 1).min(last);
        }
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
