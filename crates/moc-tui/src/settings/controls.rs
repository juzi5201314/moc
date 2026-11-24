//! 字段 → 编辑控件：只决定用什么方式编辑，写回的值仍是 core 配置 schema 的原样。
//!
//! 能枚举的都给控件：开关、单选、模型选择器、勾选清单、键值表；文本框只留给
//! 真正自由的内容（地址、密钥、命令、正则、提示词、数值）。

use moc_i18n::t;

use super::*;

/// 一个字段的编辑方式。
pub enum Control {
    /// 布尔开关：Enter/Space 直接翻转。
    Toggle,
    /// 单选弹窗：Space 原地轮换，Enter 打开列表。
    Select(Vec<Pick>),
    /// 共用模型选择器，只列已配置的模型；`specials` 是角色伪值。
    Model(Vec<(&'static str, String)>),
    /// 勾选清单 / 自由列表（字符串数组）。
    List(ListSpec),
    /// 键值表（对象）。
    Record(RecordSpec),
    /// 文本框：单行原地编辑，多行与 JSON 用弹窗。
    Input(FieldKind),
}

/// 单选候选。
pub struct Pick {
    pub label: String,
    pub action: PickAction,
}

/// 选中候选后写回什么。
pub enum PickAction {
    Set(Value),
    /// 删除该项：回到继承值 / 自动判断。
    Remove,
    /// Azure 端点：选中后再填 `api_version`。
    Azure,
}

impl Pick {
    fn set(value: Value, label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            action: PickAction::Set(value),
        }
    }

    /// 当前值落在这个候选上。
    fn matches(&self, value: &Value) -> bool {
        match &self.action {
            PickAction::Set(candidate) => candidate == value,
            PickAction::Remove => value.is_null(),
            PickAction::Azure => value.get("Azure").is_some(),
        }
    }
}

/// 当前值对应的候选下标。
pub fn current_pick(picks: &[Pick], value: &Value) -> Option<usize> {
    picks.iter().position(|pick| pick.matches(value))
}

/// 字符串数组的编辑规格。有预设时是勾选清单，没有时是自由列表。
pub struct ListSpec {
    /// `(值, 说明)`：常用候选，勾选即写入。
    pub presets: Vec<(String, String)>,
    /// 允许追加的自定义项；`None` 表示只能从预设里选。
    pub custom: Option<Entry>,
    /// 顺序有意义（尝试顺序、默认项），允许 Shift+↑↓ 调整。
    pub ordered: bool,
    /// 什么都不选时写回什么。
    pub empty: Empty,
}

/// 自定义项的录入方式与校验。
#[derive(Clone, Copy)]
pub enum Entry {
    /// 单行文本。
    Text,
    /// 思考等级名，按 core 的规则校验。
    Level,
    /// 直接按下要绑定的键。
    Key,
}

impl Entry {
    /// 规范化并校验一条自定义项。
    pub fn validate(self, text: &str) -> anyhow::Result<String> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "{}", t!("tui.ctrl.empty_content"));
        anyhow::ensure!(!text.contains('\n'), "{}", t!("tui.ctrl.one_per_line"));
        match self {
            Entry::Text | Entry::Key => Ok(text.to_owned()),
            Entry::Level => {
                let level: moc_core::ThinkingLevel = text
                    .parse()
                    .map_err(|error: moc_core::CoreError| anyhow::anyhow!(error.detail()))?;
                anyhow::ensure!(
                    level != moc_core::ThinkingLevel::Off,
                    "{}",
                    t!("tui.ctrl.off_always")
                );
                Ok(level.as_str().to_owned())
            }
        }
    }
}

/// 清空选择时写回的值。
pub enum Empty {
    /// 写空数组。
    Array,
    /// 删除该项，回到 `defaults` 列出的默认集合。
    Default(Vec<String>),
    /// 不允许为空；存 locale 键。
    Reject(&'static str),
}

/// 对象的编辑规格。
pub struct RecordSpec {
    /// `(键, 说明)`：固定行，值可留空。
    pub keys: Vec<(String, String)>,
    /// 允许追加任意键。
    pub custom: bool,
    /// 值按数字解析（价格）。
    pub number: bool,
    /// 填了任何一项时必须一起填写的键。
    pub required: &'static [&'static str],
}

/// 内置思考等级与说明；与 core 未配置 `thinking_levels` 时的默认集合一致。
const LEVELS: [(&str, &str); 5] = [
    ("low", "tui.level.low"),
    ("medium", "tui.level.medium"),
    ("high", "tui.level.high"),
    ("xhigh", "tui.level.xhigh"),
    ("max", "tui.level.max"),
];

