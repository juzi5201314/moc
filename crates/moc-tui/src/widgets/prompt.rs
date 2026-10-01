//! 输入框状态与编辑逻辑：多行编辑、历史、粘贴 chip 与图片 chip。
//!
//! 大粘贴（≥ 4 行或 10 KB）收成 `[paste#N]` 占位，再粘相同内容展开为原文；
//! 图片收成 `[image#N]` 占位，发送时拆成 `InputPart::Image`。占位是普通
//! 文本——光标、选择、发送展开都以纯文本运算，不需要幽灵字符协议。

use moc_protocol::ImageSource;
use moc_protocol::methods::InputPart;

/// 粘贴超过该行数时 chip 化。
const PASTE_CHIP_LINES: usize = 4;
/// 粘贴超过该字节数时 chip 化。
const PASTE_CHIP_BYTES: usize = 10_000;
/// 历史上限。
const HISTORY_MAX: usize = 200;

/// 文本粘贴 chip。
#[derive(Debug, Clone)]
struct PasteChip {
    id: u32,
    content: String,
}

/// 图片 chip（剪贴板读到的图像）。
#[derive(Debug, Clone)]
pub struct ImageChip {
    pub id: u32,
    pub media_type: String,
    /// base64 载荷。
    pub data: String,
}

/// 输入框状态。
#[derive(Default)]
pub struct PromptState {
    pub text: String,
    /// 光标（字节偏移，恒在字符边界）。
    pub cursor: usize,
    pastes: Vec<PasteChip>,
    pub images: Vec<ImageChip>,
    histories: Vec<String>,
    /// 当前浏览的历史位置；`None` = 编辑最新输入。
    history_cursor: Option<usize>,
    /// 浏览历史时暂存的编辑中草稿。
    stash: String,
    next_chip_id: u32,
    /// 最近一次粘贴的内容（再粘展开用）。
    last_paste: Option<String>,
}

impl PromptState {
    pub fn from_text(text: String) -> Self {
        let cursor = text.len();
        Self {
            text,
            cursor,
            ..Default::default()
        }
    }
    /// 移出当前草稿而不复制图片，也不带走输入历史。
    pub fn take_draft(&mut self) -> Self {
        self.break_history_browse();
        Self {
            text: std::mem::take(&mut self.text),
            cursor: std::mem::take(&mut self.cursor),
            pastes: std::mem::take(&mut self.pastes),
            images: std::mem::take(&mut self.images),
            next_chip_id: self.next_chip_id,
            last_paste: self.last_paste.take(),
            ..Default::default()
        }
    }

    pub fn restore_draft(&mut self, mut draft: Self) {
        std::mem::swap(&mut self.histories, &mut draft.histories);
        *self = draft;
    }

    /// Steering 回执到达之前保留原草稿，失败或轮次已结束时仍可编辑、排队。
    pub fn preview_parts(&self) -> Vec<InputPart> {
        Self {
            text: self.text.clone(),
            cursor: self.cursor,
            pastes: self.pastes.clone(),
            images: self.images.clone(),
            next_chip_id: self.next_chip_id,
            ..Default::default()
        }
        .build_input_parts()
    }
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    fn chip_id(&mut self) -> u32 {
        self.next_chip_id += 1;
        self.next_chip_id
    }

