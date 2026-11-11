//! 技能目录使用完整缓存提示；未缓存描述按最大最小公平份额分配预算。
//! 预算不足时保留每项最小发现提示并明确报告，不按排序剥夺后项描述。

use super::SkillIndex;
use crate::compact::{estimate_tokens, prefix_within_tokens};
use crate::config::SkillsConfig;

const INTRO: &str = "Available skills. Before a task a skill covers, load it with the skill tool and follow it; never infer behavior from the name.";
const SHORTENED: &str =
    "Some uncached descriptions were shortened; load the skill for full details.";
const OVER_BUDGET: &str =
    "The skills catalog exceeds its budget to preserve every skill's routing hint.";
const MIN_DESCRIPTION_CHARS: usize = 80;

pub fn render(index: &SkillIndex, config: &SkillsConfig, context_window: u64) -> Option<String> {
    let entries: Vec<_> = index
        .list()
        .filter(|skill| skill.model_invocable)
        .map(|skill| {
            let prefix = format!("- {}: ", skill.name);
            if let Some(hint) = index.hints.get(&skill.name) {
                return (prefix, hint.clone(), hint.clone());
            }
            let mut full = truncate_chars(
                &skill.description,
                config.description_max_chars.max(MIN_DESCRIPTION_CHARS),
            );
            if let Some(when) = &skill.when_to_use {
                full.push_str("; when to use: ");
                full.push_str(when);
            }
            if !skill.globs.is_empty() {
                full.push_str(" (applies to: ");
                full.push_str(&skill.globs.join(", "));
                full.push(')');
            }
            let minimum = truncate_chars(&full, MIN_DESCRIPTION_CHARS);
            (prefix, full, minimum)
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let budget = config.catalog_budget_tokens(context_window);
    let fixed = estimate_tokens(&format!("<skills>\n{INTRO}\n{SHORTENED}\n</skills>"));
    let floor: u64 = entries
        .iter()
        .map(|(prefix, _, minimum)| estimate_tokens(prefix) + estimate_tokens(minimum) + 1)
        .sum();
    let remaining = budget.saturating_sub(fixed + floor);
    let demands: Vec<_> = entries
        .iter()
        .map(|(_, full, minimum)| estimate_tokens(full).saturating_sub(estimate_tokens(minimum)))
        .collect();
    // 按统一水位分配额外份额；短项用不完的额度自然归还，不依赖目录顺序。
    let mut low = 0;
    let mut high = demands.iter().copied().max().unwrap_or(0);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let cost: u64 = demands.iter().map(|need| (*need).min(mid)).sum();
        if cost <= remaining {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut shortened = false;
    let mut output = format!("<skills>\n{INTRO}");
    for ((prefix, full, minimum), demand) in entries.iter().zip(demands) {
        output.push('\n');
        output.push_str(prefix);
        if demand <= low {
            output.push_str(full);
        } else {
            shortened = true;
            let cap = estimate_tokens(minimum) + low;
            let candidate = prefix_within_tokens(full, cap.saturating_sub(1));
            if candidate.chars().count() < minimum.trim_end_matches('…').chars().count() {
                output.push_str(minimum);
            } else {
                output.push_str(candidate.trim_end());
                output.push('…');
            }
        }
    }
    if shortened {
        output.push('\n');
        output.push_str(SHORTENED);
    }
    if estimate_tokens(&output) + estimate_tokens("\n</skills>") > budget {
        output.push('\n');
        output.push_str(OVER_BUDGET);
    }
    output.push_str("\n</skills>");
    Some(output)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(max_chars) else {
        return text.to_owned();
    };
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillMeta;
    use std::{collections::BTreeMap, path::PathBuf};

    fn meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: description.into(),
            when_to_use: None,
            globs: Vec::new(),
            model_invocable: true,
            base_dir: PathBuf::from("/tmp"),
            source: "test".into(),
        }
    }
    fn config(budget: u64) -> SkillsConfig {
        SkillsConfig {
            catalog_budget_percent: 0,
            catalog_budget_min_tokens: budget,
            ..SkillsConfig::default()
        }
    }
    fn descriptions(text: &str) -> BTreeMap<&str, &str> {
        text.lines()
            .filter_map(|line| line.strip_prefix("- ")?.split_once(": "))
            .collect()
    }
    #[test]
    fn budget_is_fair_and_independent_of_order() {
        let long = "Routing context about a specialized task. ".repeat(100);
        let mut index = SkillIndex::empty();
        for name in ["a", "b", "c"] {
            index.insert(meta(name, &long));
        }
        index.insert(meta("short", "Review changes"));
        let first = render(&index, &config(240), 0).unwrap();
        index.order.reverse();
        let second = render(&index, &config(240), 0).unwrap();
        assert_eq!(descriptions(&first), descriptions(&second));
        let entries = descriptions(&first);
        assert_eq!(entries["a"], entries["b"]);
        assert_eq!(entries["b"], entries["c"]);
        assert_eq!(entries["short"], "Review changes");
        assert!(entries["a"].chars().count() >= MIN_DESCRIPTION_CHARS);
        assert!(entries["a"].len() < long.len());
    }
    #[test]
    fn tiny_budget_preserves_cached_exclusions_and_uncached_hints() {
        let mut index = SkillIndex::empty();
        index.insert(meta("review", "Original description"));
        index.insert(meta("other", "Other task instructions"));
        let hint = "Review changes; not implementation work.";
        index.hints.insert("review".into(), hint.into());
        let text = render(&index, &config(1), 0).unwrap();
        assert_eq!(descriptions(&text)["review"], hint);
        assert_eq!(descriptions(&text)["other"], "Other task instructions");
    }
    #[test]
    fn no_model_visible_skills_has_no_catalog() {
        let mut index = SkillIndex::empty();
        assert!(render(&index, &config(100), 0).is_none());
        let mut hidden = meta("manual", "Manual task");
        hidden.model_invocable = false;
        index.insert(hidden);
        assert!(render(&index, &config(100), 0).is_none());
    }
}
