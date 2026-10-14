//! Stable system prefix and volatile tail reminders.

pub mod context_files;
pub mod reminder;

use std::path::Path;

use moc_llm::{ContentPart, Message, Role};
use moc_protocol::methods::SessionMode;

use crate::config::ResolvedConfig;
use crate::config::model::ResolvedModel;

const DEFAULT_INTRO: &str = "You are a precise software-engineering agent working in the user's local workspace through tools.";
const TOOL_POLICY: &str = "Use read for files and directories, grep for content, find for paths, edit for anchored local changes, write only for new or complete-file writes, and bash only when a shell is required.";
/// 仅当 web_search/web_fetch 对模型可见时拼入工具政策。
const WEB_SEARCH_POLICY: &str = "Use web_search for current web information and web_fetch to read specific URLs; cite the URLs you use.";
const TOOL_POLICY_REST: &str = "Do not use bash, cat, grep, rg, find, ls, sed, awk, perl, Python, redirection, or similar commands to read, search, or edit workspace files. Before editing, read the current file; edit uses the three-character anchors in read output, formatted as `ABC│line`. If anchors may be stale, read again.\n\nModes are enforced when tools execute: normal permits workspace changes; ask is read-only; plan is read-only, write_plan saves the plan and exit_plan ends the round for review; ptc permits direct run_code only. All modes permit session-only state and todo updates. The current mode is supplied in a tail reminder.";
/// 项目指令块的导语：标明这些文件的身份，避免与工具政策混读。
const REPO_RULES_INTRO: &str = "MUST follow these context files for all tasks:";

/// 片段作用域：决定缓存断点落位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceScope {
    /// 与工作目录无关的静态前缀：断点落在它的最后一块上，跨目录、跨会话共享缓存。
    Static,
    /// 工作目录或会话相关的内容：排在静态前缀之后，不参与静态缓存。
    Project,
}

#[derive(Clone)]
pub struct PromptPiece {
    /// Section category (`system`, `project`, or `skills`) for context accounting.
    pub title: &'static str,
    /// 正文分块的短名（1-3 个英文单词）：上下文正文窗的标题行直接用这个，
    /// 与 `title` 的占用分组名互不影响。
    pub name: &'static str,
    /// 片段作用域。
    pub scope: PieceScope,
    pub text: String,
}

/// 组装完成的系统提示。
pub struct SystemPrompt {
    /// 系统消息：每个片段一个内容块，顺序即线上顺序。
    pub message: Message,
    /// 静态前缀块数；缓存断点落在第 `cache_prefix_blocks - 1` 块。
    pub cache_prefix_blocks: usize,
}

pub fn build(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
    subagent: Option<&str>,
) -> SystemPrompt {
    compose(pieces(
        config,
        model,
        cwd,
        context_files,
        skills,
        mcp_summary,
        subagent,
    ))
}

/// 按片段拼装系统提示：每块一个内容块，静态前缀在最前。
///
/// 内容块边界只服务于本地缓存断点；线上拍平（Chat / Responses）与无断点时的
/// 字符串形态（Anthropic）都由编码器补回块间空行，文本与单块形态逐字一致。
pub fn compose(pieces: Vec<PromptPiece>) -> SystemPrompt {
    let cache_prefix_blocks = pieces
        .iter()
        .take_while(|piece| piece.scope == PieceScope::Static)
        .count();
    let content = pieces
        .into_iter()
        .map(|piece| ContentPart::Text(piece.text))
        .collect();
    SystemPrompt {
        message: Message {
            role: Role::System,
            content,
        },
        cache_prefix_blocks,
    }
}

