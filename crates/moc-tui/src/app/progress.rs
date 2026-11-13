use super::App;
use super::COMPACTION_RUNNING_PREFIX;
use super::CompactionProgress;
use super::RetryWait;
use super::compaction_running_note;
use crate::transcript::Block;
use crate::transcript::Entry;
use crate::transcript::NoteTone;

impl App {
    pub fn start_skills_compaction(&mut self, action: moc_protocol::SkillsCompactAction) {
        self.skills_compacting = true;
        self.prompt_running = true;
        self.prompt_started_at = Some(std::time::Instant::now());
        self.cancel_requested = false;
        let operation = match action {
            moc_protocol::SkillsCompactAction::Missing
            | moc_protocol::SkillsCompactAction::Force => "检查技能与缓存，准备压缩 skills…",
            moc_protocol::SkillsCompactAction::Cleanup => "清理已不存在的技能缓存…",
            moc_protocol::SkillsCompactAction::Clear => "清空技能摘要缓存…",
        };
        self.push_note(format!("技能摘要处理中：{operation}"), NoteTone::Info);
        self.scroll_offset = 0;
    }

    pub fn update_skills_compaction(&mut self, progress: &moc_protocol::SkillsProgressParams) {
        if !self.skills_compacting || self.cancel_requested {
            return;
        }
        let stage = match progress.phase {
            moc_protocol::SkillsProgressPhase::Generating => "正在压缩 skills",
            moc_protocol::SkillsProgressPhase::Committing => "正在原子提交缓存",
        };
        self.replace_skills_note(format!("技能摘要处理中：{stage}\n已生成并校验 {} / {} 个 · 缓存跳过 {} 个\n进度按批次更新；全部成功后才发布缓存。", progress.completed, progress.total, progress.skipped), NoteTone::Info);
    }

    pub fn finish_skills_compaction(
        &mut self,
        result: anyhow::Result<moc_protocol::SkillsCompactResult>,
    ) {
        self.skills_compacting = false;
        self.prompt_running = false;
        self.prompt_started_at = None;
        self.cancel_requested = false;
        let (text, tone) = match result {
            Ok(result) => {
                let mut text = result.message;
                for warning in result.warnings {
                    text.push('\n');
                    text.push_str(&warning);
                }
                (text, NoteTone::Info)
            }
            Err(error) => (
                format!("技能摘要操作未完成：{error:#}\n本次操作没有发布缓存。"),
                NoteTone::Error,
            ),
        };
        self.replace_skills_note(text, tone);
    }

    /// 与上下文压缩提示相同，按进行中前缀原位替换，不影响更早的命令结果。
    pub fn replace_skills_note(&mut self, text: String, tone: NoteTone) {
        if let Some(entry) = self.blocks.iter_mut().rev().find(|entry| {
            matches!(&entry.block, Block::Note { text, .. } if text.starts_with("技能摘要处理中："))
        }) {
            entry.block = Block::Note { text, tone };
            entry.touch();
        }
    }

    /// 上下文压缩开始：插入进行中提示；换方法重试时沿用同一条计时。
    pub fn start_compaction(&mut self) {
        if self.compaction_progress.is_some() {
            return;
        }
        let Some(session_id) = self.session.as_ref().map(|session| session.id.clone()) else {
            return;
        };
        self.push_note(compaction_running_note(0), NoteTone::Info);
        self.compaction_progress = Some(CompactionProgress {
            session_id,
            started_at: std::time::Instant::now(),
            shown_secs: 0,
        });
    }

    /// 刷新进行中提示的秒数；返回提示是否变化（供事件循环决定重绘）。
    pub fn tick_compaction(&mut self) -> bool {
        let Some(current) = self.session.as_ref().map(|session| session.id.clone()) else {
            self.compaction_progress = None;
            return false;
        };
        let Some(progress) = self.compaction_progress.as_mut() else {
            return false;
        };
        if progress.session_id != current {
            self.compaction_progress = None;
            return false;
        }
        let secs = progress.started_at.elapsed().as_secs();
        if secs == progress.shown_secs {
            return false;
        }
        progress.shown_secs = secs;
        self.replace_compaction_note(compaction_running_note(secs), NoteTone::Info);
        true
    }

