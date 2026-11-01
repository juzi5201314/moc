//! 设置页编辑器的状态机：文本框、单选、勾选清单 / 自由列表、键值表。
//!
//! 编辑器只处理按键与草稿，产出 [`Outcome`]；写入配置、撤回栈与错误提示由 forms 负责。

use super::controls::{Empty, Entry, ListSpec, Pick, PickAction, RecordSpec, current_pick};
use super::*;
use crate::widgets::prompt::edit_input;

pub enum Editor {
    Text(TextEditor),
    Select(SelectEditor),
    List(ListEditor),
    Record(RecordEditor),
}

/// 一次按键的结果。
pub enum Outcome {
    /// 继续编辑。
    Stay,
    /// 关闭编辑器，不写入（含「选了原值」）。
    Cancel,
    /// 文本框提交，按字段类型解析后写入。
    Submit { kind: FieldKind, text: String },
    /// 写入该值；`None` 删除该项。
    Value(Option<Value>),
    /// 选中 Azure 端点，接着填写 api-version。
    Azure,
    /// 留在编辑器里并提示错误。
    Error(String),
}

pub struct TextEditor {
    pub kind: FieldKind,
    pub input: PromptState,
    pub reveal: bool,
}

pub struct SelectEditor {
    pub picks: Vec<Pick>,
    pub selected: usize,
    /// 当前值对应的候选。
    pub current: Option<usize>,
}

pub struct ListEditor {
    pub spec: ListSpec,
    pub items: Vec<ListItem>,
    /// `items.len()` 是末尾的「添加」行（允许自定义时）。
    pub cursor: usize,
    pub draft: Option<Draft>,
    initial: Option<Value>,
}

pub struct ListItem {
    pub value: String,
    pub note: String,
    pub checked: bool,
    /// 来自预设：删除时只取消勾选，不从清单里消失。
    pub preset: bool,
}

/// 行内草稿：`index` 为 `None` 时是新增；列表是条目下标，键值表是行下标。
pub struct Draft {
    pub index: Option<usize>,
    pub input: PromptState,
}

pub struct RecordEditor {
    pub spec: RecordSpec,
    pub rows: Vec<RecordRow>,
    /// `rows.len()` 是末尾的「添加键」行（允许自定义键时）。
    pub cursor: usize,
    /// `index` 为 `None` 时在填写新键名，否则在填写该行的值。
    pub draft: Option<Draft>,
    initial: Option<Value>,
}

pub struct RecordRow {
    pub key: String,
    pub note: String,
    pub value: String,
    /// 规格里的固定键：删除时只清空值。
    pub fixed: bool,
}

/// 列表类编辑器的光标移动；`Up`/`Down` 首尾循环。
fn navigate(cursor: usize, rows: usize, code: KeyCode) -> Option<usize> {
    let last = rows.saturating_sub(1);
    Some(match code {
        KeyCode::Up | KeyCode::Char('k') => cursor.checked_sub(1).unwrap_or(last),
        KeyCode::Down | KeyCode::Char('j') => {
            if cursor >= last {
                0
            } else {
                cursor + 1
            }
        }
        KeyCode::PageUp => cursor.saturating_sub(10),
        KeyCode::PageDown => (cursor + 10).min(last),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        _ => return None,
    })
}

/// 结束编辑：值没变就当取消，免得记一条空改动。
fn finish(output: anyhow::Result<Option<Value>>, initial: &Option<Value>) -> Outcome {
    match output {
        Err(error) => Outcome::Error(error.to_string()),
        Ok(value) if value == *initial => Outcome::Cancel,
        Ok(value) => Outcome::Value(value),
    }
}

fn is_save(mods: KeyModifiers, code: KeyCode) -> bool {
    mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('s') | KeyCode::Enter)
}

impl Editor {
    pub fn key(&mut self, mods: KeyModifiers, code: KeyCode) -> Outcome {
        match self {
            Editor::Text(editor) => editor.key(mods, code),
            Editor::Select(editor) => editor.key(code),
            Editor::List(editor) => editor.key(mods, code),
            Editor::Record(editor) => editor.key(mods, code),
        }
    }

    /// 粘贴进正在输入的文本框或草稿。
    pub fn paste(&mut self, text: &str) {
        let input = match self {
            Editor::Text(editor) => &mut editor.input,
            Editor::List(ListEditor {
                draft: Some(draft), ..
            })
            | Editor::Record(RecordEditor {
                draft: Some(draft), ..
            }) => &mut draft.input,
            _ => return,
        };
        input.insert_str_raw(text);
    }