/// Build the stable prefix. Time, jobs, workspace changes, MCP status, and mode belong in tail reminders.
pub fn pieces(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
    subagent: Option<&str>,
) -> Vec<PromptPiece> {
    let mut sections = Vec::new();
    let intro = if config.system_prompt.override_text.trim().is_empty() {
        DEFAULT_INTRO
    } else {
        config.system_prompt.override_text.trim()
    };
    sections.push(PromptPiece {
        title: "system",
        name: "intro",
        scope: PieceScope::Static,
        text: intro.to_owned(),
    });
    let mut tool_policy = String::with_capacity(
        TOOL_POLICY.len() + TOOL_POLICY_REST.len() + WEB_SEARCH_POLICY.len() + 2,
    );
    tool_policy.push_str(TOOL_POLICY);
    if config.web_search.available() {
        tool_policy.push(' ');
        tool_policy.push_str(WEB_SEARCH_POLICY);
    }
    tool_policy.push(' ');
    tool_policy.push_str(TOOL_POLICY_REST);
    sections.push(PromptPiece {
        title: "system",
        name: "tool policy",
        scope: PieceScope::Static,
        text: tool_policy,
    });
    sections.push(PromptPiece {
        title: "system",
        name: "ptc runtime",
        scope: PieceScope::Static,
        text: format!(
            "All modes support run_code. In PTC, it is the only native entry point; other modes may call built-in tools directly. Discover MCP tools only inside run_code: first `await catalog.search({{query:\"\"}})`, then `await catalog.describe(name)`, then `await tools[name](args)`. Discovery does not grant permission.\n\nEach result has `{{content,summary,structuredContent,isError,status,artifact}}`; prefer structuredContent and parse text-only JSON explicitly with `JSON.parse(r.content)`. Failures reject with serializable `code`, `message`, and `result`; use try/catch or Promise.allSettled. Example: `const r = await tools.read({{path:\"README.md\"}}); return r.content;`\n\nIndependent read-only calls may use Promise.all. Standard tool batches allow up to {} concurrent calls; run_code child calls allow up to {}. Writes, bash approval, and session-state updates are barriers: await all calls before returning. Return only final information; intermediate results are not added to model history. `state.store(key,json)`, `state.load(key)`, `state.remove(key)`, and `state.keys()` persist session JSON; missing keys return undefined, writes commit immediately, and later failures do not roll them back. JavaScript globals do not persist. WASM has no file, network, process, or timer APIs; use host tools.",
            config.tools.max_concurrent_calls,
            config.tools.ptc.max_concurrent_calls,
        ),
    });
    if !mcp_summary.is_empty() {
        sections.push(PromptPiece {
            title: "tools",
            name: "mcp servers",
            scope: PieceScope::Project,
            text: mcp_summary.to_owned(),
        });
    }
    if !context_files.is_empty() {
        let mut rules = String::from("<repo-rules>\n");
        rules.push_str(REPO_RULES_INTRO);
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
            name: "repo rules",
            scope: PieceScope::Project,
            text: rules,
        });
    }
    if let Some(catalog) =
        crate::skills::catalog::render(skills, &config.skills, model.context_window)
    {
        sections.push(PromptPiece {
            title: "skills",
            name: "skills",
            scope: PieceScope::Project,
            text: catalog,
        });
    }
    if let Some(subagent) = subagent {
        sections.push(PromptPiece {
            title: "project",
            name: "subagent",
            scope: PieceScope::Project,
            text: subagent.to_owned(),
        });
    }
    sections.push(PromptPiece {
        title: "system",
        name: "environment",
        scope: PieceScope::Project,
        text: format!(
            "<env>\ncwd: {}\nplatform: {} {}\nmodel: {}\n</env>",
            cwd.display(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            model.id,
        ),
    });
    // 追加指令放在最后：它是用户对本会话最直接的指令，权威顺序上最靠近模型。
    if !config.system_prompt.append.trim().is_empty() {
        sections.push(PromptPiece {
            title: "system",
            name: "append",
            scope: PieceScope::Project,
            text: config.system_prompt.append.trim().to_owned(),
        });
    }
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
            "Current mode: plan. The workspace is read-only while exploring. When the plan is complete, call write_plan with its title and markdown to save it, then call exit_plan with an empty object to end this round and request review. run_code inherits this restriction."
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
        let raw = "[models.roles]\ndefault='p/model'\nexplore='p/model'\nplan='p/model:xhigh'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model]\n";
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

    /// 系统提示的文本块，按线上顺序；测试据此断言块边界。
    fn blocks(prompt: &SystemPrompt) -> Vec<String> {
        prompt
            .message
            .content
            .iter()
            .map(|part| match part {
                ContentPart::Text(text) => text.clone(),
                other => panic!("系统提示只应包含文本块，收到 {other:?}"),
            })
            .collect()
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
        let prompt = build(
            &config,
            &model,
            &cwd,
            &context_files,
            &crate::skills::SkillIndex::empty(),
            "<mcp-servers>\ngithub\n</mcp-servers>",
            None,
        );
        let text = prompt.message.instruction_text();
        assert!(text.starts_with(DEFAULT_INTRO));
        assert!(text.contains("Follow local rule."));
        assert!(text.contains(REPO_RULES_INTRO));
        assert!(text.contains("model: p/model"));
        assert!(text.contains("<mcp-servers>"));
        assert!(text.contains("structuredContent"));
        assert!(text.contains("state.store"));
        assert!(text.contains("Standard tool batches allow up to 16"));
        assert!(!text.contains("Current mode:"));
        // 静态前缀只含与工作目录无关的模板块，工作目录相关内容全部在其后。
        assert_eq!(prompt.cache_prefix_blocks, 3);
        let blocks = blocks(&prompt);
        assert!(blocks[0].starts_with(DEFAULT_INTRO));
        assert!(blocks[1].starts_with(TOOL_POLICY));
        assert!(blocks[2].contains("run_code"));
        assert!(blocks[3].starts_with("<mcp-servers>"));
        assert!(blocks[4].starts_with("<repo-rules>\n"));
        assert!(
            !blocks[..3]
                .iter()
                .any(|block| block.contains(&cwd.display().to_string()))
        );
    }

    #[test]
    fn append_text_lands_after_project_content() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).expect("create repo");
        let mut config = resolved(&cwd, &home);
        config.system_prompt.append = "Always answer in Chinese.".to_owned();
        let model = config.resolve_role("default").expect("resolve model");
        let prompt = build(
            &config,
            &model,
            &cwd,
            &[],
            &crate::skills::SkillIndex::empty(),
            "",
            None,
        );
        let blocks = blocks(&prompt);
        assert_eq!(
            blocks.last().map(String::as_str),
            Some("Always answer in Chinese.")
        );
        assert_eq!(prompt.cache_prefix_blocks, 3);
    }

    #[test]
    fn web_search_policy_follows_tool_visibility() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).expect("create repo");
        let mut config = resolved(&cwd, &home);
        let model = config.resolve_role("default").expect("resolve model");
        let context_files =
            context_files::load_context_files(&cwd, &config.home, &config.context_files);
        let prompt = |config: &ResolvedConfig| {
            build(
                config,
                &model,
                &cwd,
                &context_files,
                &crate::skills::SkillIndex::empty(),
                "",
                None,
            )
            .message
            .instruction_text()
        };
        assert!(
            !prompt(&config).contains("web_search"),
            "未配置密钥时系统提示不得提及 web 工具"
        );
        config.web_search.api_key = Some("tvly-test".to_owned());
        assert!(prompt(&config).contains("web_search"));
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
        let prompt = build(&config, &model, &cwd, &context_files, &skills, "", None);
        let text = prompt.message.instruction_text();
        assert!(text.contains("<skills>"));
        assert!(text.contains("- demo: demo skill"));
    }
}
