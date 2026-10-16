//! `<skills>` 目录渲染：元数据常驻 + 预算降级。
//!
//! 业界基线是「每 skill 约 50-100 token 的元数据常驻 + 正文按需加载」。
//! 本模块实现三级降级，保证**所有 skill 名字永远可见**（模型因此会主动
//! 使用 skill 工具），同时把目录总量限制在
//! [`SkillsConfig::catalog_budget_tokens`] 给出的预算内：
//! 1. 预算内：完整 description（单条截断到 `description_max_chars`）；
//! 2. 超预算：降级为 name + 80 字符短语；
//! 3. 仍超预算：仅列 name。

use super::SkillIndex;
use crate::compact::estimate_tokens;
use crate::config::SkillsConfig;

/// 降级短语的最大字符数。
const DEGRADED_CHARS: usize = 80;

/// 渲染 `<skills>` 段；无可用 skill 时返回 `None`。
///
/// `context_window` 用于换算目录预算：占上下文窗口的百分比与配置下限取较大者。
pub fn render(index: &SkillIndex, config: &SkillsConfig, context_window: u64) -> Option<String> {
    if index.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Available skills. Before a task a skill covers, load it with the skill tool and follow it; never infer behavior from the name.".to_owned(),
    ];
    let mut budget = config
        .catalog_budget_tokens(context_window)
        .saturating_sub(estimate_tokens(&lines[0]));
    let mut degraded = 0usize;
    for skill in index.list().filter(|skill| skill.model_invocable) {
        let line = full_line(skill, config);
        let cost = estimate_tokens(&line);
        if cost <= budget {
            budget -= cost;
            lines.push(line);
            continue;
        }
        degraded += 1;
        let short = short_line(skill);
        let cost = estimate_tokens(&short);
        if cost <= budget {
            budget -= cost;
            lines.push(short);
            continue;
        }
        let bare = format!("- {}", skill.name);
        budget = budget.saturating_sub(estimate_tokens(&bare));
        lines.push(bare);
    }
    if degraded > 0 {
        lines.push(format!(
            "({degraded} skill descriptions were shortened; load them with the skill tool for full details)"
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
        line.push_str("; when to use: ");
        line.push_str(when_to_use);
    }
    if !skill.globs.is_empty() {
        line.push_str(&format!(" (applies to: {})", skill.globs.join(", ")));
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

    /// 关闭比例项、以 token 下限固定预算，便于断言。
    fn config(budget_tokens: u64) -> SkillsConfig {
        SkillsConfig {
            catalog_budget_percent: 0,
            catalog_budget_min_tokens: budget_tokens,
            ..SkillsConfig::default()
        }
    }

    #[test]
    fn empty_index_renders_nothing() {
        assert!(render(&SkillIndex::empty(), &config(1024), 0).is_none());
    }

    #[test]
    fn budget_takes_larger_of_percent_and_floor() {
        let config = SkillsConfig {
            catalog_budget_percent: 1,
            catalog_budget_min_tokens: 8000,
            ..SkillsConfig::default()
        };
        // 500k 窗口的 1% 是 5000，低于下限 → 用下限。
        assert_eq!(config.catalog_budget_tokens(500_000), 8000);
        // 1M 窗口的 1% 是 10000，高于下限 → 用比例值。
        assert_eq!(config.catalog_budget_tokens(1_000_000), 10_000);
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
        let rendered = render(&index, &config(400), 0).unwrap();
        assert!(rendered.contains("<skills>"));
        // 名字全部可见。
        assert!(rendered.contains("- short-one"));
        assert!(rendered.contains("- long-one"));
        assert!(rendered.contains("- hidden-name"));
        // 预算内者有完整描述，超预算者被降级并注明。
        assert!(rendered.contains("短描述"));
        assert!(rendered.contains("skill descriptions were shortened"));
    }

    #[test]
    fn description_truncated_to_configured_limit() {
        let index = index_of(vec![meta("toolong", "字".repeat(2000).as_str())]);
        let rendered = render(&index, &config(8000), 0).unwrap();
        assert!(rendered.contains('…'));
        assert!(rendered.chars().count() < 2000);
    }

    #[test]
    fn globs_and_when_to_use_are_annotated() {
        let mut skill = meta("annotated", "描述");
        skill.globs = vec!["*.rs".into()];
        skill.when_to_use = Some("写 Rust 时".into());
        let rendered = render(&index_of(vec![skill]), &config(8000), 0).unwrap();
        assert!(rendered.contains("(applies to: *.rs)"));
        assert!(rendered.contains("when to use: "));
    }

    #[test]
    fn model_hidden_skills_are_excluded() {
        let mut skill = meta("manual-only", "仅手动调用");
        skill.model_invocable = false;
        let rendered = render(&index_of(vec![skill]), &config(8000), 0).unwrap();
        assert!(!rendered.contains("manual-only"));
    }

    #[test]
    fn tiny_budget_still_lists_bare_names() {
        let index = index_of(vec![
            meta("a-very-long-skill-name", "描述"),
            meta("b", "描述"),
        ]);
        let rendered = render(&index, &config(10), 0).unwrap();
        assert!(rendered.contains("- a-very-long-skill-name") || rendered.contains("a-very-long"));
        assert!(rendered.contains("- b"));
    }
}
