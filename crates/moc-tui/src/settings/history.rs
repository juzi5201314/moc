use super::Job;
use super::SettingsPanel;
use super::Snapshot;
use super::Store;
use super::View;
use crate::app::App;
use crate::app::ConfigWrite;
use crate::config::KeyMap;
use crate::config::TuiConfig;
use crate::protocol::EmbeddedConnection;
use moc_i18n::t;
use moc_protocol::ConfigAction;
use moc_protocol::ConfigReply;
use moc_protocol::ConfigScope;
use serde_json::Value;

impl SettingsPanel {
    /// 改动前状态的快照，撤回时整体恢复。
    pub(super) fn snapshot(&self, label: &str) -> Snapshot {
        Snapshot {
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
    pub(super) fn commit(
        &mut self,
        label: &str,
        app: &mut App,
        connection: &EmbeddedConnection,
        apply: impl FnOnce(&mut Self) -> anyhow::Result<Store>,
    ) -> anyhow::Result<()> {
        if self.data.is_none() {
            let wait = t!("tui.hist.not_loaded").into_owned();
            self.error = Some(wait.clone());
            anyhow::bail!("{wait}");
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
                self.message = Some(t!("tui.hist.prefs_saved").into_owned());
            }
        }
        Ok(())
    }

    /// 把内存里的配置写入文件并立刻生效；有写入在途时先排队合并。
    pub(super) fn flush(&mut self, app: &mut App, connection: &EmbeddedConnection) {
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

    /// 切换 Global / Project 配置作用域并重新加载文档。
    pub fn toggle_scope(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        if app.saving_config() || self.busy {
            self.error = Some(t!("tui.err.saving_wait").into_owned());
            return;
        }
        self.scope = match self.scope {
            ConfigScope::Global => ConfigScope::Project,
            ConfigScope::Project => ConfigScope::Global,
        };
        self.data = None;
        self.edit = None;
        self.error = None;
        self.message = None;
        self.history = Default::default();
        self.providers.discovery = None;
        self.providers.param_picker = None;
        self.providers.test = None;
        self.providers.batch_models.clear();
        self.selected = 0;
        self.offset = 0;
        self.scroll.reveal_selection = true;
        self.ensure_loaded(app, connection);
    }

    /// 写入界面偏好文件，并把新设置应用到当前界面。
    pub(super) fn save_prefs(&mut self, app: &mut App) -> anyhow::Result<()> {
        let mut preferences: TuiConfig = serde_json::from_value(self.tui.clone())?;
        preferences.tool_detail = preferences.tool_detail.tool_preset();
        preferences.statusline = preferences
            .statusline
            .filter(|text| !text.trim().is_empty());
        let keys = KeyMap::compile(&preferences)?;
        app.tui_store
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("{}", t!("tui.hist.store_not_init")))?
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
    pub(super) fn undo(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(entry) = self.history.undo.pop() else {
            self.message = Some(t!("tui.hist.no_undo").into_owned());
            return;
        };
        self.step(entry, true, app, connection);
    }

    /// Ctrl+Y 或 Ctrl+Shift+Z：重做刚刚撤回的改动。
    pub(super) fn redo(&mut self, app: &mut App, connection: &EmbeddedConnection) {
        let Some(entry) = self.history.redo.pop() else {
            self.message = Some(t!("tui.hist.no_redo").into_owned());
            return;
        };
        self.step(entry, false, app, connection);
    }

    /// 套用一条历史快照。
    pub(super) fn step(
        &mut self,
        entry: Snapshot,
        undo: bool,
        app: &mut App,
        connection: &EmbeddedConnection,
    ) {
        let label = entry.label.clone();
        let key = if undo {
            "tui.hist.undid_fmt"
        } else {
            "tui.hist.redid_fmt"
        };
        let current = self.snapshot(&label);
        self.history.put(current, !undo);
        self.apply_snapshot(entry, app, connection);
        self.message = Some(t!(key, label = label).into_owned());
        self.outcome = self.message.clone();
    }

    /// 把快照套回内存并立刻写入生效。
    pub(super) fn apply_snapshot(
        &mut self,
        entry: Snapshot,
        app: &mut App,
        connection: &EmbeddedConnection,
    ) {
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

    /// 读取、发现、维护等回执：并入面板视图。
    pub(super) fn handle_response(
        &mut self,
        _app: &mut App,
        _connection: &EmbeddedConnection,
        result: anyhow::Result<ConfigReply>,
    ) {
        match result {
            Err(error) => {
                self.message = None;
                self.error = Some(error.to_string());
            }
            Ok(ConfigReply::Settings(data)) => {
                self.data = Some(*data);
                self.message = None;
                self.rebuild();
            }
            Ok(ConfigReply::Models(_)) | Ok(ConfigReply::ProviderTest(_)) => {}
            Ok(ConfigReply::Maintenance(report)) => {
                self.view = View::Maintenance { report, offset: 0 }
            }
            Ok(ConfigReply::Updated(message)) => {
                self.view = View::Fields;
                self.message = Some(message);
            }
        }
    }

    /// 确保已经读到配置文件；仍有改动在写入时先等着，免得读到旧内容又覆盖回去。
    pub(super) fn ensure_loaded(&mut self, app: &mut App, connection: &EmbeddedConnection) {
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
}
