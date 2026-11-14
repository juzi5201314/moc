//! 只提取展示字段的增量 JSON 扫描器；不构造临时可执行参数。
use moc_protocol::ToolPreviewContent;
use std::collections::VecDeque;

const BODY_BYTES: usize = 32 * 1024;
const META_BYTES: usize = 1024;

/// 尾部窗口按字符淘汰，保证 UTF-8 边界；每个字符只入队、出队一次。
#[derive(Default)]
struct Tail {
    chars: VecDeque<char>,
    bytes: usize,
    truncated: bool,
}
impl Tail {
    fn push(&mut self, c: char, limit: usize) {
        // 展示内容不能把终端控制序列带入 TUI。
        let c = if c.is_control() && !matches!(c, '\n' | '\t') {
            '�'
        } else {
            c
        };
        self.bytes += c.len_utf8();
        self.chars.push_back(c);
        while self.bytes > limit {
            self.bytes -= self.chars.pop_front().expect("窗口非空").len_utf8();
            self.truncated = true;
        }
    }
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
}

#[derive(Clone, Copy, Default)]
enum Field {
    #[default]
    Ignore,
    Path,
    Cwd,
    Description,
    Command,
    Code,
    Content,
    Edit,
}
#[derive(Clone, Copy, PartialEq)]
enum Context {
    Root,
    Edits,
    Edit,
    Move,
    Other,
}
struct Frame {
    context: Context,
    object: bool,
    key: String,
    wants_key: bool,
}

#[derive(Default)]
pub(super) struct JsonPreview {
    stack: Vec<Frame>,
    string: bool,
    key: bool,
    escaped: bool,
    unicode: Option<(u16, u8)>,
    high: Option<u16>,
    key_buffer: String,
    scalar: bool,
    field: Field,
    disabled: bool,
    raw: Tail,
    path: Tail,
    cwd: Tail,
    description: Tail,
    command: Tail,
    code: Tail,
    content: Tail,
    edit: Tail,
}

impl JsonPreview {
    pub(super) fn feed(&mut self, input: &str) {
        for c in input.chars() {
            self.raw.push(c, BODY_BYTES);
            if self.disabled {
                continue;
            }
            if self.string {
                self.string_char(c);
                continue;
            }
            if self.scalar {
                if matches!(c, ',' | '}' | ']') || c.is_whitespace() {
                    self.scalar = false;
                } else {
                    self.output(c);
                    continue;
                }
            }
            match c {
                '"' => {
                    self.key = self.stack.last().is_some_and(|f| f.object && f.wants_key);
                    self.key_buffer.clear();
                    self.escaped = false;
                    self.unicode = None;
                    self.high = None;
                    if !self.key {
                        self.begin_value();
                    }
                    self.string = true;
                }
                '{' | '[' => {
                    // JSON 执行校验独立进行；展示的嵌套栈严格限制在 128 层。
                    if self.stack.len() == 128 {
                        self.disabled = true;
                        continue;
                    }
                    let context = match self.stack.last() {
                        None if c == '{' => Context::Root,
                        Some(f) if f.context == Context::Root && f.key == "edits" && c == '[' => {
                            Context::Edits
                        }
                        Some(f) if f.context == Context::Edits && c == '{' => Context::Edit,
                        Some(f) if f.context == Context::Root && f.key == "move" && c == '{' => {
                            Context::Move
                        }
                        _ => Context::Other,
                    };
                    if matches!(context, Context::Edit | Context::Move) {
                        for c in "\n---\n".chars() {
                            self.edit.push(c, BODY_BYTES);
                        }
                    }
                    self.stack.push(Frame {
                        context,
                        object: c == '{',
                        key: String::new(),
                        wants_key: c == '{',
                    });
                    debug_assert!(self.stack.len() <= 128);
                }
                '}' | ']' => {
                    self.stack.pop();
                }
                ':' => {
                    if let Some(f) = self.stack.last_mut() {
                        f.wants_key = false;
                    }
                }
                ',' => {
                    if let Some(f) = self.stack.last_mut() {
                        f.wants_key = f.object;
                    }
                }
                c if c.is_whitespace() => {}
                _ => {
                    self.begin_value();
                    self.scalar = true;
                    self.output(c);
                }
            }
        }
    }

    fn begin_value(&mut self) {
        self.field = match self.stack.last() {
            Some(f) if f.context == Context::Root => match f.key.as_str() {
                "path" => Field::Path,
                "cwd" => Field::Cwd,
                "description" => Field::Description,
                "command" => Field::Command,
                "code" => Field::Code,
                "content" => Field::Content,
                _ => Field::Ignore,
            },
            Some(f) if matches!(f.context, Context::Edit | Context::Move) => {
                if matches!(
                    f.key.as_str(),
                    "op" | "from" | "to" | "after" | "text" | "target_path"
                ) {
                    for c in format!("\n{}: ", f.key).chars() {
                        self.edit.push(c, BODY_BYTES);
                    }
                    Field::Edit
                } else {
                    Field::Ignore
                }
            }
            _ => Field::Ignore,
        };
        match self.field {
            Field::Path => self.path = Tail::default(),
            Field::Cwd => self.cwd = Tail::default(),
            Field::Description => self.description = Tail::default(),
            Field::Command => self.command = Tail::default(),
            Field::Code => self.code = Tail::default(),
            Field::Content => self.content = Tail::default(),
            Field::Ignore | Field::Edit => {}
        }
    }

