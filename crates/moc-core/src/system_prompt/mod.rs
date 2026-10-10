//! Stable system prefix and volatile tail reminders.

pub mod context_files;
pub mod reminder;

use std::path::Path;

use moc_llm::Message;
use moc_protocol::methods::SessionMode;

use crate::config::ResolvedConfig;
use crate::config::model::ResolvedModel;

const DEFAULT_INTRO: &str = "You are a precise software-engineering agent working in the user's local workspace through tools.";
const TOOL_POLICY: &str = "Use read for files and directories, grep for content, find for paths, edit for anchored local changes, write only for new or complete-file writes, and bash only when a shell is required. Do not use bash, cat, grep, rg, find, ls, sed, awk, perl, Python, redirection, or similar commands to read, search, or edit workspace files. Before editing, read the current file; edit uses the three-character anchors in read output, formatted as `ABC│line`. If anchors may be stale, read again.\n\nModes are enforced when tools execute: normal permits workspace changes; ask is read-only; plan is read-only until submit_plan, which ends the turn; ptc permits direct run_code only. All modes permit session-only state and todo updates. The current mode is supplied in a tail reminder.";

pub struct PromptPiece {
    /// Section category (`system`, `project`, or `skills`) for context accounting.
    pub title: &'static str,
    pub text: String,
}

pub fn build(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
) -> Message {
    compose(&pieces(
        config,
        model,
        cwd,
        context_files,
        skills,
        mcp_summary,
    ))
}

pub fn compose(pieces: &[PromptPiece]) -> Message {
    let capacity = pieces.iter().map(|piece| piece.text.len()).sum::<usize>()
        + pieces.len().saturating_sub(1) * 2;
    let mut text = String::with_capacity(capacity);
    for (index, piece) in pieces.iter().enumerate() {
        if index > 0 {
            text.push_str("\n\n");
        }
        text.push_str(&piece.text);
    }
    Message::system(text)
}

