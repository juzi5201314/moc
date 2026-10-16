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
const TOOL_POLICY_REST: &str = "Do not use bash, cat, grep, rg, find, ls, sed, awk, perl, Python, redirection, or similar commands to read, search, or edit workspace files. Before editing, read the current file; edit uses the three-character anchors in read output, formatted as `ABC│line`. If anchors may be stale, read again.\n\nSession-internal resources are read-only URIs that only read, grep, find, and job touch. Their shapes: `artifact://jobs/<file>` and `artifact://artifacts/<file>` (this session's job logs and archived tool output; copy them from notices or `job` op=list), `local://<id>.md` (a plan), `history://transcript` (the JSONL transcript), `skill://<name>/SKILL.md` (a discovered skill), `viking://<path>` (remote OpenViking). A bare scheme (`artifact://`, `artifact://jobs`, `local://`, `history://`, `skill://`) lists that namespace when read, and read accepts every form. Never pass a resource URI to bash or to any other program: read the resource with its owning tool instead, and for a job log use `job` op=output.\n\nA backgrounded job reports its completion notice (output preview plus log URI) automatically as a reminder. Keep working while it runs; use `job` op=output for a bounded log tail and op=wait to block, never sleep, re-run, or poll a job log through bash.\n\nModes are enforced when tools execute: normal permits workspace changes; ask is read-only; plan is read-only, write_plan saves the plan and exit_plan ends the round for review; ptc permits direct run_code only. All modes permit session-only state and todo updates. A non-normal mode restates its own contract in a tail reminder; no such reminder means normal.";
/// 项目指令块的导语：标明这些文件的身份，避免与工具政策混读。
const REPO_RULES_INTRO: &str = "MUST follow these context files for all tasks:";

/// 指令片段名：正文窗用它作标题，模型配置也按它指定角色归属。
pub const PIECE_INTRO: &str = "intro";
pub const PIECE_TOOL_POLICY: &str = "tool policy";
pub const PIECE_PTC_RUNTIME: &str = "ptc runtime";
pub const PIECE_MCP_SERVERS: &str = "mcp servers";
pub const PIECE_REPO_RULES: &str = "repo rules";
pub const PIECE_SKILLS: &str = "skills";
pub const PIECE_SUBAGENT: &str = "subagent";
pub const PIECE_ENVIRONMENT: &str = "environment";
pub const PIECE_APPEND: &str = "append";
/// 全部片段名；配置校验与角色归属都以它为准。
pub const PIECE_NAMES: [&str; 9] = [
    PIECE_INTRO,
    PIECE_TOOL_POLICY,
    PIECE_PTC_RUNTIME,
    PIECE_MCP_SERVERS,
    PIECE_REPO_RULES,
    PIECE_SKILLS,
    PIECE_SUBAGENT,
    PIECE_ENVIRONMENT,
    PIECE_APPEND,
];

/// 配置里的片段名 → 规范名；未知名字返回 `None`，配置加载据此报错。
pub fn canonical_piece_name(name: &str) -> Option<&'static str> {
    PIECE_NAMES.into_iter().find(|candidate| *candidate == name)
}

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

/// 组装完成的指令消息序列。
pub struct SystemPrompt {
    /// 指令消息：保持 system 的片段组成首条消息，其余片段组成随后的 user 消息；
    /// 默认配置（指令角色为 system）下只有一条 system 消息。
    pub messages: Vec<Message>,
    /// 每条指令消息的静态前缀块数，与 `messages` 一一对应；缓存断点落在
    /// `messages[i]` 的第 `static_prefix[i] - 1` 块上。
    pub static_prefix: Vec<usize>,
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
    compose(
        pieces(
            config,
            model,
            cwd,
            context_files,
            skills,
            mcp_summary,
            subagent,
        ),
        model,
    )
}