    /// 插入一个字符。
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.break_history_browse();
    }

    /// 插入一段文本（bracketed paste 的文本分支在此判定 chip 化）。
    pub fn insert_text(&mut self, content: &str) {
        let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
        // 与最近一次粘贴相同：展开最近的 paste chip（repaste-to-expand）
        if self.last_paste.as_deref() == Some(normalized.as_str()) && self.expand_latest_paste() {
            self.last_paste = None;
            return;
        }
        if normalized.matches('\n').count() + 1 >= PASTE_CHIP_LINES
            || normalized.len() > PASTE_CHIP_BYTES
        {
            let id = self.chip_id();
            self.pastes.push(PasteChip {
                id,
                content: normalized,
            });
            let placeholder = format!("[paste#{id}]");
            self.insert_str_raw(&placeholder);
            self.last_paste = self.pastes.last().map(|chip| chip.content.clone());
        } else {
            self.insert_str_raw(&normalized);
        }
    }

    /// 注册一张剪贴板图片并插入占位。
    pub fn insert_image(&mut self, media_type: String, base64_data: String) {
        let id = self.chip_id();
        self.images.push(ImageChip {
            id,
            media_type,
            data: base64_data,
        });
        let placeholder = format!("[image#{id}]");
        self.insert_str_raw(&placeholder);
    }

    pub(crate) fn insert_str_raw(&mut self, content: &str) {
        self.text.insert_str(self.cursor, content);
        self.cursor += content.len();
        self.break_history_browse();
    }
    /// 用补全候选给出的范围替换文本；URI namespace 不会提前插入空格。
    pub fn replace_range(
        &mut self,
        range: std::ops::Range<usize>,
        replacement: &str,
        append_space: bool,
    ) {
        self.text.replace_range(range.clone(), replacement);
        self.cursor = range.start + replacement.len();
        if append_space
            && (self.cursor == self.text.len()
                || self.text.as_bytes().get(self.cursor) != Some(&b' '))
        {
            self.text.insert(self.cursor, ' ');
            self.cursor += 1;
        }
        self.break_history_browse();
    }

    /// 用补全命令替换当前 token，不破坏多行草稿的其余内容。
    pub fn replace_current_token(&mut self, replacement: &str) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let token_start = self.text[line_start..self.cursor]
            .rfind(char::is_whitespace)
            .map_or(line_start, |index| line_start + index + 1);
        let token_end = self.text[self.cursor..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |index| self.cursor + index);
        self.replace_range(token_start..token_end, replacement, true);
    }

    /// 光标移到当前逻辑行行首。
    pub fn move_line_start(&mut self) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = line_start;
    }

    /// 光标移到当前逻辑行行尾。
    pub fn move_line_end(&mut self) {
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        self.cursor = line_end;
    }

    /// 展开最近插入的 paste 占位为原文。
    fn expand_latest_paste(&mut self) -> bool {
        let Some(chip) = self.pastes.last() else {
            return false;
        };
        let placeholder = format!("[paste#{}]", chip.id);
        let Some(position) = self.text.find(&placeholder) else {
            return false;
        };
        let content = chip.content.clone();
        self.pastes.pop();
        let start = position;
        let end = position + placeholder.len();
        self.text.replace_range(start..end, &content);
        self.cursor = start + content.len();
        true
    }

    /// 删除光标前一个字符；光标前恰是 chip 占位时删除整个占位。
    pub fn backspace(&mut self) {
        let Some(start) = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        else {
            return;
        };
        let mut delete_start = start;
        if let Some(chip_end) = self.chip_ending_at(self.cursor) {
            delete_start = chip_end.0;
        }
        self.text.replace_range(delete_start..self.cursor, "");
        self.cursor = delete_start;
        self.break_history_browse();
    }

    /// 删除光标处字符。
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let end = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(index, _)| self.cursor + index)
                .unwrap_or(self.text.len());
            self.text.replace_range(self.cursor..end, "");
            self.break_history_browse();
        }
    }

    /// 光标前是否恰好结束一个 chip 占位；返回 (start, end)。
    fn chip_ending_at(&self, cursor: usize) -> Option<(usize, usize)> {
        let prefix = &self.text[..cursor];
        for marker in ["[paste#", "[image#"] {
            let Some(index) = prefix.rfind(marker) else {
                continue;
            };
            let rest = &prefix[index..];
            let valid = rest
                .strip_prefix(marker)
                .and_then(|value| value.strip_suffix(']'))
                .is_some_and(|id| !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit()));
            if valid && rest.ends_with(']') {
                return Some((index, cursor));
            }
        }
        None
    }

    pub fn left(&mut self) {
        if let Some(previous) = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        {
            self.cursor = previous;
        }
    }

    pub fn right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(index, _)| self.cursor + index)
                .unwrap_or(self.text.len());
        }
    }

    /// 光标所在的逻辑行号。
    fn cursor_line(&self) -> usize {
        self.text[..self.cursor].matches('\n').count()
    }

    fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    /// 上：光标在首行时翻历史，否则行内上移。
    pub fn up(&mut self) -> bool {
        if self.cursor_line() == 0 && self.history_up() {
            return true;
        }
        self.move_line(-1);
        true
    }

    /// 下：光标在末行时翻历史，否则行内下移。
    pub fn down(&mut self) -> bool {
        if self.cursor_line() + 1 == self.line_count() && self.history_down() {
            return true;
        }
        self.move_line(1);
        true
    }

    /// 在文本内按行移动光标（保持列近似）。
    fn move_line(&mut self, delta: i32) {
        let line = self.cursor_line();
        let target = line as i32 + delta;
        if target < 0 || target as usize >= self.line_count() {
            return;
        }
        let column = self.text[..self.cursor]
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count();
        let lines: Vec<&str> = self.text.split('\n').collect();
        let target_line = lines[target as usize];
        let offset_in_line: usize = target_line.chars().take(column).map(char::len_utf8).sum();
        let line_start: usize = lines[..target as usize].iter().map(|l| l.len() + 1).sum();
        self.cursor = line_start + offset_in_line.min(target_line.len());
    }

    /// 历史：上一条。
    pub fn history_up(&mut self) -> bool {
        if self.histories.is_empty() {
            return false;
        }
        let next = match self.history_cursor {
            None => {
                self.stash = self.text.clone();
                0
            }
            Some(index) if index + 1 < self.histories.len() => index + 1,
            Some(_) => return false,
        };
        self.history_cursor = Some(next);
        self.text = self.histories[next].clone();
        self.cursor = self.text.len();
        true
    }

    /// 历史：下一条；回到编辑位置。
    pub fn history_down(&mut self) -> bool {
        let Some(index) = self.history_cursor else {
            return false;
        };
        self.history_cursor = if index == 0 {
            self.text = self.stash.clone();
            None
        } else {
            self.text = self.histories[index - 1].clone();
            Some(index - 1)
        };
        self.cursor = self.text.len();
        true
    }

    fn break_history_browse(&mut self) {
        if self.history_cursor.is_some() {
            self.history_cursor = None;
            self.stash.clear();
        }
    }

    /// 发送：把当前输入压入历史并清空，返回展开后的文本。
    pub fn take_committed_text(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.break_history_browse();
        if !text.trim().is_empty()
            && self.histories.first().map(|h| h.as_str()) != Some(text.as_str())
        {
            self.histories.insert(0, text.clone());
            self.histories.truncate(HISTORY_MAX);
        }
        self.expand_all_chips(text)
    }

    /// 展开文本里的全部占位。
    fn expand_all_chips(&self, mut text: String) -> String {
        for chip in &self.pastes {
            let placeholder = format!("[paste#{}]", chip.id);
            text = text.replace(&placeholder, &chip.content);
        }
        text
    }

    /// 构造发送的 `InputPart` 序列：文本按图片占位切分。
    pub fn build_input_parts(&mut self) -> Vec<InputPart> {
        let text = self.take_committed_text();
        let mut parts = Vec::new();
        let mut plain = String::new();
        let mut rest = text.as_str();
        while let Some(index) = rest.find("[image#") {
            let Some(end) = rest[index..].find(']') else {
                break;
            };
            let marker = &rest[index..index + end + 1];
            let id: Option<u32> = marker
                .trim_start_matches("[image#")
                .trim_end_matches(']')
                .parse()
                .ok();
            if let Some(id) = id
                && let Some(chip) = self.images.iter().find(|chip| chip.id == id)
            {
                plain.push_str(&rest[..index]);
                if !plain.trim().is_empty() {
                    parts.push(InputPart::Text {
                        text: std::mem::take(&mut plain),
                    });
                }
                parts.push(InputPart::Image {
                    source: ImageSource::Base64 {
                        media_type: chip.media_type.clone(),
                        data: chip.data.clone(),
                    },
                    detail: None,
                });
                rest = &rest[index + end + 1..];
                continue;
            }
            plain.push_str(&rest[..index + end + 1]);
            rest = &rest[index + end + 1..];
        }

        plain.push_str(rest);
        if !plain.trim().is_empty() || parts.is_empty() {
            parts.push(InputPart::Text { text: plain });
        }
        self.images.clear();
        parts
    }

    /// 清空全部输入（双 Esc）。
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.images.clear();
        self.break_history_browse();
    }

    pub fn clear_line_to_end(&mut self) {
        let end = self.text[self.cursor..]
            .find('\n')
            .map(|index| self.cursor + index)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..end, "");
        self.break_history_browse();
    }

    pub fn delete_word_back(&mut self) {
        // 回退到上一个空白边界（仅按 ASCII 空白分词，够用于编辑辅助）
        let bytes = self.text.as_bytes();
        let mut position = self.cursor;
        while position > 0 && bytes[position - 1].is_ascii_whitespace() {
            position -= 1;
        }
        while position > 0 && !bytes[position - 1].is_ascii_whitespace() {
            position -= 1;
        }
        self.text.replace_range(position..self.cursor, "");
        self.cursor = position;
        self.break_history_browse();
    }
}