    fn output(&mut self, c: char) {
        if self.key {
            if self.key_buffer.len() < META_BYTES {
                self.key_buffer.push(c);
            }
            return;
        }
        let (tail, limit) = match self.field {
            Field::Path => (&mut self.path, META_BYTES),
            Field::Cwd => (&mut self.cwd, META_BYTES),
            Field::Description => (&mut self.description, META_BYTES),
            Field::Command => (&mut self.command, BODY_BYTES),
            Field::Code => (&mut self.code, BODY_BYTES),
            Field::Content => (&mut self.content, BODY_BYTES),
            Field::Edit => (&mut self.edit, BODY_BYTES),
            Field::Ignore => return,
        };
        tail.push(c, limit);
    }

    fn string_char(&mut self, c: char) {
        if let Some((value, digits)) = self.unicode {
            let Some(digit) = c.to_digit(16) else {
                self.disabled = true;
                return;
            };
            let value = (value << 4) | digit as u16;
            if digits < 3 {
                self.unicode = Some((value, digits + 1));
                return;
            }
            self.unicode = None;
            if (0xD800..=0xDBFF).contains(&value) {
                self.high = Some(value);
            } else {
                let point = if let Some(high) = self.high.take() {
                    if !(0xDC00..=0xDFFF).contains(&value) {
                        self.disabled = true;
                        return;
                    }
                    0x10000 + ((u32::from(high) - 0xD800) << 10) + u32::from(value) - 0xDC00
                } else {
                    u32::from(value)
                };
                if let Some(c) = char::from_u32(point) {
                    self.output(c);
                }
            }
            return;
        }
        if self.escaped {
            self.escaped = false;
            let c = match c {
                'u' => {
                    self.unicode = Some((0, 0));
                    return;
                }
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'b' => '\u{8}',
                'f' => '\u{c}',
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                _ => {
                    self.disabled = true;
                    return;
                }
            };
            self.output(c);
        } else {
            match c {
                '\\' => self.escaped = true,
                '"' => {
                    self.string = false;
                    if self.key {
                        if let Some(f) = self.stack.last_mut() {
                            f.key = std::mem::take(&mut self.key_buffer);
                        }
                    }
                    self.key = false;
                }
                _ => self.output(c),
            }
        }
    }

    pub(super) fn content(&self, name: &str) -> ToolPreviewContent {
        let (body, language) = match name {
            "bash" => (&self.command, Some("bash".into())),
            "execute" => (&self.code, Some("javascript".into())),
            "write" => (&self.content, None),
            "edit" => (&self.edit, None),
            _ => (&self.raw, Some("json".into())),
        };
        let target = if self.path.bytes > 0 {
            &self.path
        } else {
            &self.cwd
        };
        ToolPreviewContent {
            target: target.text(),
            summary: self.description.text(),
            body: body.text(),
            language,
            truncated: body.truncated || target.truncated || self.description.truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_json_chunk_boundaries_and_metadata() {
        let input = r#"{"code":"中文\n\"x\"\\\uD83D\uDE00","description":"说明","path":"a.rs"}"#;
        for split in input.char_indices().map(|(i, _)| i).chain([input.len()]) {
            let mut p = JsonPreview::default();
            p.feed(&input[..split]);
            p.feed(&input[split..]);
            let view = p.content("execute");
            assert_eq!(view.body, "中文\n\"x\"\\😀", "split={split}");
            assert_eq!(view.target, "a.rs");
            assert_eq!(view.summary, "说明");
        }
        let mut p = JsonPreview::default();
        for c in input.chars() {
            p.feed(&c.to_string());
        }
        assert_eq!(p.content("execute").body, "中文\n\"x\"\\😀");
    }

    #[test]
    fn preview_json_edits_and_bounded_tail() {
        let mut p = JsonPreview::default();
        p.feed(r#"{"edits":[{"op":"delete","from":2,"to":3},{"op":"insert","after":4,"text":"新行"}],"move":{"target_path":"new.rs"},"path":"old.rs"}"#);
        let view = p.content("edit");
        for text in ["delete", "from: 2", "to: 3", "新行", "new.rs"] {
            assert!(view.body.contains(text), "{text}: {}", view.body);
        }
        assert_eq!(view.target, "old.rs");
        let mut p = JsonPreview::default();
        p.feed(r#"{"content":""#);
        for _ in 0..BODY_BYTES {
            p.feed("中");
        }
        p.feed(r#"","path":"late.rs"}"#);
        let view = p.content("write");
        assert!(view.body.len() <= BODY_BYTES);
        assert!(view.truncated);
        assert_eq!(view.target, "late.rs");
    }
}
