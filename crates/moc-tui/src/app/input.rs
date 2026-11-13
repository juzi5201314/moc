use super::App;
use crate::widgets::completion::CompletionState;

impl App {
    pub fn prompt_changed(&mut self) {
        self.selection = None;
        self.pending_click = None;
        self.selection_copied_at = None;
        self.prompt_revision = self.prompt_revision.wrapping_add(1);
        self.completion = CompletionState::derive(self);
        self.completion_prefix = None;
    }

    /// 插入一个字符。
    pub fn prompt_insert_char(&mut self, ch: char) {
        self.prompt.insert_char(ch);
        self.prompt_changed();
    }

    /// 输入框当前文本快照。
    pub fn prompt_text(&self) -> &str {
        &self.prompt.text
    }

    /// 挂载持久化输入历史，并按当前展示范围填充输入框。
    pub fn load_input_history(&mut self, home: &std::path::Path) {
        self.history = crate::history::InputHistory::load(home);
        self.refresh_input_history();
    }

    /// 把一条已提交文本写进持久化历史；内存历史由输入框在提交时维护。
    pub fn record_input_history(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let project = self.history_project();
        let entry = self.history.record_background(text, &project);
        self.prompt.bind_history(text, entry);
    }

    /// 按当前偏好与项目重新填充输入历史；切换范围或会话时调用。
    pub fn refresh_input_history(&mut self) {
        let entries = self
            .history
            .handles(self.preferences.history_scope, &self.history_project());
        self.prompt.seed_history_handles(entries);
    }
}
