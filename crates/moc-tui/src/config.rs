//! 前端偏好只保存在 tui.toml，不进入 core 配置或模型上下文。

use crossterm::event::{KeyCode, KeyModifiers};
use moc_i18n::t;
use moc_protocol::{ChoiceOption, ConfigField, FieldKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::history::HistoryScope;
use crate::transcript::DetailLevel;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveSubmit {
    #[default]
    Steering,
    Interrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// 思考块的显示档位（ctrl+t 循环，启动时生效）。
    pub thinking_detail: DetailLevel,
    /// 工具块的显示档位（ctrl+o 循环，启动时生效）。
    pub tool_detail: DetailLevel,
    pub notify_complete: bool,
    pub notify_waiting: bool,
    pub active_submit: ActiveSubmit,
    /// ↑/↓ 召回输入历史的范围：全局共享，或只看当前项目目录。
    pub history_scope: HistoryScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statusline: Option<String>,
    /// 加载后是完整键位表；落盘时只写与默认不同的动作（见 [`TuiStore::save`]）。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keybindings: BTreeMap<String, Vec<String>>,
}
impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            thinking_detail: DetailLevel::default(),
            tool_detail: DetailLevel::default(),
            notify_complete: true,
            notify_waiting: true,
            active_submit: ActiveSubmit::Steering,
            history_scope: HistoryScope::Global,
            statusline: None,
            keybindings: actions()
                .iter()
                .map(|(name, _, keys)| {
                    (
                        (*name).into(),
                        keys.iter().map(|key| (*key).into()).collect(),
                    )
                })
                .collect(),
        }
    }
}

/// 保持动作语义稳定，物理按键可以全部改写；配置页面自身的导航键保持可恢复。
pub(crate) fn actions() -> &'static [(&'static str, &'static str, &'static [&'static str])] {
    &[
        ("send", "tui.keys.send", &["enter"]),
        ("queue", "tui.keys.queue", &["ctrl+enter"]),
        ("newline", "tui.keys.newline", &["shift+enter", "alt+enter"]),
        ("cancel", "tui.keys.cancel", &["ctrl+c"]),
        ("help", "tui.keys.help", &["ctrl+h"]),
        ("thinking", "tui.keys.thinking", &["ctrl+t"]),
        ("tools", "tui.keys.tools", &["ctrl+o"]),
        ("paste_image", "tui.keys.paste_image", &["ctrl+v"]),
        ("model", "tui.keys.model", &["ctrl+p"]),
        ("resume", "tui.keys.resume", &["ctrl+r"]),
        ("engine", "tui.keys.engine", &["alt+m", "ctrl+m"]),
        ("thinking_level", "tui.keys.thinking_level", &["shift+tab"]),
        ("focus", "tui.keys.focus", &["tab"]),
        ("clear", "tui.keys.clear", &["ctrl+u"]),
        ("line_start", "tui.keys.line_start", &["home", "ctrl+a"]),
        ("line_end", "tui.keys.line_end", &["end", "ctrl+e"]),
        ("delete_word", "tui.keys.delete_word", &["ctrl+w"]),
        ("delete_to_end", "tui.keys.delete_to_end", &["ctrl+k"]),
        ("config", "tui.keys.config", &["ctrl+g"]),
        ("agents", "tui.keys.agents", &["ctrl+n"]),
        ("sidebar", "tui.keys.sidebar", &["alt+a"]),
        ("queue_edit", "tui.keys.queue_edit", &["alt+up"]),
    ]
}