/// `(值, 说明)` 表；说明列存 locale 键，构造时翻译。
fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(value, note)| ((*value).to_owned(), translate(note)))
        .collect()
}

/// 说明列：非空时按 locale 键翻译，空串留空（占位行）。
fn translate(note: &str) -> String {
    if note.is_empty() {
        String::new()
    } else {
        t!(note).into_owned()
    }
}

/// 字段的基础控件：只看路径与字段类型，不依赖运行时状态。
pub fn control(field: &ConfigField) -> Control {
    let path = field.path.as_slice();
    if let Some(role) = model_role(path) {
        return Control::Model(role_specials(role));
    }
    let key = path.last().map_or("", String::as_str);
    // 模型条目的直属字段：providers.<p>.models.<m>.<key> 或 defaults.models.<pattern>.<key>。
    let model = match path {
        [root, _, models, _, _] => root == "providers" && models == "models",
        [root, models, _, _] => root == "defaults" && models == "models",
        _ => false,
    };
    let compat = path.iter().any(|part| part == "compat");
    let server = path.len() == 4 && path[0] == "mcp" && path[1] == "servers";
    match key {
        "thinking_levels" if model => Control::List(ListSpec {
            presets: pairs(&LEVELS),
            custom: Some(Entry::Level),
            ordered: true,
            empty: Empty::Default(
                LEVELS
                    .iter()
                    .map(|(level, _)| (*level).to_owned())
                    .collect(),
            ),
        }),
        "input" if model => Control::List(ListSpec {
            presets: pairs(&[
                ("text", "tui.ctrl.input_text"),
                ("image", "tui.ctrl.input_image"),
            ]),
            custom: None,
            ordered: false,
            empty: Empty::Reject("tui.ctrl.keep_one_modality"),
        }),
        "pricing" if model => Control::Record(RecordSpec {
            keys: pairs(&[
                ("input", "tui.ctrl.price_input"),
                ("output", "tui.ctrl.price_output"),
                ("cache_read", "tui.ctrl.price_cache_read"),
                ("cache_write", "tui.ctrl.price_cache_write"),
            ]),
            custom: false,
            number: true,
            required: &["input", "output"],
        }),
        "effort_map" if compat => Control::Record(RecordSpec {
            keys: pairs(&[
                ("none", "tui.level.off"),
                ("minimal", "tui.level.minimal"),
                ("low", "tui.level.low"),
                ("medium", "tui.level.medium"),
                ("high", "tui.level.high"),
                ("xhigh", "tui.level.xhigh"),
                ("max", "tui.level.max"),
            ]),
            custom: false,
            number: false,
            required: &[],
        }),
        "reasoning_field_aliases" if compat => Control::List(ListSpec {
            presets: pairs(&[("reasoning_content", ""), ("reasoning", "")]),
            custom: Some(Entry::Text),
            ordered: true,
            empty: Empty::Array,
        }),
        "endpoint" if path.len() == 3 && path[0] == "providers" => Control::Select(vec![
            Pick::set(json!("Default"), t!("tui.ctrl.default").into_owned()),
            Pick {
                label: "Azure OpenAI".into(),
                action: PickAction::Azure,
            },
        ]),
        "headers" | "env" if matches!(field.kind, FieldKind::Object) => {
            Control::Record(RecordSpec {
                keys: Vec::new(),
                custom: true,
                number: false,
                required: &[],
            })
        }
        "type" if server => Control::Select(vec![
            Pick {
                label: t!("tui.ctrl.auto_detect").into_owned(),
                action: PickAction::Remove,
            },
            Pick::set(json!("stdio"), t!("tui.ctrl.stdio").into_owned()),
            Pick::set(json!("http"), t!("tui.ctrl.http").into_owned()),
        ]),
        _ if path.len() == 3 && path[0] == "tui" && path[1] == "keybindings" => {
            Control::List(ListSpec {
                presets: Vec::new(),
                custom: Some(Entry::Key),
                ordered: true,
                empty: Empty::Array,
            })
        }
        _ => match &field.kind {
            FieldKind::Bool => Control::Toggle,
            FieldKind::OptionalBool => Control::Select(vec![
                Pick {
                    label: t!("tui.ctrl.default_inherit").into_owned(),
                    action: PickAction::Remove,
                },
                Pick::set(json!(true), t!("tui.ctrl.on_true").into_owned()),
                Pick::set(json!(false), t!("tui.ctrl.off_false").into_owned()),
            ]),
            FieldKind::Choice(options) => Control::Select(
                path.iter()
                    .any(|part| part == "extra_body")
                    .then(|| Pick {
                        label: t!("tui.ctrl.default_inherit").into_owned(),
                        action: PickAction::Remove,
                    })
                    .into_iter()
                    .chain(
                        options
                            .iter()
                            .map(|option| Pick::set(json!(option.value), option.label.clone())),
                    )
                    .collect(),
            ),
            FieldKind::Strings => Control::List(ListSpec {
                presets: Vec::new(),
                custom: Some(Entry::Text),
                ordered: true,
                empty: Empty::Array,
            }),
            kind => Control::Input(kind.clone()),
        },
    }
}