#[cfg(test)]
mod tests {
    use super::PromptState;

    #[test]
    fn completion_replaces_token_without_destroying_multiline_draft() {
        let mut prompt = PromptState::default();
        prompt.insert_str_raw("/mo\n保留内容");
        prompt.cursor = 3;
        prompt.replace_current_token("/model");
        assert_eq!(prompt.text, "/model \n保留内容");
        assert_eq!(prompt.cursor, 7);
    }

    #[test]
    fn wide_character_cursor_moves_on_utf8_boundaries() {
        let mut prompt = PromptState::default();
        prompt.insert_str_raw("中文a");
        prompt.left();
        assert_eq!(&prompt.text[..prompt.cursor], "中文");
        prompt.right();
        assert_eq!(&prompt.text[..prompt.cursor], "中文a");
    }
    #[test]
    fn resource_completion_replaces_token_without_space_or_multiline_loss() {
        let mut prompt = PromptState::default();
        prompt.insert_str_raw("前 local:/\n保留内容");
        let start = "前 ".len();
        let end = "前 local:/".len();
        prompt.cursor = end;
        prompt.replace_range(start..end, "local://ses_1/plans/", false);
        assert_eq!(prompt.text, "前 local://ses_1/plans/\n保留内容");
        assert_eq!(prompt.cursor, start + "local://ses_1/plans/".len());
    }
}
