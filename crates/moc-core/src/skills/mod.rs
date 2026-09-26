//! Agent Skills 支持：SKILL.md 发现、元数据解析、渐进式披露目录与 `skill` 工具。
//!
//! 规范遵循 agentskills.io（frontmatter 必填 `name`/`description`，三级渐进披露），
//! 目录约定采用社区通行的 `.agents/skills/`（项目级 + `~/.agents/skills/` 用户级），
//! 另加 moc 自有 `.moc/skills/` 与配置目录。上下文治理：目录常驻只含
//! name + description（预算超限降级为短语），正文经 `skill` 工具按需加载。

pub mod catalog;
pub mod discovery;
pub mod frontmatter;
pub mod tool;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

/// 单个 skill 的元数据；正文不驻留内存，加载时读盘。
#[derive(Debug, Clone)]
pub struct SkillMeta {
    /// 规范名（小写连字符）；目录清单与 `skill` 工具的入参。
    pub name: String,
    pub description: String,
    /// Claude 风格的触发提示；拼入目录行。
    pub when_to_use: Option<String>,
    /// 文件相关性提示（Cursor 风格）；仅在目录行尾标注，不做过滤。
    pub globs: Vec<String>,
    /// `disable-model-invocation: true` 时不进目录与工具，`/skill:` 仍可用。
    pub model_invocable: bool,
    /// skill 目录绝对路径（真实路径，已解 symlink）。
    pub base_dir: PathBuf,
    /// 发现来源描述，用于诊断。
    pub source: String,
}

impl SkillMeta {
    /// SKILL.md 的绝对路径。
    pub fn skill_file(&self) -> PathBuf {
        self.base_dir.join("SKILL.md")
    }
}

/// 按渲染顺序排列的 skill 索引；会话内不变，保证 system prompt 缓存稳定。
#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    skills: BTreeMap<String, SkillMeta>,
    order: Vec<String>,
}

impl SkillIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.get(name)
    }

    /// 按发现顺序（rank 升序、路径稳定序）遍历。
    pub fn list(&self) -> impl Iterator<Item = &SkillMeta> {
        self.order.iter().filter_map(|name| self.skills.get(name))
    }

    fn insert(&mut self, meta: SkillMeta) {
        if !self.order.contains(&meta.name) {
            self.order.push(meta.name.clone());
        }
        self.skills.insert(meta.name.clone(), meta);
    }
}

/// 已加载 skill 的会话内记账；`skill` 工具据此避免重复注入正文。
pub type LoadedSkills = HashSet<String>;
