use super::ContextContentState;
use super::{ContextLayout, ContextScrollbar};
use moc_protocol::methods::ContextContentResult;
use moc_protocol::methods::ContextContentSection;

impl ContextContentState {
    /// 装载同一份用量与目录快照。
    pub fn set_content(&mut self, content: ContextContentResult) {
        self.current = None;
        self.navigation.clear();
        self.error = content.error.clone();
        self.content = Some(content);
        self.pages.clear();
        self.pending.clear();
        self.node_errors.clear();
        self.loads.clear();
        self.cursors.clear();
        self.previous.clear();
        self.focus = None;
        self.scroll = 0;
        self.layout = None;
    }

    /// 装上失败原因。
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.content = None;
        self.layout = None;
    }

    /// 分块列表；还没读到结果时为空。
    pub fn sections(&self) -> &[ContextContentSection] {
        if let Some(node) = &self.current {
            self.pages
                .get(&node.id)
                .map_or(&[], |page| page.children.as_slice())
        } else {
            self.content
                .as_ref()
                .map_or(&[], |content| content.sections.as_slice())
        }
    }

    pub fn is_body(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|node| node.kind != "category")
    }

    /// 当前分块数。
    pub fn count(&self) -> usize {
        self.sections().len()
    }

    /// 隐藏项与空分类不可进入；返回时恢复父页面的滚动、焦点与阅读模式。
    pub fn enter(&mut self, index: usize) {
        let Some(node) = self.sections().get(index).cloned() else {
            return;
        };
        if node.hidden.is_some() || (node.kind == "category" && node.children == 0) {
            return;
        }
        self.navigation
            .push((self.current.take(), self.scroll, Some(index), self.raw));
        self.scroll = 0;
        self.focus = None;
        self.raw = node.kind == "json";
        if !self.pages.contains_key(&node.id) {
            self.queue(node.id.clone(), None);
        }
        self.current = Some(node);
        self.layout = None;
    }

    pub fn back(&mut self) -> bool {
        let Some((node, scroll, focus, raw)) = self.navigation.pop() else {
            return false;
        };
        self.current = node;
        self.scroll = scroll;
        self.focus = focus;
        self.raw = raw;
        self.layout = None;
        true
    }

    pub(super) fn queue(&mut self, id: String, cursor: Option<u64>) {
        if self.pending.insert(id.clone()) {
            self.node_errors.remove(&id);
            self.loads.push((id, cursor));
        }
    }

    pub fn next_page(&mut self) {
        if let Some(node) = &self.current {
            let id = node.id.clone();
            let cursor = self.pages.get(&id).and_then(|page| page.next_cursor);
            if self.pending.contains(&id) {
                return;
            }
            if self.node_errors.contains_key(&id) {
                self.queue(id.clone(), self.cursors.get(&id).copied());
            } else if let Some(cursor) = cursor {
                let current = self.cursors.get(&id).copied().unwrap_or(0);
                self.previous.entry(id.clone()).or_default().push(current);
                self.cursors.insert(id.clone(), cursor);
                self.queue(id, Some(cursor));
                self.scroll = 0;
            }
        }
    }

    pub fn previous_page(&mut self) {
        let Some(node) = &self.current else { return };
        let id = node.id.clone();
        if self.pending.contains(&id) {
            return;
        }
        if let Some(cursor) = self.previous.get_mut(&id).and_then(Vec::pop) {
            self.cursors.insert(id.clone(), cursor);
            self.scroll = 0;
            self.queue(id, Some(cursor));
        }
    }

    pub fn apply_page(&mut self, result: moc_protocol::methods::ContextReadResult) {
        self.pending.remove(&result.node_id);
        self.pages.insert(result.node_id.clone(), result);
        self.layout = None;
    }

    /// 滚动 `delta` 行（正数向下）；按上一帧的缓存夹住范围。
    pub fn scroll_by(&mut self, delta: i64) {
        let Some(layout) = &self.layout else {
            return;
        };
        let max = layout.max_offset(self.viewport);
        self.scroll = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.scroll.saturating_add(delta as usize)
        }
        .min(max);
    }

    /// 跳到顶部或底部。
    pub fn scroll_to_edge(&mut self, bottom: bool) {
        let Some(layout) = &self.layout else {
            return;
        };
        self.scroll = if bottom {
            layout.max_offset(self.viewport)
        } else {
            0
        };
    }

    /// 焦点移到相邻块，并把该块滚进视口；未选中时从首块开始。
    pub fn focus_step(&mut self, delta: isize) {
        let count = self.count();
        if count == 0 {
            return;
        }
        let current = match self.focus {
            None => 0,
            Some(index) if delta < 0 => index.saturating_sub(delta.unsigned_abs()),
            Some(index) => index.saturating_add(delta as usize).min(count - 1),
        };
        self.focus = Some(current);
        self.reveal_focused();
    }

    /// 把焦点块滚进视口（块比视口高时对齐「更近」的那一端）。
    pub fn reveal_focused(&mut self) {
        let Some(index) = self.focus else {
            return;
        };
        let Some(layout) = &self.layout else {
            return;
        };
        let Some(span) = layout.spans.get(index) else {
            return;
        };
        let viewport = self.viewport;
        let max = layout.max_offset(viewport);
        if span.start < self.scroll {
            self.scroll = span.start.min(max);
        } else if span.end > self.scroll + viewport {
            self.scroll = span.end.saturating_sub(viewport).min(max);
        }
    }

    /// 命中屏幕行所在的分块；正文之外返回 `None`。
    pub fn block_at(&self, row: u16) -> Option<usize> {
        let track = self.scrollbar.track;
        if row < track.y || row >= track.y.saturating_add(track.height) {
            return None;
        }
        let line = self.scroll + usize::from(row - track.y);
        let layout = self.layout.as_ref()?;
        layout
            .spans
            .iter()
            .position(|span| line >= span.start && line < span.end)
    }
}

impl ContextLayout {
    /// 行数不足一屏时的最大滚动偏移（0 表示内容不溢出）。
    pub fn max_offset(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport)
    }
}

impl ContextScrollbar {
    /// 滑块顶端相对轨道顶端的行数；内容不溢出时贴轨道顶端。
    pub fn thumb_top(&self, scroll: usize) -> u16 {
        let travel = self.track.height.saturating_sub(self.thumb);
        if self.max_offset == 0 || travel == 0 {
            return 0;
        }
        let position = scroll.min(self.max_offset) * usize::from(travel) / self.max_offset;
        position as u16
    }

    /// 光标行对应的滚动偏移；`grab` 是拖拽起点到滑块顶端的行差。
    ///
    /// 轨道不足一屏（`max_offset == 0`）时永远回到顶端。
    pub fn offset_at(&self, row: u16, grab: i32) -> usize {
        let travel = i32::from(self.track.height.saturating_sub(self.thumb));
        if travel <= 0 || self.max_offset == 0 {
            return 0;
        }
        let target =
            (i32::from(row).saturating_sub(i32::from(self.track.y)) - grab).clamp(0, travel);
        (target as usize * self.max_offset) / travel as usize
    }
}