pub struct KeyMap {
    bindings: Vec<(KeyModifiers, KeyCode, String, KeyModifiers, KeyCode)>,
    reserved: Vec<(KeyModifiers, KeyCode)>,
}
impl KeyMap {
    pub fn compile(config: &TuiConfig) -> anyhow::Result<Self> {
        let mut bindings = Vec::new();
        for (action, keys) in &config.keybindings {
            anyhow::ensure!(
                actions().iter().any(|(name, _, _)| name == action),
                "{}",
                t!("tui.err.unknown_action", action = action.as_str())
            );
            for key in keys {
                let (modifiers, code) = parse_key(key)?;
                anyhow::ensure!(
                    !bindings
                        .iter()
                        .any(|(other_mods, other_code, _, _, _)| *other_mods == modifiers
                            && *other_code == code),
                    "{}",
                    t!("tui.err.duplicate_key", key = key.as_str())
                );
                let (target_modifiers, target_code) = canonical(action).expect("已校验动作");
                bindings.push((
                    modifiers,
                    code,
                    action.clone(),
                    target_modifiers,
                    target_code,
                ));
            }
        }
        let reserved = actions()
            .iter()
            .flat_map(|(_, _, keys)| {
                keys.iter()
                    .map(|text| parse_key(text).expect("内置键位合法"))
            })
            .collect();
        Ok(Self { bindings, reserved })
    }
    pub fn action(&self, modifiers: KeyModifiers, code: KeyCode) -> Option<&str> {
        let (modifiers, code) = normalize(modifiers, code);
        self.bindings
            .iter()
            .find(|(mods, key, _, _, _)| *mods == modifiers && *key == code)
            .map(|(_, _, action, _, _)| action.as_str())
    }
    pub fn translated(
        &self,
        modifiers: KeyModifiers,
        code: KeyCode,
    ) -> Option<(KeyModifiers, KeyCode)> {
        let key = normalize(modifiers, code);
        self.bindings
            .iter()
            .find(|(mods, code, _, _, _)| (*mods, *code) == key)
            .map(|(_, _, _, mods, code)| (*mods, *code))
    }
    pub fn reserved(&self, modifiers: KeyModifiers, code: KeyCode) -> bool {
        self.reserved.contains(&normalize(modifiers, code))
    }
}

pub fn canonical(action: &str) -> Option<(KeyModifiers, KeyCode)> {
    actions()
        .iter()
        .find(|(name, _, _)| *name == action)
        .and_then(|(_, _, keys)| parse_key(keys[0]).ok())
}
fn normalize(mut modifiers: KeyModifiers, mut code: KeyCode) -> (KeyModifiers, KeyCode) {
    if code == KeyCode::BackTab {
        modifiers.insert(KeyModifiers::SHIFT);
        code = KeyCode::Tab;
    }
    if let KeyCode::Char(character) = code
        && character.is_ascii_uppercase()
    {
        modifiers.insert(KeyModifiers::SHIFT);
        code = KeyCode::Char(character.to_ascii_lowercase());
    }
    (modifiers, code)
}
pub fn parse_key(text: &str) -> anyhow::Result<(KeyModifiers, KeyCode)> {
    let lower = text.trim().to_lowercase();
    let mut parts: Vec<_> = lower.split('+').collect();
    let key = parts.pop().unwrap_or_default();
    let mut modifiers = KeyModifiers::NONE;
    for part in parts {
        modifiers |= match part {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" => KeyModifiers::SUPER,
            _ => anyhow::bail!("{}", t!("tui.err.unknown_modifier", part = part)),
        };
    }
    let code = match key {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        key if key.starts_with('f') && key[1..].parse::<u8>().is_ok() => {
            KeyCode::F(key[1..].parse()?)
        }
        key if key.chars().count() == 1 => KeyCode::Char(key.chars().next().expect("单字符")),
        _ => anyhow::bail!("{}", t!("tui.err.unreadable_key", text = text)),
    };
    Ok(normalize(modifiers, code))
}

/// [`parse_key`] 的逆运算：把一次按键写成配置口径（`ctrl+g`、`shift+tab`、`f2`）。
/// 单独的修饰键、`+` 本身（与分隔符冲突）和无法写进配置的键返回 `None`。
pub fn format_key(modifiers: KeyModifiers, code: KeyCode) -> Option<String> {
    let (modifiers, code) = normalize(modifiers, code);
    let key = match code {
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Char('+') => return None,
        KeyCode::Char(character) => character.to_lowercase().to_string(),
        _ => return None,
    };
    let mut text = String::new();
    for (flag, name) in [
        (KeyModifiers::CONTROL, "ctrl+"),
        (KeyModifiers::ALT, "alt+"),
        (KeyModifiers::SHIFT, "shift+"),
        (KeyModifiers::SUPER, "super+"),
    ] {
        if modifiers.contains(flag) {
            text.push_str(name);
        }
    }
    text.push_str(&key);
    Some(text)
}