/// 打开编辑器时的控件：在基础控件上补运行时才知道的候选。
pub fn resolve(field: &ConfigField, panel: &SettingsPanel, app: &App) -> Control {
    let mut control = control(field);
    let path = field.path.as_slice();
    match &mut control {
        // 已加载的插件名作为「禁用插件」的候选。
        Control::List(spec) if path == ["plugins", "disable"] => {
            spec.presets = app
                .plugins
                .iter()
                .map(|plugin| {
                    (
                        plugin.name.clone(),
                        plugin.description.clone().unwrap_or_default(),
                    )
                })
                .collect();
        }
        // 模型声明的自定义思考等级也要能映射到上游取值。
        Control::Record(spec) if path.last().is_some_and(|key| key == "effort_map") => {
            if let Some(index) = path.iter().position(|part| part == "compat") {
                let mut levels = path[..index].to_vec();
                levels.push("thinking_levels".into());
                for level in panel
                    .value(&levels)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if !spec.keys.iter().any(|(key, _)| key == level) {
                        spec.keys
                            .push((level.to_owned(), t!("tui.ctrl.custom_level").into_owned()));
                    }
                }
            }
        }
        _ => {}
    }
    control
}

/// 字段行上的取值摘要；`None` 交给通用格式化。
pub fn summary(control: &Control, value: &Value) -> Option<String> {
    match control {
        Control::Select(picks) => Some(match current_pick(picks, value) {
            Some(index) => {
                let pick = &picks[index];
                match (&pick.action, value.pointer("/Azure/api_version")) {
                    (PickAction::Azure, Some(version)) => format!(
                        "‹ {} · {} ›",
                        pick.label,
                        version.as_str().unwrap_or_default()
                    ),
                    _ => format!("‹ {} ›", pick.label),
                }
            }
            None if value.is_null() => t!("tui.ctrl.unset").into_owned(),
            None => value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned),
        }),
        Control::List(spec) => Some(match value.as_array() {
            Some(items) if items.is_empty() => t!("tui.ctrl.none").into_owned(),
            Some(items) => {
                let items: Vec<String> = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                if matches!(spec.custom, Some(Entry::Key)) {
                    crate::render::labels::keys(&items)
                } else {
                    items.join(&t!("tui.ctrl.list_sep"))
                }
            }
            None => match &spec.empty {
                Empty::Default(levels) => t!(
                    "tui.ctrl.default_list",
                    items = levels.join(&t!("tui.ctrl.list_sep"))
                )
                .into_owned(),
                _ => t!("tui.ctrl.unset").into_owned(),
            },
        }),
        Control::Record(_) => Some(match value.as_object() {
            Some(entries) if !entries.is_empty() => entries
                .iter()
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned);
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
                .join(" · "),
            Some(_) => t!("tui.ctrl.none").into_owned(),
            None => t!("tui.ctrl.unset").into_owned(),
        }),
        Control::Toggle | Control::Model(_) | Control::Input(_) => None,
    }
}

/// 字段行的操作提示。
pub fn action_hint(control: &Control) -> String {
    match control {
        Control::Toggle => t!("tui.ctrl.hint_toggle"),
        Control::Select(_) => t!("tui.ctrl.hint_select"),
        Control::Model(_) => t!("tui.ctrl.hint_edit"),
        Control::List(spec) if spec.presets.is_empty() => t!("tui.ctrl.hint_edit_enter"),
        Control::List(_) => t!("tui.ctrl.hint_choose"),
        Control::Record(_) => t!("tui.ctrl.hint_edit_enter"),
        Control::Input(_) => t!("tui.ctrl.hint_edit_enter"),
    }
    .into_owned()
}
