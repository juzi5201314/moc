use super::App;
use super::COMPACTION_RUNNING_PREFIX;
use super::CompactionProgress;
use super::compaction_running_note;
use crate::transcript::Block;
use crate::transcript::Entry;

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
        self.push_note(format!("技能摘要处理中：{operation}"), false);
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
        self.replace_skills_note(format!("技能摘要处理中：{stage}\n已生成并校验 {} / {} 个 · 缓存跳过 {} 个\n进度按批次更新；全部成功后才发布缓存。", progress.completed, progress.total, progress.skipped), false);
    }

    pub fn finish_skills_compaction(
        &mut self,
        result: anyhow::Result<moc_protocol::SkillsCompactResult>,
    ) {
        self.skills_compacting = false;
        self.prompt_running = false;
        self.prompt_started_at = None;
        self.cancel_requested = false;
        let (text, error) = match result {
            Ok(result) => {
                let mut text = result.message;
                for warning in result.warnings {
                    text.push('\n');
                    text.push_str(&warning);
                }
                (text, false)
            }
            Err(error) => (
                format!("技能摘要操作未完成：{error:#}\n本次操作没有发布缓存。"),
                true,
            ),
        };
        self.replace_skills_note(text, error);
    }

    /// 与上下文压缩提示相同，按进行中前缀原位替换，不影响更早的命令结果。
    pub fn replace_skills_note(&mut self, text: String, error: bool) {
        if let Some(entry) = self.blocks.iter_mut().rev().find(|entry| {
            matches!(&entry.block, Block::Note { text, .. } if text.starts_with("技能摘要处理中："))
        }) {
            entry.block = Block::Note { text, error };
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
        self.push_note(compaction_running_note(0), false);
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
        self.replace_compaction_note(compaction_running_note(secs), false);
        true
    }

    /// 压缩收尾：把进行中提示原位替换为终态；没有进行中提示时追加一条。
    pub fn finish_compaction(&mut self, text: String, error: bool) {
        self.compaction_progress = None;
        self.replace_compaction_note(text, error);
    }

    /// 用终态文案替换最近一条进行中提示；找不到时追加，不留下两条。
    pub(super) fn replace_compaction_note(&mut self, text: String, error: bool) {
        let existing = self.blocks.iter_mut().rev().find(|entry| {
            matches!(&entry.block, Block::Note { text, .. } if text.starts_with(COMPACTION_RUNNING_PREFIX))
        });
        match existing {
            Some(entry) => {
                entry.block = Block::Note { text, error };
                entry.touch();
            }
            None => self.blocks.push(Entry::new(Block::Note { text, error })),
        }
    }
}