/// 按片段拼装指令消息：system 组在前、user 组在后，组内保持片段顺序。
///
/// 角色分组只影响消息边界与角色，片段正文逐字不变；默认配置（指令角色为
/// system）下所有片段都留在 system，只有一条消息，与单条形态完全一致。内容块
/// 边界只服务于本地缓存断点：线上拍平（Chat / Responses）与无断点时的字符串
/// 形态（Anthropic）都由编码器补回块间空行，文本与单块形态逐字一致。
pub fn compose(pieces: Vec<PromptPiece>, model: &ResolvedModel) -> SystemPrompt {
    let mut groups: [Vec<PromptPiece>; 2] = [Vec::new(), Vec::new()];
    for piece in pieces {
        let group = usize::from(!model.piece_keeps_system(piece.name));
        groups[group].push(piece);
    }
    let roles = [Role::System, model.instruction_role.role()];
    let mut messages = Vec::with_capacity(2);
    let mut static_prefix = Vec::with_capacity(2);
    for (group, role) in groups.into_iter().zip(roles) {
        if group.is_empty() {
            continue;
        }
        static_prefix.push(
            group
                .iter()
                .take_while(|piece| piece.scope == PieceScope::Static)
                .count(),
        );
        messages.push(Message {
            role,
            content: group
                .into_iter()
                .map(|piece| ContentPart::Text(piece.text))
                .collect(),
        });
    }
    SystemPrompt {
        messages,
        static_prefix,
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
        name: PIECE_INTRO,
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
        name: PIECE_TOOL_POLICY,
        scope: PieceScope::Static,
        text: tool_policy,
    });
    sections.push(PromptPiece {
        title: "system",
        name: PIECE_PTC_RUNTIME,
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
            name: PIECE_MCP_SERVERS,
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
            name: PIECE_REPO_RULES,
            scope: PieceScope::Project,
            text: rules,
        });
    }
    if let Some(catalog) =
        crate::skills::catalog::render(skills, &config.skills, model.context_window)
    {
        sections.push(PromptPiece {
            title: "skills",
            name: PIECE_SKILLS,
            scope: PieceScope::Project,
            text: catalog,
        });
    }
    if let Some(subagent) = subagent {
        sections.push(PromptPiece {
            title: "project",
            name: PIECE_SUBAGENT,
            scope: PieceScope::Project,
            text: subagent.to_owned(),
        });
    }
    sections.push(PromptPiece {
        title: "system",
        name: PIECE_ENVIRONMENT,
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
            name: PIECE_APPEND,
            scope: PieceScope::Project,
            text: config.system_prompt.append.trim().to_owned(),
        });
    }
    sections
}

