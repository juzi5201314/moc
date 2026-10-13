//! `skill` 工具：按需加载单个 skill 的完整说明（渐进式披露第二层）。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::LoadedSkills;
use crate::tools::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    name: String,
}

pub struct SkillTool;

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Load a skill's full instructions. Choose it from the `<skills>` catalog; the result contains SKILL.md and its resource list. Follow the loaded instructions exactly."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "skill",
            schema(
                serde_json::json!({
                    "name": {"type": "string", "description": "Skill name from the <skills> catalog"}
                }),
                &["name"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: SkillArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let skill = model_skill(&ctx.skills, &args.name)?;
            if !ctx.loaded_skills.write().insert(args.name.clone()) {
                return Ok(ToolOutcome::success(format!(
                    "Skill `{}` is already loaded in this session; its content is still in context.",
                    args.name
                )));
            }
            let text = tokio::fs::read_to_string(skill.skill_file())
                .await
                .map_err(|error| ToolError::new("E_ACCESS", error.to_string()))?;
            let parsed = super::frontmatter::parse(&text)
                .map_err(|error| ToolError::new("E_SKILL", error.to_string()))?;
            let mut output = format!(
                "<skill_content name=\"{}\" base=\"skill://{}\">\n{}\n</skill_content>",
                skill.name,
                skill.name,
                parsed.body.trim()
            );
            let resources = list_resources(&skill.base_dir)
                .into_iter()
                .map(|resource| format!("skill://{}/{}", skill.name, resource))
                .collect::<Vec<_>>();
            if !resources.is_empty() {
                output.push_str(&format!(
                    "\n\nResources bundled with this skill (use read to inspect):\n{}",
                    resources.join("\n")
                ));
            }
            Ok(ToolOutcome::success(output))
        })
    }
}

/// 模型只能加载 `model_invocable` 的 skill；`disable-model-invocation` 的
/// skill 仍可由用户显式加载，但不能经该工具进入上下文。
fn model_skill<'a>(
    skills: &'a super::SkillIndex,
    name: &str,
) -> Result<&'a super::SkillMeta, ToolError> {
    let Some(skill) = skills.get(name) else {
        let available: Vec<&str> = skills
            .list()
            .filter(|skill| skill.model_invocable)
            .map(|skill| skill.name.as_str())
            .take(20)
            .collect();
        let hint = if available.is_empty() {
            "No model-invocable skills are available.".to_owned()
        } else {
            format!("Available: {}", available.join(", "))
        };
        return Err(ToolError::new(
            "E_NOT_FOUND",
            format!("Unknown skill `{name}`. {hint}"),
        ));
    };
    if !skill.model_invocable {
        return Err(ToolError::new(
            "E_SKILL_NOT_INVOCABLE",
            format!(
                "Skill `{name}` does not allow model invocation; only the user can load it explicitly."
            ),
        ));
    }
    Ok(skill)
}

/// 列出 skill 目录一级条目（SKILL.md 本体除外），按名称排序。
fn list_resources(base_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "SKILL.md" || name.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().ok().map(|t| t.is_dir()).unwrap_or(false);
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();
    names
}

/// 供 `/skill:` 用户调用复用的正文渲染（不走工具路径，不做去重）。
pub fn render_for_invocation(
    skills: &super::SkillIndex,
    name: &str,
    loaded: &mut LoadedSkills,
) -> Result<String, String> {
    let Some(skill) = skills.get(name) else {
        return Err(format!("Unknown skill `{name}`"));
    };
    let text = std::fs::read_to_string(skill.skill_file())
        .map_err(|error| format!("Failed to read skill: {error}"))?;
    let parsed = super::frontmatter::parse(&text).map_err(|error| error.to_string())?;
    loaded.insert(name.to_owned());
    let resources = list_resources(&skill.base_dir)
        .into_iter()
        .map(|resource| format!("skill://{}/{}", skill.name, resource))
        .collect::<Vec<_>>();
    let resources = if resources.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nResources bundled with this skill (use read to inspect):\n{}",
            resources.join("\n")
        )
    };
    Ok(format!(
        "<skill_content name=\"{}\" base=\"skill://{}\">\n{}\n</skill_content>{resources}",
        skill.name,
        skill.name,
        parsed.body.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillIndex, SkillMeta};
    use std::collections::HashSet;

    fn meta(name: &str, base_dir: std::path::PathBuf, model_invocable: bool) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: "演示".into(),
            when_to_use: None,
            globs: Vec::new(),
            model_invocable,
            base_dir,
            source: "test".into(),
        }
    }

    #[test]
    fn model_invocation_requires_an_invocable_skill() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = SkillIndex::empty();
        for (name, invocable) in [("open", true), ("locked", false)] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            index.insert(meta(name, dir.path().join(name), invocable));
        }
        assert!(model_skill(&index, "open").is_ok());
        assert_eq!(
            model_skill(&index, "locked").unwrap_err().code,
            "E_SKILL_NOT_INVOCABLE"
        );
        let unknown = model_skill(&index, "missing").unwrap_err();
        assert_eq!(unknown.code, "E_NOT_FOUND");
        let listing = unknown.to_string();
        assert!(listing.contains("open") && !listing.contains("locked"));
    }

    #[test]
    fn resources_listing_skips_skill_md_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "x").unwrap();
        std::fs::write(dir.path().join("helper.py"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("references")).unwrap();
        std::fs::write(dir.path().join(".hidden"), "x").unwrap();
        let listed = list_resources(dir.path());
        assert_eq!(
            listed,
            vec!["helper.py".to_owned(), "references/".to_owned()]
        );
    }

    #[test]
    fn invocation_render_strips_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("demo")).unwrap();
        std::fs::write(
            dir.path().join("demo/SKILL.md"),
            "---\nname: demo\ndescription: 演示\n---\n# 步骤\n照做。",
        )
        .unwrap();
        let mut index = SkillIndex::empty();
        index.insert(SkillMeta {
            name: "demo".into(),
            description: "演示".into(),
            when_to_use: None,
            globs: Vec::new(),
            model_invocable: true,
            base_dir: dir.path().join("demo"),
            source: "test".into(),
        });
        let mut loaded: LoadedSkills = HashSet::new();
        let rendered = render_for_invocation(&index, "demo", &mut loaded).unwrap();
        assert!(rendered.starts_with("<skill_content name=\"demo\""));
        assert!(rendered.contains("# 步骤"));
        assert!(!rendered.contains("description: 演示"));
        assert!(loaded.contains("demo"));
        assert!(render_for_invocation(&index, "missing", &mut loaded).is_err());
    }
}