/// Build the stable prefix. Time, jobs, workspace changes, MCP status, and mode belong in tail reminders.
pub fn pieces(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
) -> Vec<PromptPiece> {
    let mut sections = Vec::new();
    let intro = if config.system_prompt.override_text.trim().is_empty() {
        DEFAULT_INTRO
    } else {
        config.system_prompt.override_text.trim()
    };
    sections.push(PromptPiece {
        title: "system",
        text: intro.to_owned(),
    });
    sections.push(PromptPiece {
        title: "system",
        text: TOOL_POLICY.to_owned(),
    });
    sections.push(PromptPiece {
        title: "system",
        text: format!(
            "All modes support run_code. In PTC, it is the only native entry point; other modes may call built-in tools directly. Discover MCP tools only inside run_code: first `await catalog.search({{query:\"\"}})`, then `await catalog.describe(name)`, then `await tools[name](args)`. Discovery does not grant permission.\n\nEach result has `{{content,summary,structuredContent,isError,status,artifact}}`; prefer structuredContent and parse text-only JSON explicitly with `JSON.parse(r.content)`. Failures reject with serializable `code`, `message`, and `result`; use try/catch or Promise.allSettled. Example: `const r = await tools.read({{path:\"README.md\"}}); return r.content;`\n\nIndependent read-only calls may use Promise.all. Standard tool batches allow up to {} concurrent calls; run_code child calls allow up to {}. Writes, bash approval, and session-state updates are barriers: await all calls before returning. Return only final information; intermediate results are not added to model history. `state.store(key,json)`, `state.load(key)`, `state.remove(key)`, and `state.keys()` persist session JSON; missing keys return undefined, writes commit immediately, and later failures do not roll them back. JavaScript globals do not persist. WASM has no file, network, process, or timer APIs; use host tools.",
            config.tools.max_concurrent_calls,
            config.tools.ptc.max_concurrent_calls,
        ),
    });
    if !mcp_summary.is_empty() {
        sections.push(PromptPiece {
            title: "tools",
            text: mcp_summary.to_owned(),
        });
    }
    if !config.system_prompt.append.trim().is_empty() {
        sections.push(PromptPiece {
            title: "system",
            text: config.system_prompt.append.trim().to_owned(),
        });
    }
    if !context_files.is_empty() {
        let mut rules = String::from("<repo-rules>");
        for file in context_files {
            rules.push_str("\n<file path=\"");
            rules.push_str(&file.path.display().to_string());
            rules.push_str("\">\n");
            rules.push_str(&file.content);
            if !file.content.ends_with('\n') {
                rules.push('\n');
            }
            rules.push_str("</file>");
        }
        rules.push_str("\n</repo-rules>");
        sections.push(PromptPiece {
            title: "project",
            text: rules,
        });
    }
    if let Some(catalog) =
        crate::skills::catalog::render(skills, &config.skills, model.context_window)
    {
        sections.push(PromptPiece {
            title: "skills",
            text: catalog,
        });
    }
    sections.push(PromptPiece {
        title: "system",
        text: format!(
            "<env>\ncwd: {}\nplatform: {} {}\nmodel: {}\n</env>",
            cwd.display(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            model.id,
        ),
    });
    sections
}

pub fn mode_reminder(mode: SessionMode) -> Message {
    let text = match mode {
        SessionMode::Normal => {
            "Current mode: normal. Workspace changes are allowed; follow the tool policies above."
                .to_owned()
        }
        SessionMode::Ask => {
            "Current mode: ask. The workspace is read-only. Read and search are allowed; do not modify files. run_code inherits this restriction."
                .to_owned()
        }
        SessionMode::Plan => {
            "Current mode: plan. The workspace is read-only while exploring. When the plan is complete, call submit_plan and end the turn. run_code inherits this restriction."
                .to_owned()
        }
        SessionMode::Ptc => {
            "Current mode: ptc. Call tools through run_code only; direct native calls other than run_code are rejected. Child calls still obey tool enablement, mode policy, and the bash sandbox."
                .to_owned()
        }
        SessionMode::ClmExperimental => crate::clm::SYSTEM_INSTRUCTION.to_owned(),
    };
    reminder_message(format!("<system-reminder>{text}</system-reminder>"))
}

/// Put volatile facts after replayed history so the stable prefix remains cacheable.
pub fn reminder_message(text: String) -> Message {
    Message::user_text(text)
}
pub fn merge_reminders(messages: Vec<Message>) -> Vec<Message> {
    if messages.is_empty() {
        Vec::new()
    } else {
        vec![Message::user_text(
            messages
                .into_iter()
                .map(|message| message.text())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreConfig;
    use std::fs;

    fn resolved(cwd: &Path, home: &Path) -> ResolvedConfig {
        let raw = "[models.roles]\ndefault='p/model'\nsmol='p/model'\nslow='p/model'\nplan='p/model:xhigh'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model]\n";
        fs::create_dir_all(home).expect("create home");
        fs::write(home.join("config.toml"), raw).expect("write config");
        let core = CoreConfig {
            home: home.to_owned(),
            cwd: cwd.to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
        };
        ResolvedConfig::load(&core)
            .expect("load config")
            .for_project(cwd)
            .expect("resolve project")
    }

    #[test]
    fn stable_prompt_has_rules_mode_model_and_code_contract() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).expect("create repo");
        fs::write(cwd.join("AGENTS.md"), "Follow local rule.\n").expect("write rule");
        let config = resolved(&cwd, &home);
        let model = config.resolve_role("default").expect("resolve model");
        let context_files =
            context_files::load_context_files(&cwd, &config.home, &config.context_files);
        let message = build(
            &config,
            &model,
            &cwd,
            &context_files,
            &crate::skills::SkillIndex::empty(),
            "<mcp-servers>\ngithub\n</mcp-servers>",
        );
        let text = message.text();
        assert!(text.starts_with(DEFAULT_INTRO));
        assert!(text.contains("Follow local rule."));
        assert!(text.contains("model: p/model"));
        assert!(text.contains("<mcp-servers>"));
        assert!(text.contains("structuredContent"));
        assert!(text.contains("state.store"));
        assert!(text.contains("Standard tool batches allow up to 16"));
        assert!(!text.contains("Current mode:"));
    }

    #[test]
    fn skill_catalog_is_included_when_present() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(cwd.join(".agents/skills/demo")).expect("create skill");
        fs::write(
            cwd.join(".agents/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\nbody",
        )
        .expect("write skill");
        let config = resolved(&cwd, &home);
        let model = config.resolve_role("default").expect("resolve model");
        let skills =
            crate::skills::discovery::discover_index(&cwd, &home, Some(&home), &config.skills);
        let context_files =
            context_files::load_context_files(&cwd, &config.home, &config.context_files);
        let message = build(&config, &model, &cwd, &context_files, &skills, "");
        assert!(message.text().contains("<skills>"));
        assert!(message.text().contains("- demo: demo skill"));
    }
}