    /// 滚轮：在候选间移动；输入草稿时不动。
    pub fn scroll(&mut self, down: bool) {
        let code = if down { KeyCode::Down } else { KeyCode::Up };
        let (cursor, rows) = match self {
            Editor::Select(editor) => (&mut editor.selected, editor.picks.len()),
            Editor::List(editor) if editor.draft.is_none() => {
                let rows = editor.rows();
                (&mut editor.cursor, rows)
            }
            Editor::Record(editor) if editor.draft.is_none() => {
                let rows = editor.rows();
                (&mut editor.cursor, rows)
            }
            _ => return,
        };
        if let Some(next) = navigate(*cursor, rows, code) {
            *cursor = next;
        }
    }

    /// 点击第 `index` 行：移动光标，并返回等价的按键（单选即选中、清单即勾选）。
    pub fn click(&mut self, index: usize) -> Option<KeyCode> {
        match self {
            Editor::Text(_) => None,
            Editor::Select(editor) => {
                editor.selected = index;
                Some(KeyCode::Enter)
            }
            Editor::List(editor) if editor.draft.is_none() => {
                editor.cursor = index;
                Some(if index < editor.items.len() {
                    KeyCode::Char(' ')
                } else {
                    KeyCode::Enter
                })
            }
            Editor::Record(editor) if editor.draft.is_none() => {
                editor.cursor = index;
                Some(KeyCode::Enter)
            }
            _ => None,
        }
    }

    /// 底部操作提示：`(键, 动作)`，最可省的在前。
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match self {
            Editor::Text(editor) if editor.multiline() => {
                vec![("ctrl+s", "应用"), ("esc", "取消")]
            }
            Editor::Text(editor) => {
                let mut items = Vec::new();
                match editor.kind {
                    FieldKind::Secret => items.push(("f6", "显示/隐藏")),
                    FieldKind::Integer | FieldKind::Number => items.push(("", "支持 200k、1m")),
                    _ => {}
                }
                items.extend([("enter", "应用"), ("esc", "取消")]);
                items
            }
            Editor::Select(_) => vec![("↑↓", "选择"), ("enter", "应用"), ("esc", "取消")],
            Editor::List(editor) => match &editor.draft {
                Some(_) if matches!(editor.spec.custom, Some(Entry::Key)) => {
                    vec![("", "按下要绑定的键"), ("esc", "取消")]
                }
                Some(_) => vec![("enter", "确定"), ("esc", "取消")],
                None => {
                    let mut items = vec![("↑↓", "移动")];
                    if editor.spec.ordered {
                        items.push(("shift+↑↓", "调整顺序"));
                    }
                    if editor.checklist() {
                        items.push(("space", "勾选"));
                    } else {
                        items.push(("space", "修改"));
                    }
                    if editor.spec.custom.is_some() {
                        items.push(("a", "添加"));
                    }
                    items.push(("del", "移除"));
                    // 光标在「添加」行时 Enter 是添加，保存只能用 Ctrl+S。
                    if editor.cursor < editor.items.len() {
                        items.push(("enter", "应用"));
                    } else {
                        items.push(("ctrl+s", "应用"));
                    }
                    items.push(("esc", "取消"));
                    items
                }
            },
            Editor::Record(editor) => match &editor.draft {
                Some(_) => vec![("enter", "确定"), ("ctrl+s", "保存"), ("esc", "取消")],
                None => {
                    let mut items = vec![("↑↓", "移动"), ("enter", "编辑")];
                    if editor.spec.custom {
                        items.push(("a", "添加键"));
                    }
                    items.extend([("del", "清除"), ("ctrl+s", "保存"), ("esc", "取消")]);
                    items
                }
            },
        }
    }
}

impl TextEditor {
    pub fn new(kind: FieldKind, text: String) -> Self {
        Self {
            kind,
            input: PromptState::from_text(text),
            reveal: false,
        }
    }

    pub fn multiline(&self) -> bool {
        matches!(self.kind, FieldKind::Multiline | FieldKind::Object)
    }

