use super::EditState;
use super::EditTarget;
use super::Job;
use super::RowKind;
use super::SettingsPanel;
use super::Store;
use super::TABS;
use super::controls;
use super::controls::Control;
use super::controls::PickAction;
use super::editors::Editor;
use super::editors::ListEditor;
use super::editors::RecordEditor;
use super::editors::SelectEditor;
use super::editors::TextEditor;
use super::forms;
use super::get;
use super::remove;
use super::set;
use crate::app::App;
use crate::config::TuiConfig;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use moc_i18n::t;
use moc_protocol::ConfigAction;
use moc_protocol::ConfigManage;
use moc_protocol::FieldKind;
use serde_json::Value;
use serde_json::json;

impl SettingsPanel {
    /// 在配置里写入一项；`tui` 开头的路径写界面偏好。
    pub(super) fn set_value(&mut self, path: &[String], value: Value) {
        if path.first().is_some_and(|part| part == "tui") {
            set(&mut self.tui, &path[1..], value);
        } else if let Some(data) = &mut self.data {
            set(&mut data.document, path, value.clone());
            // effective 是服务端结果的本地工作副本；仅编辑项在此叠加，
            // 下一次服务端回执会以新的 effective 重新建立基线。
            set(&mut data.effective, path, value);
        }
    }

    /// 恢复默认：删掉配置文件里的这一项；界面偏好则写回默认值。
    pub(super) fn remove_value(&mut self, path: &[String]) {
        if path.first().is_some_and(|part| part == "tui") {
            let defaults = serde_json::to_value(TuiConfig::default()).expect("默认设置");
            let value = get(&defaults, &path[1..]).cloned().unwrap_or(Value::Null);
            set(&mut self.tui, &path[1..], value);
        } else if let Some(data) = &mut self.data {
            remove(&mut data.document, path);
            // 删除显式值后显示服务端提供的继承值/默认值，而不是空值。
            if let Some(value) = get(&data.inherited, path).cloned() {
                set(&mut data.effective, path, value);
            } else {
                remove(&mut data.effective, path);
            }
        }
    }

    pub(super) fn request(
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

    /// 开始编辑选中行：按字段的控件打开开关、单选、清单、键值表、模型选择器或文本框。
    pub(super) fn begin_edit(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(RowKind::Field(index)) = self.row() else {
            if let Some((key, depth)) =
                self.rows
                    .get(self.selected)
                    .and_then(|row| match &row.kind {
                        RowKind::Group(group) => Some((group.key.clone(), row.depth)),
                        RowKind::Field(_) | RowKind::Action(_) => None,
                    })
            {
                self.toggle_group(&key, depth);
            }
            return;
        };
        let field = self.fields[*index].clone();
        let value = self.value(&field.path).cloned().unwrap_or(Value::Null);
        let editor = match controls::resolve(&field, self, app) {
            Control::Toggle => {
                let path = field.path;
                let _ = self.commit(&field.label, app, connection, |panel| {
                    panel.set_value(&path, json!(!value.as_bool().unwrap_or(false)));
                    Ok(Store::of(&path))
                });
                return;
            }
            // 共用的可搜索模型选择器：只列已配置的模型，可同时挑思考等级。
            Control::Model(specials) => {
                if app.models.is_empty() {
                    self.error = Some(t!("tui.err.no_models").into_owned());
                } else {
                    app.open_config_model_picker(
                        field.path,
                        field.label,
                        forms::format_value(&value, &FieldKind::Text),
                        &specials,
                    );
                }
                return;
            }
            Control::Select(picks) => Editor::Select(SelectEditor::new(picks, &value)),
            Control::List(spec) => Editor::List(ListEditor::new(spec, &value)),
            Control::Record(spec) => Editor::Record(RecordEditor::new(spec, &value)),
            Control::Input(kind) => {
                let text = forms::format_value(&value, &kind);
                Editor::Text(TextEditor::new(kind, text))
            }
        };
        let popup = !matches!(&editor, Editor::Text(text) if !text.multiline());
        self.edit = Some(EditState {
            target: EditTarget::Field(field.path),
            editor,
            popup,
            title: field.label,
            note: String::new(),
        });
    }

    /// Space：单选字段原地轮换到下一个候选，不打开弹窗；其余字段返回 `false`。
    pub(super) fn cycle(&mut self, app: &mut App, connection: &EmbeddedConnection) -> bool {
        let Some(RowKind::Field(index)) = self.row() else {
            return false;
        };
        let index = *index;
        let Control::Select(picks) = &self.controls[index] else {
            return false;
        };
        if picks.is_empty() {
            return false;
        }
        let field = &self.fields[index];
        let value = self.value(&field.path).unwrap_or(&Value::Null);
        let next =
            controls::current_pick(picks, value).map_or(0, |current| (current + 1) % picks.len());
        let pick = &picks[next];
        let label = format!("{}: {}", field.label, pick.label);
        let path = field.path.clone();
        let value = match &pick.action {
            PickAction::Set(value) => Some(Some(value.clone())),
            PickAction::Remove => Some(None),
            PickAction::Azure => None,
        };
        match value {
            Some(value) => {
                let _ = self.commit(&label, app, connection, |panel| {
                    panel.put(&path, value);
                    Ok(Store::of(&path))
                });
            }
            None => self.begin_azure(path),
        }
        true
    }

    /// 写入一项；`None` 删除该项，回到继承值或默认值。
    pub(super) fn put(&mut self, path: &[String], value: Option<Value>) {
        match value {
            Some(value) => self.set_value(path, value),
            None => self.remove_value(path),
        }
    }

    /// 打开一个只有一行输入的弹窗（新增条目、搜索等）。
    pub(super) fn begin_named(
        &mut self,
        target: EditTarget,
        title: &str,
        note: &str,
        initial: &str,
    ) {
        self.edit = Some(EditState {
            target,
            editor: Editor::Text(TextEditor::new(FieldKind::Text, initial.into())),
            popup: true,
            title: title.into(),
            note: note.into(),
        });
    }

    /// 选中 Azure 端点后填写接口版本；已是 Azure 时带出原值。
    pub(super) fn begin_azure(&mut self, path: Vec<String>) {
        let current = self
            .value(&path)
            .and_then(|value| value.pointer("/Azure/api_version"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.begin_named(
            EditTarget::Azure(path),
            &t!("tui.form.azure_title"),
            &t!("tui.form.azure_hint"),
            &current,
        );
    }

    pub(super) fn add(&mut self) {
        match TABS[self.tab] {
            "mcp" => self.begin_named(
                EditTarget::Mcp,
                &t!("tui.form.add_mcp_title"),
                &t!("tui.form.add_mcp_hint"),
                "",
            ),
            _ => self.error = Some(t!("tui.form.add_hint_wrong_tab").into_owned()),
        }
    }

    /// 登记可选模型；会话模型由用户在模型选择器中选择。
    pub(super) fn insert_model(&mut self, provider: &str, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!id.trim().is_empty(), "{}", t!("tui.err.model_id_empty"));
        let path = vec![
            "providers".into(),
            provider.into(),
            "models".into(),
            id.into(),
        ];
        anyhow::ensure!(
            self.value(&path).is_none(),
            "{}",
            t!("tui.err.model_exists", id = format!("{provider}/{id}"))
        );
        self.set_value(&path, json!({}));
        Ok(())
    }
}
