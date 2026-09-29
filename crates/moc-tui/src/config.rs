//! 前端偏好只保存在 tui.toml，不进入 core 配置或模型上下文。

use crossterm::event::{KeyCode, KeyModifiers};
use moc_protocol::{ConfigField, FieldKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
    pub show_thinking: bool,
    pub expand_tools: bool,
    pub notify_complete: bool,
    pub notify_waiting: bool,
    pub active_submit: ActiveSubmit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statusline: Option<String>,
    pub keybindings: BTreeMap<String, Vec<String>>,
}
impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            show_thinking: true,
            expand_tools: true,
            notify_complete: true,
            notify_waiting: true,
            active_submit: ActiveSubmit::Steering,
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
        ("send", "发送／补充消息", &["enter"]),
        ("queue", "加入后续任务队列", &["ctrl+enter"]),
        ("newline", "插入换行", &["shift+enter", "alt+enter"]),
        ("cancel", "取消轮次／退出", &["ctrl+c"]),
        ("help", "快捷键帮助", &["ctrl+h"]),
        ("thinking", "展开／折叠思考", &["ctrl+t"]),
        ("tools", "展开／折叠工具", &["ctrl+o"]),
        ("paste_image", "粘贴图片", &["ctrl+v"]),
        ("model", "选择模型", &["ctrl+p"]),
        ("resume", "恢复会话", &["ctrl+r"]),
        ("mode", "切换会话模式", &["ctrl+m"]),
        ("thinking_level", "切换思考等级", &["shift+tab"]),
        ("focus", "切换输入／转写焦点", &["tab"]),
        ("clear", "清空输入", &["ctrl+u"]),
        ("line_start", "移动到行首", &["home", "ctrl+a"]),
        ("line_end", "移动到行尾", &["end", "ctrl+e"]),
        ("delete_word", "删除前一词", &["ctrl+w"]),
        ("delete_to_end", "删除至行尾", &["ctrl+k"]),
        ("config", "打开配置", &["ctrl+g"]),
        ("queue_edit", "编辑队首消息", &["alt+up"]),
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
            "tui.toml 已被其他窗口修改，请重新加载；当前草稿保留"
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

pub fn fields() -> Vec<ConfigField> {
    let descriptions = [
        (
            "show_thinking",
            "默认展开思考",
            "新思考块默认展开；会话内仍可用快捷键临时切换。",
            FieldKind::Bool,
        ),
        (
            "expand_tools",
            "默认展开工具",
            "新工具块默认展开；会话内的折叠不回写默认值。",
            FieldKind::Bool,
        ),
        (
            "notify_complete",
            "完成通知",
            "终端不在前台时，轮次完成发送终端通知；由终端负责系统通知呈现。",
            FieldKind::Bool,
        ),
        (
            "notify_waiting",
            "等待回答通知",
            "终端不在前台时，提问与权限询问发送终端通知。",
            FieldKind::Bool,
        ),
        (
            "active_submit",
            "运行中 Enter 行为",
            "steering 在安全工具边界补充指令；interrupt 取消当前轮次并以此消息启动下一轮。Ctrl+Enter 始终排队。",
            FieldKind::Choice(vec!["steering".into(), "interrupt".into()]),
        ),
        (
            "statusline",
            "状态栏模板",
            "留空使用内置状态栏；支持 {model} {mode} {workdir} {branch} {context} {context_percent} {todo} {tokens_in} {tokens_out} {tok_s} {turn_elapsed} {cost}。",
            FieldKind::Text,
        ),
    ];
    let mut output: Vec<_> = descriptions
        .into_iter()
        .map(|(key, label, description, kind)| ConfigField {
            path: vec!["tui".into(), key.into()],
            tab: "TUI".into(),
            label: label.into(),
            description: description.into(),
            kind,
            advanced: false,
        })
        .collect();
    output.extend(actions().iter().map(|(name,label,_)| ConfigField {
        path:vec!["tui".into(),"keybindings".into(),(*name).into()],tab:"TUI".into(),label:format!("键位 · {label}"),
        description:"逐行填写按键，例如 ctrl+g、alt+enter、f2；可有多个绑定，空列表禁用。Ctrl+Enter 等增强按键需要终端支持，不能凭空区分被终端合并的编码。".into(),
        kind:FieldKind::Strings,advanced:false,
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
        preferences.show_thinking = false;
        preferences.expand_tools = false;
        preferences.active_submit = ActiveSubmit::Interrupt;
        store.save(&preferences).unwrap();
        let (_, reloaded) = TuiStore::load(root.path()).unwrap();
        assert!(!reloaded.show_thinking && !reloaded.expand_tools);
        assert_eq!(reloaded.active_submit, ActiveSubmit::Interrupt);
        assert_eq!(
            std::fs::read_to_string(root.path().join("config.toml")).unwrap(),
            "# core 不应被前端保存覆盖\n"
        );
    }
}