    fn key(&mut self, mods: KeyModifiers, code: KeyCode) -> Outcome {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        if code == KeyCode::Esc {
            return Outcome::Cancel;
        }
        if code == KeyCode::F(6) || ctrl && matches!(code, KeyCode::Char('r' | 'R')) {
            self.reveal = !self.reveal;
            return Outcome::Stay;
        }
        if code == KeyCode::Enter && (ctrl || !self.multiline()) || is_save(mods, code) {
            return Outcome::Submit {
                kind: self.kind.clone(),
                text: self.input.text.clone(),
            };
        }
        edit_input(&mut self.input, mods, code);
        Outcome::Stay
    }
}

impl SelectEditor {
    pub fn new(picks: Vec<Pick>, value: &Value) -> Self {
        let current = current_pick(&picks, value);
        Self {
            selected: current.unwrap_or(0),
            current,
            picks,
        }
    }

    fn key(&mut self, code: KeyCode) -> Outcome {
        match code {
            KeyCode::Esc => Outcome::Cancel,
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(pick) = self.picks.get(self.selected) else {
                    return Outcome::Cancel;
                };
                match &pick.action {
                    // 已是 Azure 也要能改 api-version。
                    PickAction::Azure => Outcome::Azure,
                    _ if self.current == Some(self.selected) => Outcome::Cancel,
                    PickAction::Set(value) => Outcome::Value(Some(value.clone())),
                    PickAction::Remove => Outcome::Value(None),
                }
            }
            _ => {
                if let Some(next) = navigate(self.selected, self.picks.len(), code) {
                    self.selected = next;
                }
                Outcome::Stay
            }
        }
    }
}

impl ListEditor {
    pub fn new(spec: ListSpec, value: &Value) -> Self {
        let chosen: Vec<String> = match value.as_array() {
            Some(items) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            None => match &spec.empty {
                Empty::Default(defaults) => defaults.clone(),
                Empty::Array | Empty::Reject(_) => Vec::new(),
            },
        };
        let mut items: Vec<ListItem> = Vec::new();
        let add = |items: &mut Vec<ListItem>, value: &str, checked: bool| {
            if items.iter().any(|item| item.value == value) {
                return;
            }
            let note = spec
                .presets
                .iter()
                .find(|(preset, _)| preset == value)
                .map(|(_, note)| note.clone());
            items.push(ListItem {
                value: value.to_owned(),
                preset: note.is_some(),
                note: note.unwrap_or_default(),
                checked,
            });
        };
        // 有序：已选项按配置顺序排在前面；无序：预设保持固定顺序。
        if spec.ordered {
            for value in &chosen {
                add(&mut items, value, true);
            }
        }
        for (preset, _) in &spec.presets {
            add(&mut items, preset, chosen.contains(preset));
        }
        for value in &chosen {
            add(&mut items, value, true);
        }
        let mut editor = Self {
            spec,
            items,
            cursor: 0,
            draft: None,
            initial: None,
        };
        editor.initial = editor.output().ok().flatten();
        editor
    }

    /// 有预设时是勾选清单；否则是逐项增删的自由列表。
    pub fn checklist(&self) -> bool {
        !self.spec.presets.is_empty()
    }

    pub fn rows(&self) -> usize {
        self.items.len() + usize::from(self.spec.custom.is_some())
    }

    /// 勾选项按显示顺序写回。
    pub fn output(&self) -> anyhow::Result<Option<Value>> {
        let chosen: Vec<&str> = self
            .items
            .iter()
            .filter(|item| item.checked)
            .map(|item| item.value.as_str())
            .collect();
        if chosen.is_empty() {
            return match &self.spec.empty {
                Empty::Array => Ok(Some(json!([]))),
                Empty::Default(_) => Ok(None),
                Empty::Reject(message) => Err(anyhow::anyhow!(*message)),
            };
        }
        Ok(Some(json!(chosen)))
    }