/// 当前模式的行为契约，`None` 表示普通模式不需要额外声明。
///
/// 契约只走尾部提醒：静态系统提示保持与模式无关（跨目录、跨会话复用缓存），
/// 因此模式不能靠"当前是 X、去上面找规则"这种指针，必须自带该模式该做什么。
/// 普通模式没有增量——工具面是完整的，静态政策也已写明它允许改工作区——不发声明
/// 本身就是"当前是普通模式"的信号。
pub fn mode_contract(mode: SessionMode) -> Option<Message> {
    let text = match mode {
        SessionMode::Normal => return None,
        SessionMode::Ask => {
            "Ask mode is active. The workspace is read-only: read and search freely, but do not modify files. run_code inherits the read-only restriction."
        }
        SessionMode::Plan => {
            "Plan mode is active. The workspace is read-only while exploring. When the plan is complete, save it with write_plan and end the round with exit_plan to request review. run_code inherits the read-only restriction."
        }
        SessionMode::Ptc => {
            "PTC mode is active. Call tools only through run_code; direct native calls other than run_code are rejected. Child calls still obey tool enablement, mode policy, and the bash sandbox."
        }
        SessionMode::ClmExperimental => crate::clm::SYSTEM_INSTRUCTION,
    };
    Some(reminder_message(format!(
        "<system-reminder>{text}</system-reminder>"
    )))
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
        resolved_with_compat(cwd, home, "")
    }

    /// 带 compat 覆写的配置：用于覆盖指令角色等逐模型开关。
    fn resolved_with_compat(cwd: &Path, home: &Path, compat: &str) -> ResolvedConfig {
        let raw = format!(
            "[models.roles]\ndefault='p/model'\nexplore='p/model'\nplan='p/model:xhigh'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model]\n[providers.p.models.model.compat]\n{compat}\n"
        );
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

    /// 指令消息的文本块，按线上顺序；测试据此断言块边界。
    fn blocks(prompt: &SystemPrompt) -> Vec<String> {
        prompt
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .map(|part| match part {
                ContentPart::Text(text) => text.clone(),
                other => panic!("指令消息只应包含文本块，收到 {other:?}"),
            })
            .collect()
    }

    /// 指令消息的纯文本形态，按线上顺序拼接。
    fn prompt_text(prompt: &SystemPrompt) -> String {
        prompt
            .messages
            .iter()
            .map(Message::instruction_text)
            .collect::<Vec<_>>()
            .join("\n\n")
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
        let text = prompt_text(&prompt);
        assert!(text.starts_with(DEFAULT_INTRO));
        assert!(text.contains("Follow local rule."));
        assert!(text.contains(REPO_RULES_INTRO));
        assert!(text.contains("model: p/model"));
        assert!(text.contains("<mcp-servers>"));
        assert!(text.contains("structuredContent"));
        assert!(text.contains("state.store"));
        assert!(text.contains("Standard tool batches allow up to 16"));
        assert!(!text.contains("Current mode:"));
        // 默认配置下所有片段留在 system：只有一条消息，静态前缀即前三块。
        assert_eq!(prompt.messages.len(), 1);
        assert_eq!(prompt.messages[0].role, Role::System);
        assert_eq!(prompt.static_prefix, vec![3]);
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

    /// 工具政策必须声明内部 URI 的归属工具与后台任务的结果投递：模型不再拿
    /// bash 去 tail `artifact://jobs/job_1.log` 的依据只能来自这里。
    #[test]
    fn tool_policy_declares_internal_uris_and_job_delivery() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        let config = resolved(&cwd, &home);
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
        let text = prompt_text(&prompt);
        for uri in [
            "artifact://jobs/<file>",
            "artifact://artifacts/<file>",
            "local://<id>.md",
            "history://transcript",
            "skill://<name>/SKILL.md",
            "viking://<path>",
        ] {
            assert!(text.contains(uri), "工具政策缺少 {uri}：{text}");
        }
        assert!(
            text.contains("A bare scheme")
                && text.contains("lists that namespace when read")
                && text.contains("read accepts every form"),
            "必须说明裸 scheme 是命名空间列表入口：{text}"
        );
        assert!(
            text.contains("Never pass a resource URI to bash or to any other program"),
            "必须禁止把 URI 交给 shell 或任何程序：{text}"
        );
        assert!(
            text.contains("reports its completion notice")
                && text.contains("never sleep, re-run, or poll"),
            "必须说明后台任务的完成通知自动投递且禁止轮询：{text}"
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
        assert_eq!(prompt.static_prefix, vec![3]);
    }

    /// 指令角色切到 user：规定性片段改走 user 消息，描述性片段留在 system，
    /// 片段正文逐字不变，只是分组与排列顺序不同。
    #[test]
    fn instruction_role_user_splits_pieces_without_rewriting_them() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).expect("create repo");
        fs::write(cwd.join("AGENTS.md"), "Follow local rule.\n").expect("write rule");
        let config = resolved_with_compat(&cwd, &home, "instruction_role = \"user\"");
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
        // 两条消息：system 组承载描述性片段，user 组承载规定性片段。
        assert_eq!(prompt.messages.len(), 2);
        assert_eq!(prompt.messages[0].role, Role::System);
        assert_eq!(prompt.messages[1].role, Role::User);
        assert_eq!(prompt.static_prefix, vec![1, 2]);
        let system = prompt.messages[0].text();
        assert!(system.starts_with(DEFAULT_INTRO));
        assert!(system.contains("<mcp-servers>"));
        assert!(system.contains("model: p/model"));
        assert!(!system.contains(TOOL_POLICY), "工具政策不得留在 system 组");
        let user = prompt.messages[1].text();
        assert!(user.starts_with(TOOL_POLICY));
        assert!(user.contains("structuredContent"));
        assert!(user.contains(REPO_RULES_INTRO));
        assert!(user.contains("Follow local rule."));
        assert!(!user.contains("<env>"), "环境事实不得混进 user 组");

        // 与默认配置相比：片段正文逐字相同，只有分组与顺序不同。
        let plain_config = resolved(&cwd, &home);
        let plain_model = plain_config.resolve_role("default").expect("resolve model");
        let plain = build(
            &plain_config,
            &plain_model,
            &cwd,
            &context_files,
            &crate::skills::SkillIndex::empty(),
            "<mcp-servers>\ngithub\n</mcp-servers>",
            None,
        );
        assert_eq!(plain.messages.len(), 1);
        let mut split = blocks(&prompt);
        let mut single = blocks(&plain);
        split.sort();
        single.sort();
        assert_eq!(split, single);
    }

    /// 未知片段名在配置加载期报错，而不是静默退化成整条 user。
    #[test]
    fn unknown_keep_system_piece_name_fails_config_load() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).expect("create repo");
        let raw = format!(
            "[models.roles]\ndefault='p/model'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model.compat]\ninstruction_system=[\"{}\"]\n",
            "tool polcy"
        );
        fs::create_dir_all(&home).expect("create home");
        fs::write(home.join("config.toml"), raw).expect("write config");
        let core = CoreConfig {
            home: home.to_owned(),
            cwd: cwd.to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
        };
        let error = ResolvedConfig::load(&core)
            .and_then(|config| config.for_project(&cwd))
            .err()
            .expect("未知片段名必须报错");
        assert!(error.to_string().contains("tool polcy"), "{error}");
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
            prompt_text(&build(
                config,
                &model,
                &cwd,
                &context_files,
                &crate::skills::SkillIndex::empty(),
                "",
                None,
            ))
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
        let text = prompt_text(&prompt);
        assert!(text.contains("<skills>"));
        assert!(text.contains("- demo: demo skill"));
    }

    /// 模式契约：普通模式不发声明，其余模式自带该模式该做什么（不是名字指针）。
    #[test]
    fn mode_contract_only_declares_non_normal_modes() {
        assert!(
            mode_contract(SessionMode::Normal).is_none(),
            "普通模式没有任何增量，不发声明本身就是信号"
        );
        for (mode, needles) in [
            (
                SessionMode::Ask,
                ["Ask mode is active", "read-only"].as_slice(),
            ),
            (
                SessionMode::Plan,
                ["Plan mode is active", "write_plan", "exit_plan"].as_slice(),
            ),
            (
                SessionMode::Ptc,
                ["PTC mode is active", "only through run_code"].as_slice(),
            ),
            (
                SessionMode::ClmExperimental,
                ["<clm-experimental>", "clm_context"].as_slice(),
            ),
        ] {
            let text = mode_contract(mode).expect("非普通模式必须有契约").text();
            assert!(
                text.starts_with("<system-reminder>") && text.ends_with("</system-reminder>"),
                "{mode:?}: {text}"
            );
            for needle in needles {
                assert!(text.contains(needle), "{mode:?} 缺少「{needle}」：{text}");
            }
            assert!(
                !text.contains("follow the tool policies above"),
                "{mode:?} 不得只报模式名让模型自己去上面找规则：{text}"
            );
        }
    }
}
