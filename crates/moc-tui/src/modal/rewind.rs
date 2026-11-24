use super::ModelScroll;
use super::RewindItem;
use super::RewindState;
use moc_protocol::Role;
use moc_protocol::methods::TranscriptEntry;
use moc_i18n::t;

impl RewindState {
    pub fn set_entries(&mut self, entries: Vec<TranscriptEntry>, future: Vec<TranscriptEntry>) {
        self.prompts.clear();
        self.items.clear();
        // 已回退条目沿用同一分组规则接在历史尾部：非用户条目挂进既有提问组，
        // 用户条目新开组——时间线上看就是接在被截断的那一轮之后。
        let mut future_start = usize::MAX;
        for (entry, future) in entries
            .into_iter()
            .map(|entry| (entry, false))
            .chain(future.into_iter().map(|entry| (entry, true)))
        {
            // `!!` 命令、代理消息与运行时提醒都不是人类提问，不作为回退边界。
            if entry.bash.is_some() || entry.agent.is_some() || entry.reminder.is_some() {
                continue;
            }
            let p_idx;
            let i_idx;
            if entry.message.role == Role::User {
                p_idx = self.prompts.len();
                i_idx = 0;
                self.prompts.push(vec![entry.clone()]);
            } else if self.prompts.is_empty() {
                continue;
            } else {
                p_idx = self.prompts.len() - 1;
                let prompt = self.prompts.last_mut().expect("非空必有尾组");
                i_idx = prompt.len();
                prompt.push(entry.clone());
            }
            if future {
                future_start = future_start.min(self.items.len());
            }
            let is_user = entry.message.role == Role::User;
            // 用户消息把图片块显示成 `[image#N]` 标记，其余角色只取文本。
            let text = if is_user {
                crate::transcript_restore::content_text(&entry.message.content)
            } else {
                entry.message.text()
            };
            let tool_calls: Vec<String> = entry
                .message
                .tool_calls()
                .map(|call| call.name.clone())
                .collect();

            let preview = if is_user {
                let first_line = text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                if first_line.is_empty() {
                    t!("tui.rewind.user_prompt").into_owned()
                } else {
                    first_line.to_string()
                }
            } else if !tool_calls.is_empty() {
                t!(
                    "tui.rewind.tools_called_fmt",
                    names = tool_calls.join(&t!("tui.values.list_sep"))
                )
                .into_owned()
            } else {
                let first_line = text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                if first_line.is_empty() {
                    if entry.message.role == Role::Tool {
                        t!("tui.rewind.tool_result").into_owned()
                    } else {
                        t!("tui.rewind.model_reply").into_owned()
                    }
                } else {
                    first_line.to_string()
                }
            };

            let full_text = if !text.trim().is_empty() {
                text
            } else if !tool_calls.is_empty() {
                let mut s = format!(
                    "{}\n",
                    t!(
                        "tui.rewind.tools_called_fmt",
                        names = tool_calls.join(&t!("tui.values.list_sep"))
                    )
                );
                for call in entry.message.tool_calls() {
                    s.push_str(&t!(
                        "tui.rewind.tool_args_fmt",
                        name = call.name.as_str(),
                        args = call.arguments.to_string()
                    ));
                }
                s
            } else {
                entry
                    .message
                    .tool_results()
                    .map(|res| res.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            self.items.push(RewindItem {
                entry_id: entry.id.clone(),
                role: entry.message.role,
                is_user,
                future,
                at_ms: entry.at_ms,
                preview,
                full_text,
                tool_calls,
                prompt_index: p_idx,
                item_index: i_idx,
                entry,
            });
        }
        self.future_start = if future_start == usize::MAX {
            self.items.len()
        } else {
            future_start
        };

        // 默认光标停在最后一条历史条目上：已回退的尾部是可选的前滚目标，
        // 不该成为打开模态时的默认确认对象。
        self.selected = self.future_start.min(self.items.len()).saturating_sub(1);
        self.sync_prompt_item_from_selected();
        self.loading = false;
        self.error = None;
        self.filtering = false;
        self.query.clear();
        self.list_scroll = 0;
        self.scroll = ModelScroll::default();
        self.pointer = None;
        self.detail_scroll = 0;
    }

    pub fn sync_selected_from_prompt_item(&mut self) {
        if let Some(pos) = self.items.iter().position(|it| {
            it.prompt_index == self.selected_prompt && it.item_index == self.selected_item
        }) {
            self.selected = pos;
        } else {
            self.selected = self.items.len().saturating_sub(1);
        }
        self.scroll.reveal_selection = true;
        self.detail_scroll = 0;
    }

    pub fn sync_prompt_item_from_selected(&mut self) {
        if let Some(item) = self.items.get(self.selected) {
            self.selected_prompt = item.prompt_index;
            self.selected_item = item.item_index;
        }
        self.scroll.reveal_selection = true;
        self.detail_scroll = 0;
    }

    pub fn selected(&self) -> Option<&TranscriptEntry> {
        if let Some(item) = self.items.get(self.selected) {
            Some(&item.entry)
        } else {
            self.prompts
                .get(self.selected_prompt)
                .and_then(|prompt| prompt.get(self.selected_item))
        }
    }

    pub fn selected_item_ref(&self) -> Option<&RewindItem> {
        self.items.get(self.selected)
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        let words: Vec<&str> = q.split_whitespace().collect();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                let target_str = format!("{} {}", it.preview, it.full_text).to_lowercase();
                words.iter().all(|w| target_str.contains(w))
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn step(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let matches = self.filtered_indices();
        if matches.is_empty() {
            return;
        }
        let curr_pos = matches.iter().position(|&idx| idx == self.selected);
        let new_pos = match curr_pos {
            Some(p) => (p as i32 + delta).clamp(0, matches.len() as i32 - 1) as usize,
            None => {
                if delta < 0 {
                    matches
                        .iter()
                        .rposition(|&idx| idx < self.selected)
                        .unwrap_or(0)
                } else {
                    matches
                        .iter()
                        .position(|&idx| idx > self.selected)
                        .unwrap_or(matches.len() - 1)
                }
            }
        };
        self.selected = matches[new_pos];
        self.sync_prompt_item_from_selected();
    }

    pub fn jump_user(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let matches = self.filtered_indices();
        if matches.is_empty() {
            return;
        }
        if delta < 0 {
            if let Some(&prev_idx) = matches
                .iter()
                .rfind(|&&idx| idx < self.selected && self.items[idx].is_user)
            {
                self.selected = prev_idx;
                self.sync_prompt_item_from_selected();
            }
        } else if delta > 0
            && let Some(&next_idx) = matches
                .iter()
                .find(|&&idx| idx > self.selected && self.items[idx].is_user)
        {
            self.selected = next_idx;
            self.sync_prompt_item_from_selected();
        }
    }

    pub fn move_vertical(&mut self, delta: i32) {
        let Some(prompt) = self.prompts.get(self.selected_prompt) else {
            return;
        };
        let max = prompt.len().saturating_sub(1);
        self.selected_item = if delta < 0 {
            self.selected_item
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_item.saturating_add(delta as usize).min(max)
        };
        self.sync_selected_from_prompt_item();
    }

    pub fn move_horizontal(&mut self, delta: i32) {
        if self.prompts.is_empty() {
            return;
        }
        let max = self.prompts.len().saturating_sub(1);
        self.selected_prompt = if delta < 0 {
            self.selected_prompt
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_prompt.saturating_add(delta as usize).min(max)
        };
        self.selected_item = self
            .selected_item
            .min(self.prompts[self.selected_prompt].len().saturating_sub(1));
        self.sync_selected_from_prompt_item();
    }

    pub fn start_filter(&mut self) {
        self.filtering = true;
    }

    pub fn input_char(&mut self, c: char) {
        self.query.push(c);
        let matches = self.filtered_indices();
        if !matches.is_empty() && !matches.contains(&self.selected) {
            self.selected = matches[0];
            self.sync_prompt_item_from_selected();
        }
    }

    pub fn backspace(&mut self) {
        if !self.query.is_empty() {
            self.query.pop();
            let matches = self.filtered_indices();
            if !matches.is_empty() && !matches.contains(&self.selected) {
                self.selected = matches[0];
                self.sync_prompt_item_from_selected();
            }
        } else {
            self.filtering = false;
        }
    }

    pub fn clear_filter(&mut self) {
        self.query.clear();
        self.filtering = false;
    }

    pub fn scroll_detail(&mut self, delta: i32) {
        if delta < 0 {
            self.detail_scroll = self
                .detail_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as usize);
        }
    }
}