    fn key(&mut self, mods: KeyModifiers, code: KeyCode) -> Outcome {
        if self.draft.is_some() {
            return self.draft_key(mods, code);
        }
        if is_save(mods, code) {
            return finish(self.output(), &self.initial);
        }
        let on_item = self.cursor < self.items.len();
        match code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Up | KeyCode::Down if mods.contains(KeyModifiers::SHIFT) => {
                self.reorder(code == KeyCode::Down);
            }
            KeyCode::Char('K') => self.reorder(false),
            KeyCode::Char('J') => self.reorder(true),
            KeyCode::Char(' ') if on_item && self.checklist() => {
                let item = &mut self.items[self.cursor];
                item.checked = !item.checked;
            }
            KeyCode::Char(' ') | KeyCode::Char('e') | KeyCode::F(2) if on_item => {
                self.edit_current();
            }
            KeyCode::Enter if on_item => return finish(self.output(), &self.initial),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('a') => self.new_draft(),
            KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d' | 'x') if on_item => {
                self.remove_current();
            }
            _ => {
                if let Some(next) = navigate(self.cursor, self.rows(), code) {
                    self.cursor = next;
                }
            }
        }
        Outcome::Stay
    }

    fn reorder(&mut self, down: bool) {
        if !self.spec.ordered || self.cursor >= self.items.len() {
            return;
        }
        let target = if down {
            self.cursor + 1
        } else {
            match self.cursor.checked_sub(1) {
                Some(target) => target,
                None => return,
            }
        };
        if target < self.items.len() {
            self.items.swap(self.cursor, target);
            self.cursor = target;
        }
    }

    fn new_draft(&mut self) {
        if self.spec.custom.is_some() {
            self.draft = Some(Draft {
                index: None,
                input: PromptState::default(),
            });
        }
    }

    /// 修改自定义项；预设项的值固定，不能改写。
    fn edit_current(&mut self) {
        if let Some(item) = self.items.get(self.cursor)
            && !item.preset
        {
            let text = match self.spec.custom {
                Some(Entry::Key) => String::new(),
                _ => item.value.clone(),
            };
            self.draft = Some(Draft {
                index: Some(self.cursor),
                input: PromptState::from_text(text),
            });
        }
    }

    fn remove_current(&mut self) {
        let item = &mut self.items[self.cursor];
        if item.preset {
            item.checked = false;
        } else {
            self.items.remove(self.cursor);
            self.cursor = self.cursor.min(self.rows().saturating_sub(1));
        }
    }

    fn draft_key(&mut self, mods: KeyModifiers, code: KeyCode) -> Outcome {
        let entry = self.spec.custom.expect("只有允许自定义的清单才会进入草稿");
        if code == KeyCode::Esc {
            self.draft = None;
            return Outcome::Stay;
        }
        let text = match entry {
            Entry::Key => match crate::config::format_key(mods, code) {
                Some(text) => text,
                // 单独按下修饰键：继续等完整的组合键。
                None if matches!(code, KeyCode::Modifier(_)) => return Outcome::Stay,
                None => return Outcome::Error("这个键无法写进配置，请换一个".into()),
            },
            Entry::Text | Entry::Level => {
                let draft = self.draft.as_mut().expect("已在草稿态");
                if code != KeyCode::Enter && !is_save(mods, code) {
                    edit_input(&mut draft.input, mods, code);
                    return Outcome::Stay;
                }
                draft.input.text.clone()
            }
        };
        match self.commit(entry, &text) {
            Ok(()) => Outcome::Stay,
            Err(error) => Outcome::Error(error.to_string()),
        }
    }

    /// 草稿落入清单：新增项排在最后一个已选项之后并自动勾选；与未勾选的预设同名时直接勾选它。
    fn commit(&mut self, entry: Entry, text: &str) -> anyhow::Result<()> {
        let value = entry.validate(text)?;
        let editing = self.draft.as_ref().and_then(|draft| draft.index);
        match (
            self.items.iter().position(|item| item.value == value),
            editing,
        ) {
            (Some(found), editing) if Some(found) != editing => {
                let item = &mut self.items[found];
                anyhow::ensure!(
                    editing.is_none() && item.preset && !item.checked,
                    "“{value}”已在列表中"
                );
                item.checked = true;
                self.cursor = found;
            }
            (_, Some(index)) => {
                self.items[index].value = value;
                self.cursor = index;
            }
            (_, None) => {
                let at = self
                    .items
                    .iter()
                    .rposition(|item| item.checked)
                    .map_or(0, |index| index + 1);
                self.items.insert(
                    at,
                    ListItem {
                        value,
                        note: String::new(),
                        checked: true,
                        preset: false,
                    },
                );
                self.cursor = at;
            }
        }
        self.draft = None;
        Ok(())
    }
}