    /// 压缩收尾：把进行中提示原位替换为终态；没有进行中提示时追加一条。
    pub fn finish_compaction(&mut self, text: String, tone: NoteTone) {
        self.compaction_progress = None;
        self.replace_compaction_note(text, tone);
    }

    /// 用终态文案替换最近一条进行中提示；找不到时追加，不留下两条。
    pub(super) fn replace_compaction_note(&mut self, text: String, tone: NoteTone) {
        let existing = self.blocks.iter_mut().rev().find(|entry| {
            matches!(&entry.block, Block::Note { text, .. } if text.starts_with(COMPACTION_RUNNING_PREFIX))
        });
        match existing {
            Some(entry) => {
                entry.block = Block::Note { text, tone };
                entry.touch();
            }
            None => self.blocks.push(Entry::new(Block::Note { text, tone })),
        }
    }

    /// 一次模型生成失败后的等待重试：推一条警告便签，并把秒数位置登记成倒数。
    ///
    /// 文案切不出秒数锚点（措辞变了）时退化为静态便签，便签仍是警告色。
    pub fn start_retry_wait(&mut self, message: String, delay_ms: u64) {
        let Some(session_id) = self.session.as_ref().map(|session| session.id.clone()) else {
            self.push_note(message, NoteTone::Warn);
            return;
        };
        let delay_secs = delay_ms as f64 / 1000.0;
        let Some((head, tail)) = retry_note_parts(&message) else {
            self.push_note(message, NoteTone::Warn);
            return;
        };
        let shown = format!("{delay_secs:.1}");
        self.push_note(format!("{head}{shown} {tail}"), NoteTone::Warn);
        self.retry_wait = Some(RetryWait {
            session_id,
            note_index: self.blocks.len() - 1,
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(delay_ms),
            shown,
            head,
            tail,
            message,
        });
    }

    /// 刷新重试倒数；返回文案是否变化（供事件循环决定重绘）。
    ///
    /// 便签被 rewind/切会话拿掉时状态一并丢弃；倒数到 0 后停在原地，
    /// 等 `generation_started` 把它定稿回 core 的原文。
    pub fn tick_retry_wait(&mut self) -> bool {
        let Some(wait) = self.retry_wait.take() else {
            return false;
        };
        let current = self.session.as_ref().map(|session| session.id.as_str());
        if current != Some(wait.session_id.as_str()) {
            return false;
        }
        let remaining = wait
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        let shown = format!("{:.1}", remaining.as_secs_f64());
        if shown == wait.shown {
            self.retry_wait = Some(wait);
            return false;
        }
        let Some(entry) = self.blocks.get_mut(wait.note_index) else {
            return false;
        };
        if !matches!(&entry.block, Block::Note { text, .. } if text.starts_with(&wait.head)) {
            return false;
        }
        entry.block = Block::Note {
            text: format!("{}{shown} {}", wait.head, wait.tail),
            tone: NoteTone::Warn,
        };
        entry.touch();
        self.retry_wait = Some(RetryWait { shown, ..wait });
        true
    }

    /// 重试等待结束：把倒数便签写回 core 下发的原文。
    ///
    /// 便签已经不在（被 rewind 撤掉或整个转写换掉）时只丢状态。
    pub fn finish_retry_wait(&mut self) {
        let Some(wait) = self.retry_wait.take() else {
            return;
        };
        let Some(entry) = self.blocks.get_mut(wait.note_index) else {
            return;
        };
        if matches!(&entry.block, Block::Note { text, .. } if text.starts_with(&wait.head)) {
            entry.block = Block::Note {
                text: wait.message,
                tone: NoteTone::Warn,
            };
            entry.touch();
        }
    }
}

/// 把重试文案切成秒数两侧的静态片段：`{head}{:.1} {tail}`。
///
/// core 的措辞是「本次生成失败，N.N 秒后自动重试（第 A/M 次尝试）。失败预览已丢弃。…」，
/// 秒数是文案里唯一随时间变化的部分；锚点缺失返回 `None`，调用方按静态便签处理。
fn retry_note_parts(message: &str) -> Option<(String, String)> {
    let (left, right) = message.split_once("秒后自动重试")?;
    let (prefix, _) = left.rsplit_once('，')?;
    Some((format!("{prefix}，"), format!("秒后自动重试{right}")))
}