pub struct TuiStore {
    pub path: PathBuf,
    original: String,
}
impl TuiStore {
    pub fn load(home: &Path) -> anyhow::Result<(Self, TuiConfig)> {
        let path = home.join("tui.toml");
        let original = read_optional(&path)?;
        let mut merged = serde_json::to_value(TuiConfig::default())?;
        let stored: toml::Value = toml::from_str(&original)?;
        moc_core::config::editor::merge_json(&mut merged, serde_json::to_value(stored)?);
        let mut config: TuiConfig = serde_json::from_value(merged)?;
        config.tool_detail = config.tool_detail.tool_preset();
        config.statusline = config.statusline.filter(|text| !text.trim().is_empty());
        KeyMap::compile(&config)?;
        Ok((Self { path, original }, config))
    }
    pub fn save(&mut self, config: &TuiConfig) -> anyhow::Result<()> {
        KeyMap::compile(config)?;
        let parent = self.path.parent().expect("TUI 配置父目录");
        std::fs::create_dir_all(parent)?;
        let _guard = moc_core::session::lock::SessionLockGuard::acquire(
            parent.join("tui-config.lock"),
            "tui-config",
        )?;
        anyhow::ensure!(
            read_optional(&self.path)? == self.original,
            "{}",
            t!("tui.err.settings_modified")
        );
        // 未改动的键位跟随内置默认：以后调整默认键位时，旧配置不会把它钉死
        let defaults = TuiConfig::default().keybindings;
        let mut stored = config.clone();
        stored
            .keybindings
            .retain(|action, keys| defaults.get(action) != Some(keys));
        let text = toml::to_string_pretty(&stored)?;
        let temp = parent.join(format!(".tui-{}.tmp", std::process::id()));
        use std::io::Write;
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, &self.path)?;
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
        self.original = text;
        Ok(())
    }
}
fn read_optional(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

/// 思考块保留三档显示；工具只使用前两个全局预设。
fn detail_options() -> Vec<ChoiceOption> {
    vec![
        ChoiceOption::new("hidden", &t!("tui.settings.opt_hidden")),
        ChoiceOption::new("brief", &t!("tui.settings.opt_brief")),
        ChoiceOption::new("full", &t!("tui.settings.opt_full")),
    ]
}

pub fn fields() -> Vec<ConfigField> {
    let descriptions = [
        (
            "thinking_detail",
            t!("tui.settings.thinking_detail"),
            t!("tui.settings.thinking_detail_desc"),
            FieldKind::Choice(detail_options()),
        ),
        (
            "tool_detail",
            t!("tui.settings.tool_detail"),
            t!("tui.settings.tool_detail_desc"),
            FieldKind::Choice(vec![
                ChoiceOption::new("hidden", &t!("tui.settings.opt_hidden")),
                ChoiceOption::new("brief", &t!("tui.settings.opt_brief")),
            ]),
        ),
        (
            "notify_complete",
            t!("tui.settings.notify_complete"),
            t!("tui.settings.notify_complete_desc"),
            FieldKind::Bool,
        ),
        (
            "notify_waiting",
            t!("tui.settings.notify_waiting"),
            t!("tui.settings.notify_waiting_desc"),
            FieldKind::Bool,
        ),
        (
            "active_submit",
            t!("tui.settings.active_submit"),
            t!("tui.settings.active_submit_desc"),
            FieldKind::Choice(vec![
                ChoiceOption::new("steering", &t!("tui.settings.opt_steering")),
                ChoiceOption::new("interrupt", &t!("tui.settings.opt_interrupt")),
            ]),
        ),
        (
            "history_scope",
            t!("tui.settings.history_scope"),
            t!("tui.settings.history_scope_desc"),
            FieldKind::Choice(vec![
                ChoiceOption::new("global", &t!("tui.settings.opt_global")),
                ChoiceOption::new("project", &t!("tui.settings.opt_project")),
            ]),
        ),
        (
            "statusline",
            t!("tui.settings.statusline"),
            t!("tui.settings.statusline_desc"),
            FieldKind::Text,
        ),
    ];
    let mut output: Vec<_> = descriptions
        .into_iter()
        .map(|(key, label, description, kind)| ConfigField {
            path: vec!["tui".into(), key.into()],
            tab: t!("tui.tab.ui").into_owned(),
            label: label.into_owned(),
            description: description.into_owned(),
            kind,
            advanced: false,
        })
        .collect();
    output.extend(actions().iter().map(|(name, label, _)| ConfigField {
        path: vec!["tui".into(), "keybindings".into(), (*name).into()],
        tab: t!("tui.tab.ui").into_owned(),
        label: t!(*label).into_owned(),
        description: t!("tui.settings.keybinding_desc").into_owned(),
        kind: FieldKind::Strings,
        advanced: false,
    }));
    output
}

pub fn notify(message: &str) {
    use std::io::{IsTerminal, Write};
    let mut stdout = std::io::stdout();
    if stdout.is_terminal() {
        let message: String = message
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        let _ = write!(
            stdout,
            "\x1b]9;{}\x1b\\",
            t!("tui.notify.format", message = message.as_str())
        );
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_key_map_keeps_unrelated_actions_and_rejects_conflicts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("tui.toml"),
            "[keybindings]\nsend=['ctrl+j']",
        )
        .unwrap();
        let (mut store, mut config) = TuiStore::load(root.path()).unwrap();
        store.save(&config).unwrap();
        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(
            saved.contains("send") && !saved.contains("queue"),
            "只落盘改动过的键位：{saved}"
        );
        let keys = KeyMap::compile(&config).unwrap();
        assert_eq!(
            keys.action(KeyModifiers::CONTROL, KeyCode::Char('j')),
            Some("send")
        );
        assert_eq!(keys.action(KeyModifiers::NONE, KeyCode::Enter), None);
        assert!(keys.reserved(KeyModifiers::NONE, KeyCode::Enter));
        assert_eq!(
            keys.action(KeyModifiers::CONTROL, KeyCode::Enter),
            Some("queue")
        );
        config
            .keybindings
            .insert("queue".into(), vec!["ctrl+j".into()]);
        assert!(KeyMap::compile(&config).is_err());
    }

    /// 录入的按键必须能被 parse_key 原样读回，否则保存后键位会悄悄变成另一个键。
    #[test]
    fn captured_keys_round_trip_through_the_parser() {
        for (modifiers, code) in [
            (KeyModifiers::CONTROL, KeyCode::Char('g')),
            (KeyModifiers::NONE, KeyCode::Char('G')),
            (KeyModifiers::SHIFT, KeyCode::BackTab),
            (KeyModifiers::ALT, KeyCode::Enter),
            (KeyModifiers::CONTROL | KeyModifiers::SHIFT, KeyCode::F(5)),
            (KeyModifiers::NONE, KeyCode::Char(' ')),
            (KeyModifiers::ALT, KeyCode::Up),
        ] {
            let text = format_key(modifiers, code).expect("可写进配置");
            assert_eq!(
                parse_key(&text).unwrap(),
                normalize(modifiers, code),
                "{text}"
            );
        }
        assert_eq!(
            format_key(KeyModifiers::NONE, KeyCode::Char('G')).as_deref(),
            Some("shift+g")
        );
        assert_eq!(format_key(KeyModifiers::CONTROL, KeyCode::Char('+')), None);
        assert_eq!(format_key(KeyModifiers::NONE, KeyCode::Insert), None);
    }

    #[test]
    fn front_end_preferences_persist_without_touching_core_configuration() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("config.toml"),
            "# core 不应被前端保存覆盖\n",
        )
        .unwrap();
        let (mut store, mut preferences) = TuiStore::load(root.path()).unwrap();
        preferences.thinking_detail = DetailLevel::Full;
        preferences.tool_detail = DetailLevel::Hidden;
        preferences.active_submit = ActiveSubmit::Interrupt;
        preferences.history_scope = HistoryScope::Project;
        store.save(&preferences).unwrap();
        let (_, reloaded) = TuiStore::load(root.path()).unwrap();
        assert_eq!(reloaded.thinking_detail, DetailLevel::Full);
        assert_eq!(reloaded.tool_detail, DetailLevel::Hidden);
        assert_eq!(reloaded.active_submit, ActiveSubmit::Interrupt);
        assert_eq!(reloaded.history_scope, HistoryScope::Project);
        assert_eq!(
            std::fs::read_to_string(root.path().join("config.toml")).unwrap(),
            "# core 不应被前端保存覆盖\n"
        );
    }
}