/// 键值表里显示的文本：字符串去引号，其余按 JSON。
fn plain(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

impl RecordEditor {
    pub fn new(spec: RecordSpec, value: &Value) -> Self {
        let object = value.as_object();
        let mut rows: Vec<RecordRow> = spec
            .keys
            .iter()
            .map(|(key, note)| RecordRow {
                key: key.clone(),
                note: note.clone(),
                value: object
                    .and_then(|object| object.get(key))
                    .filter(|value| !value.is_null())
                    .map_or_default(plain),
                fixed: true,
            })
            .collect();
        for (key, value) in object.into_iter().flatten() {
            if !rows.iter().any(|row| row.key == *key) {
                rows.push(RecordRow {
                    key: key.clone(),
                    note: String::new(),
                    value: plain(value),
                    fixed: false,
                });
            }
        }
        let mut editor = Self {
            spec,
            rows,
            cursor: 0,
            draft: None,
            initial: None,
        };
        editor.initial = editor.output().ok().flatten();
        editor
    }

    pub fn rows(&self) -> usize {
        self.rows.len() + usize::from(self.spec.custom)
    }

    /// 留空的行不写入；整张表都空时删除该项。
    pub fn output(&self) -> anyhow::Result<Option<Value>> {
        let mut object = serde_json::Map::new();
        for row in &self.rows {
            let text = row.value.trim();
            if text.is_empty() {
                continue;
            }
            let value = if self.spec.number {
                forms::parse(text, &FieldKind::Number)?
            } else {
                Some(json!(text))
            };
            if let Some(value) = value {
                object.insert(row.key.clone(), value);
            }
        }
        if object.is_empty() {
            return Ok(None);
        }
        let missing: Vec<&str> = self
            .spec
            .required
            .iter()
            .copied()
            .filter(|key| !object.contains_key(*key))
            .collect();
        anyhow::ensure!(missing.is_empty(), "还需要填写：{}", missing.join("、"));
        Ok(Some(Value::Object(object)))
    }

    fn key(&mut self, mods: KeyModifiers, code: KeyCode) -> Outcome {
        let save = is_save(mods, code);
        if let Some(draft) = &mut self.draft {
            if code == KeyCode::Esc {
                self.draft = None;
                return Outcome::Stay;
            }
            if code != KeyCode::Enter && !save {
                edit_input(&mut draft.input, mods, code);
                return Outcome::Stay;
            }
            if let Err(error) = self.commit() {
                return Outcome::Error(error.to_string());
            }
            return if save {
                finish(self.output(), &self.initial)
            } else {
                Outcome::Stay
            };
        }
        if save {
            return finish(self.output(), &self.initial);
        }
        let on_row = self.cursor < self.rows.len();
        match code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter | KeyCode::Char(' ') if on_row => {
                self.draft = Some(Draft {
                    index: Some(self.cursor),
                    input: PromptState::from_text(self.rows[self.cursor].value.clone()),
                });
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('a') if self.spec.custom => {
                self.draft = Some(Draft {
                    index: None,
                    input: PromptState::default(),
                });
            }
            KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d' | 'x') if on_row => {
                if self.rows[self.cursor].fixed {
                    self.rows[self.cursor].value.clear();
                } else {
                    self.rows.remove(self.cursor);
                    self.cursor = self.cursor.min(self.rows().saturating_sub(1));
                }
            }
            _ => {
                if let Some(next) = navigate(self.cursor, self.rows(), code) {
                    self.cursor = next;
                }
            }
        }
        Outcome::Stay
    }

    /// 草稿落表：值按需校验；新键名落表后接着填它的值。
    fn commit(&mut self) -> anyhow::Result<()> {
        let draft = self.draft.as_ref().expect("已在草稿态");
        let (index, text) = (draft.index, draft.input.text.trim().to_owned());
        match index {
            Some(row) => {
                if self.spec.number && !text.is_empty() {
                    forms::parse(&text, &FieldKind::Number)?;
                }
                self.rows[row].value = text;
                self.draft = None;
            }
            None => {
                anyhow::ensure!(
                    !text.is_empty() && !text.contains('\n'),
                    "键名不能为空，也不能换行"
                );
                anyhow::ensure!(
                    !self.rows.iter().any(|row| row.key == text),
                    "键 {text} 已存在"
                );
                self.rows.push(RecordRow {
                    key: text,
                    note: String::new(),
                    value: String::new(),
                    fixed: false,
                });
                self.cursor = self.rows.len() - 1;
                self.draft = Some(Draft {
                    index: Some(self.cursor),
                    input: PromptState::default(),
                });
            }
        }
        Ok(())
    }
}
