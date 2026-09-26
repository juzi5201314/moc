//! `<skills>` 目录渲染：元数据常驻 + 预算降级。
//!
//! 业界基线是「每 skill 约 50-100 token 的元数据常驻 + 正文按需加载」。
//! 本模块实现三级降级，保证**所有 skill 名字永远可见**（模型因此会主动
//! 使用 skill 工具），同时把目录总量限制在预算内：
//! 1. 预算内：完整 description（单条截断到 `description_max_chars`）；
//! 2. 超预算：降级为 name + 80 字符短语；
//! 3. 仍超预算：仅列 name。

use super::SkillIndex;
use crate::config::SkillsConfig;

/// 降级短语的最大字符数。
const DEGRADED_CHARS: usize = 80;

/// 渲染 `<skills>` 段；无可用 skill 时返回 `None`。
pub fn render(index: &SkillIndex, config: &SkillsConfig) -> Option<String> {
    if index.is_empty() {
        return None;
    }
    let mut lines = vec![
        "以下 skill 可用。执行与某个 skill 描述相关的任务前，先调用 skill 工具加载其完整说明并严格遵循，不要凭名称臆测其内容。".to_owned(),
    ];
    let mut budget = config.catalog_char_budget.saturating_sub(lines[0].len());
    let mut degraded = 0usize;
    for skill in index.list().filter(|skill| skill.model_invocable) {
        let line = full_line(skill, config);
        if line.len() <= budget {
            budget -= line.len();
            lines.push(line);
            continue;
        }
        degraded += 1;
        let short = short_line(skill);
        if short.len() <= budget {
            budget -= short.len();
            lines.push(short);
            continue;
        }
        let bare = format!("- {}", skill.name);
        budget = budget.saturating_sub(bare.len());
        lines.push(bare);
    }
    if degraded > 0 {
        lines.push(format!(
            "（{degraded} 个 skill 的描述已精简；调用 skill 工具加载后可见完整说明）"
        ));
    }
    Some(format!("<skills>\n{}\n</skills>", lines.join("\n")))
}

fn full_line(skill: &super::SkillMeta, config: &SkillsConfig) -> String {
    let mut line = format!(
        "- {}: {}",
        skill.name,
        truncate_chars(&skill.description, config.description_max_chars)
    );
    if let Some(when_to_use) = &skill.when_to_use {
        line.push_str("；何时使用: ");
        line.push_str(when_to_use);
    }
    if !skill.globs.is_empty() {
        line.push_str(&format!("（适用: {}）", skill.globs.join(", ")));
    }
    line
}

fn short_line(skill: &super::SkillMeta) -> String {
    format!(
        "- {}: {}",
        skill.name,
        truncate_chars(&skill.description, DEGRADED_CHARS)
    )
}

/// 按字符边界截断；超长时附省略号。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillMeta;
    use std::path::PathBuf;

    fn meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.to_owned(),
            description: description.to_owned(),
            when_to_use: None,
            globs: Vec::new(),
            model_invocable: true,
            base_dir: PathBuf::from("/tmp"),
            source: "test".into(),
        }
    }

    fn index_of(metas: Vec<SkillMeta>) -> SkillIndex {
        let mut index = SkillIndex::empty();
        for meta in metas {
            index.insert(meta);
        }
        index
    }

    fn config(budget: usize) -> SkillsConfig {
        SkillsConfig {
            catalog_char_budget: budget,
            ..SkillsConfig::default()
        }
    }

    #[test]
    fn empty_index_renders_nothing() {
        assert!(render(&SkillIndex::empty(), &config(1024)).is_none());
    }

    #[test]
    fn all_names_stay_visible_under_budget_degradation() {
        let long_description = "很长的描述".repeat(200);
        let metas = vec![
            meta("short-one", "短描述"),
            meta("long-one", &long_description),
            meta("hidden-name", &long_description),
        ];
        let index = index_of(metas);
        let rendered = render(&index, &config(400)).unwrap();
        assert!(rendered.contains("<skills>"));
        // 名字全部可见。
        assert!(rendered.contains("- short-one"));
        assert!(rendered.contains("- long-one"));
        assert!(rendered.contains("- hidden-name"));
        // 预算内者有完整描述，超预算者被降级并注明。
        assert!(rendered.contains("短描述"));
        assert!(rendered.contains("描述已精简"));
    }

    #[test]
    fn description_truncated_to_configured_limit() {
        let index = index_of(vec![meta("toolong", "字".repeat(2000).as_str())]);
        let rendered = render(&index, &config(8000)).unwrap();
        assert!(rendered.contains('…'));
        assert!(rendered.chars().count() < 2000);
    }

    #[test]
    fn globs_and_when_to_use_are_annotated() {
        let mut skill = meta("annotated", "描述");
        skill.globs = vec!["*.rs".into()];
        skill.when_to_use = Some("写 Rust 时".into());
        let rendered = render(&index_of(vec![skill]), &config(8000)).unwrap();
        assert!(rendered.contains("（适用: *.rs）"));
        assert!(rendered.contains("何时使用: 写 Rust 时"));
    }

    #[test]
    fn model_hidden_skills_are_excluded() {
        let mut skill = meta("manual-only", "仅手动调用");
        skill.model_invocable = false;
        let rendered = render(&index_of(vec![skill]), &config(8000)).unwrap();
        assert!(!rendered.contains("manual-only"));
    }

    #[test]
    fn tiny_budget_still_lists_bare_names() {
        let index = index_of(vec![
            meta("a-very-long-skill-name", "描述"),
            meta("b", "描述"),
        ]);
        let rendered = render(&index, &config(10)).unwrap();
        assert!(rendered.contains("- a-very-long-skill-name") || rendered.contains("a-very-long"));
        assert!(rendered.contains("- b"));
    }
}
