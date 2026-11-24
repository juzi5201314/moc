use super::PlanAnnotationDraft;
use super::PlanAnnotationEditor;
use super::PlanReviewState;
use super::truncate_chars;
use moc_i18n::t;

impl PlanReviewState {
    pub fn new(
        plan: moc_protocol::methods::PlanInfo,
        execution_engine: moc_protocol::SessionEngine,
    ) -> Self {
        // 有上一版时优先让用户看「改了什么」，没有才看正文。
        let tab = if plan.previous_plan_id.is_some() {
            super::PlanTab::Diff
        } else {
            super::PlanTab::Plan
        };
        Self {
            plan_id: plan.plan_id,
            title: plan.title,
            path: plan.path,
            markdown: plan.markdown,
            status: plan.status,
            version: plan.version,
            previous_plan_id: plan.previous_plan_id,
            submitted: plan.annotations,
            drafts: Vec::new(),
            next_annotation_id: 1,
            tab,
            cursor: 0,
            anchor: None,
            editor: None,
            focus: super::PlanControl::Run,
            engine_menu: None,
            hover_block: None,
            dragging: false,
            drag_start: None,
            scroll_grab: None,
            scroll_offset: 0,
            execution_engine,
            execution_model: None,
            submitting: false,
            error: None,
            diff: None,
            diff_loading: false,
            diff_error: None,
        }
    }

    /// 动作区焦点环（存在的项按显示顺序：芯片在前，按钮在后）。
    pub fn focus_ring(&self) -> Vec<super::PlanControl> {
        use super::PlanControl;
        let mut ring = vec![
            PlanControl::Model,
            PlanControl::NewModel,
            PlanControl::Engine,
            PlanControl::Resume,
        ];
        if !self.drafts.is_empty() {
            ring.push(PlanControl::Submit);
        }
        ring.extend([
            PlanControl::RunNew,
            PlanControl::RunCompacted,
            PlanControl::Run,
        ]);
        ring
    }

    /// 焦点环前移/后移一格（循环）。
    pub fn focus_move(&mut self, delta: isize) {
        let ring = self.focus_ring();
        if ring.is_empty() {
            return;
        }
        let index = ring
            .iter()
            .position(|item| *item == self.focus)
            .unwrap_or(0);
        let len = isize::try_from(ring.len()).unwrap_or(isize::MAX);
        let next = (isize::try_from(index).unwrap_or(0) + delta).rem_euclid(len);
        self.focus = ring[usize::try_from(next).unwrap_or(0)];
    }

    /// 把选区置为 1-based 闭区间对应的源码行（块点击/拖选用）。
    pub fn select_lines(&mut self, start: usize, end: usize) {
        self.anchor = Some(start.saturating_sub(1));
        self.cursor = end.saturating_sub(1);
    }

    /// 是否有可见的选区（计划标签块选择或源码标签选行）。
    pub fn has_selection(&self) -> bool {
        self.anchor.is_some()
    }

    /// 计划源码行（0-based）。
    pub fn source_lines(&self) -> Vec<&str> {
        self.markdown.lines().collect()
    }

    /// 当前选区（0-based，含端点）；没有选区时是光标所在单行。
    pub fn selection(&self) -> (usize, usize) {
        let cursor = self.cursor;
        let anchor = self.anchor.unwrap_or(cursor);
        (cursor.min(anchor), cursor.max(anchor))
    }

    /// 当前版本是否还能批注（只有待审阅的计划可以）。
    pub fn can_annotate(&self) -> bool {
        self.status == moc_protocol::methods::PlanStatus::Submitted
    }

    /// 与选区重叠的第一条草稿。
    pub fn draft_at_cursor(&self) -> Option<usize> {
        let (start, end) = self.selection();
        let (start, end) = (start + 1, end + 1);
        self.drafts
            .iter()
            .position(|draft| draft.start_line <= end && draft.end_line >= start)
    }

    /// 打开编辑器：选区落在已有草稿上就编辑它，否则新建一条。
    pub fn open_editor(&mut self) {
        if !self.can_annotate() {
            self.error = Some(t!("tui.plan.not_annotatable").into_owned());
            return;
        }
        let lines = self.source_lines();
        if lines.is_empty() {
            return;
        }
        let (start, end) = self.selection();
        let editing = self.draft_at_cursor();
        let (start_line, end_line, quote, body) = match editing {
            Some(index) => {
                let draft = &self.drafts[index];
                (
                    draft.start_line,
                    draft.end_line,
                    draft.quote.clone(),
                    draft.body.clone(),
                )
            }
            None => (
                start + 1,
                end + 1,
                lines[start..=end].join("\n"),
                String::new(),
            ),
        };
        self.error = None;
        self.editor = Some(PlanAnnotationEditor {
            editing,
            start_line,
            end_line,
            quote,
            body: crate::widgets::prompt::PromptState::from_text(body),
        });
    }

    /// 保存编辑器里的批注；正文为空时不落草稿。
    pub fn save_editor(&mut self) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let body = editor.body.text.trim_end().to_owned();
        if body.trim().is_empty() {
            self.error = Some(t!("tui.plan.annotation_empty").into_owned());
            return;
        }
        let draft = PlanAnnotationDraft {
            id: format!("a{}", self.next_annotation_id),
            start_line: editor.start_line,
            end_line: editor.end_line,
            quote: truncate_chars(&editor.quote, 4096),
            body,
        };
        let editing = editor.editing;
        self.next_annotation_id += 1;
        self.editor = None;
        self.error = None;
        match editing {
            Some(index) if index < self.drafts.len() => self.drafts[index] = draft,
            _ => self.drafts.push(draft),
        }
    }

    /// 删除光标/选区上的草稿。
    pub fn remove_draft_at_cursor(&mut self) {
        if let Some(index) = self.draft_at_cursor() {
            self.drafts.remove(index);
        }
    }

    /// 移动光标；`select` 为真时同步推进选区端点。
    pub fn move_cursor(&mut self, delta: isize, select: bool) {
        let lines = self.source_lines().len();
        if lines == 0 {
            return;
        }
        let next = self
            .cursor
            .saturating_add_signed(delta)
            .min(lines.saturating_sub(1));
        self.cursor = next;
        if !select {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(next);
        }
    }

    /// 让光标落在给定视口高度内。
    pub fn ensure_cursor_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset.saturating_add(height) {
            self.scroll_offset = self.cursor + 1 - height;
        }
    }

    /// 提交用的批注列表。
    pub fn annotations(&self) -> Vec<moc_protocol::methods::PlanAnnotation> {
        self.drafts
            .iter()
            .map(|draft| moc_protocol::methods::PlanAnnotation {
                id: draft.id.clone(),
                start_line: u32::try_from(draft.start_line).unwrap_or(u32::MAX),
                end_line: u32::try_from(draft.end_line).unwrap_or(u32::MAX),
                quote: draft.quote.clone(),
                body: draft.body.clone(),
            })
            .collect()
    }
}
