//! 前端偏好只保存在 tui.toml，不进入 core 配置或模型上下文。

use crossterm::event::{KeyCode, KeyModifiers};
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
        ("send", "发送消息", &["enter"]),
        ("queue", "排队发送", &["ctrl+enter"]),
        ("newline", "插入换行", &["shift+enter", "alt+enter"]),
        ("cancel", "停止当前生成 / 退出", &["ctrl+c"]),
        ("help", "帮助", &["ctrl+h"]),
        ("thinking", "切换思考显示档位", &["ctrl+t"]),
        ("tools", "切换工具显示档位", &["ctrl+o"]),
        ("paste_image", "粘贴图片或文本", &["ctrl+v"]),
        ("model", "选择模型", &["ctrl+p"]),
        ("resume", "恢复会话", &["ctrl+r"]),
        ("mode", "切换会话方言", &["alt+m", "ctrl+m"]),
        ("thinking_level", "切换思考等级", &["shift+tab"]),
        ("focus", "切换输入焦点", &["tab"]),
        ("clear", "清空输入", &["ctrl+u"]),
        ("line_start", "移动到行首", &["home", "ctrl+a"]),
        ("line_end", "移动到行尾", &["end", "ctrl+e"]),
        ("delete_word", "删除前一词", &["ctrl+w"]),
        ("delete_to_end", "删除至行尾", &["ctrl+k"]),
        ("config", "打开设置", &["ctrl+g"]),
        ("agents", "打开子代理管理", &["ctrl+n"]),
        ("queue_edit", "编辑排队中的消息", &["alt+up"]),
    ]
}

/// 把一组绑定渲染成紧凑键位列：同一键码的多个修饰键合并成 `shift/alt + enter`，
/// 不同键码之间以 ` / ` 分隔。修饰键与键码之间留空格，长组合不再挤压描述列。
pub(crate) fn display_keys(keys: &[String]) -> String {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for key in keys {
        let code = code_of(key);
        let modifiers = &key[..key.len() - code.len()];
        let modifiers = modifiers.strip_suffix('+').unwrap_or_default().to_owned();
        match groups.iter_mut().find(|(group, _)| group == code) {
            Some((_, alternatives)) => alternatives.push(modifiers),
            None => groups.push((code.to_owned(), vec![modifiers])),
        }
    }
    groups
        .into_iter()
        .map(|(code, modifiers)| {
            let modifiers: Vec<_> = modifiers
                .into_iter()
                .filter(|modifier| !modifier.is_empty())
                .collect();
            if modifiers.is_empty() {
                code
            } else {
                format!("{} + {code}", modifiers.join("/"))
            }
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 绑定文本里的键码部分（`ctrl+enter` → `enter`；`home` → `home`）。
fn code_of(key: &str) -> &str {
    key.rsplit_once('+').map_or(key, |(_, code)| code)
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
                "未知键位动作：{action}"
            );
            for key in keys {
                let (modifiers, code) = parse_key(key)?;
                anyhow::ensure!(
                    !bindings
                        .iter()
                        .any(|(other_mods, other_code, _, _, _)| *other_mods == modifiers
                            && *other_code == code),
                    "按键 {key} 重复绑定，请先清除原动作中的绑定"
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
            _ => anyhow::bail!("未知修饰键：{part}"),
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
        _ => anyhow::bail!("无法识别按键 {text}；例如 ctrl+g、alt+enter、f2"),
    };
    Ok(normalize(modifiers, code))
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
            "设置文件已被其他窗口修改；请重新打开设置页再保存，当前修改不会丢失"
        );
        let text = toml::to_string_pretty(config)?;
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

/// 三档显示的单选项（思考与工具共用同一组语义）。
fn detail_options() -> Vec<ChoiceOption> {
    vec![
        ChoiceOption::new("hidden", "隐藏"),
        ChoiceOption::new("brief", "简略"),
        ChoiceOption::new("full", "完整"),
    ]
}

pub fn fields() -> Vec<ConfigField> {
    let descriptions = [
        (
            "thinking_detail",
            "思考显示档位",
            "启动时思考块的档位：隐藏只留耗时行，简略显示正文末尾预览，完整显示全部。",
            FieldKind::Choice(detail_options()),
        ),
        (
            "tool_detail",
            "工具显示档位",
            "启动时工具块的档位：隐藏只留摘要行，简略微量正文预览，完整显示全部输出。",
            FieldKind::Choice(detail_options()),
        ),
        (
            "notify_complete",
            "完成时通知",
            "窗口不在前台时，任务完成发送终端通知。",
            FieldKind::Bool,
        ),
        (
            "notify_waiting",
            "需要你回答时通知",
            "窗口不在前台时，需要你确认或回答时发送终端通知。",
            FieldKind::Bool,
        ),
        (
            "active_submit",
            "运行中按 Enter",
            "任务运行中按 Enter：补充要求，或先停下再重新提问。",
            FieldKind::Choice(vec![
                ChoiceOption::new("steering", "补充要求"),
                ChoiceOption::new("interrupt", "先停下再重新提问"),
            ]),
        ),
        (
            "history_scope",
            "输入历史范围",
            "↑/↓ 召回的历史：全局共享，或只看当前项目目录；提交内容始终写入同一份历史。",
            FieldKind::Choice(vec![
                ChoiceOption::new("global", "全局"),
                ChoiceOption::new("project", "当前项目"),
            ]),
        ),
        (
            "statusline",
            "状态栏模板",
            "自定义底部状态栏；留空使用默认样式。可用 {model} {mode} {workdir} {branch} {context} {context_percent} {todo} {tokens_in} {tokens_out} {tok_s} {prompt_elapsed} {cost}。",
            FieldKind::Text,
        ),
    ];
    let mut output: Vec<_> = descriptions
        .into_iter()
        .map(|(key, label, description, kind)| ConfigField {
            path: vec!["tui".into(), key.into()],
            tab: "界面".into(),
            label: label.into(),
            description: description.into(),
            kind,
            advanced: false,
        })
        .collect();
    output.extend(actions().iter().map(|(name, label, _)| ConfigField {
        path: vec!["tui".into(), "keybindings".into(), (*name).into()],
        tab: "界面".into(),
        label: format!("快捷键 · {label}"),
        description: "逐行填写按键，例如 ctrl+g、alt+enter、f2；留空表示禁用。".into(),
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
        let _ = write!(stdout, "\x1b]9;MOC：{message}\x1b\\");
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_column_merges_alternative_modifiers() {
        let keys = |list: &[&str]| list.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            display_keys(&keys(&["shift+enter", "alt+enter"])),
            "shift/alt + enter"
        );
        assert_eq!(display_keys(&keys(&["home", "ctrl+a"])), "home / ctrl + a");
        assert_eq!(display_keys(&keys(&["ctrl+shift+p"])), "ctrl+shift + p");
        assert_eq!(display_keys(&keys(&[])), "");
    }

    #[test]
    fn partial_key_map_keeps_unrelated_actions_and_rejects_conflicts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("tui.toml"),
            "[keybindings]\nsend=['ctrl+j']",
        )
        .unwrap();
        let (_, mut config) = TuiStore::load(root.path()).unwrap();
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
