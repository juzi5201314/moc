//! 系统提示：稳定前缀与每轮现算的尾部提醒。
//!
//! # 提示词编写规范（改任何模型可见文本前先读）
//!
//! 模型可见文本包括：本模块的静态片段与模式提醒、各工具的 `description` 与 schema
//! 字段说明、工具结果与错误、notice 和尾部提醒、压缩请求与回放包装。这些文本每轮都
//! 计入 token，编写时遵守：
//!
//! 1. **一处一事实**：每条事实只写在一个地方。工具用法写在该工具的 description；
//!    跨工具纪律（不用 bash 读写搜索、内部 URI、后台任务结果投递）只写在工具政策；
//!    当前引擎的规则只写在 [`engine_reminder`]；schema 已表达的枚举、默认值、范围不在
//!    description 里重复。
//! 2. **只写模型用得上的信息**：不写模型无法调节的实现细节（并行扫描、超时协作等），
//!    不写客套话、动机解释和「请注意」之类的铺垫。
//! 3. **短句祈使**：优先以动词开头，用分号连接；能用 `a/b/c` 列举就不展开成句。
//! 4. **模型可见文本用英文**：英文 token 密度更高。给前端看的文案（GuardNotice、
//!    RPC 错误）不受此限。
//! 5. **测试锚点**：断言里的子串（如 `Ask mode is active`、`Standard tool batches allow up to`）
//!    用来检验行为约定；改写时保留语义并同步改断言，不要为了过测试塞回废话。
//! 6. 加新文本前先找有没有同义的现成表述，有就复用或合并，没有再加，并按上面的密度写。

pub mod context_files;
pub mod reminder;

use std::path::Path;

use moc_llm::{ContentPart, Message, Role};
use moc_protocol::SessionPermission;
use moc_protocol::methods::SessionEngine;

use crate::config::ResolvedConfig;
use crate::config::ResolvedSandboxConfig;
use crate::config::model::ResolvedModel;

const DEFAULT_INTRO: &str = "You are a precise software-engineering agent working in the user's local workspace through tools.";
/// 回复里的图怎么写。自定义身份会换掉 [`DEFAULT_INTRO`]，这句仍接在后面：
/// 终端和网页都按这个契约渲染，换身份不该把图画不出来。
const VISUAL_GUIDE: &str = "Use a ```mermaid fence for flow, sequence, state, or dependencies, and a ```svg fence for a layout, mockup, or geometry mermaid cannot draw; both render inline, so do not restate the figure. In SVG use only currentColor and var(--fg), var(--muted), var(--border), var(--surface), var(--accent), var(--success), var(--warning), var(--error); no background, scripts, animation, foreignObject, or external hrefs.";
// 跨工具纪律只写在这里；单个工具的用法写在各自的 description，这里不重复。
const TOOL_POLICY: &str = "Use read for files and directories, grep for content, find for paths, edit for anchored changes, write for new or complete files, and bash only when a shell is required.";
/// web_search/web_fetch 按各自可见性拼进工具政策。
const WEB_BOTH_POLICY: &str =
    "Use web_search for current information and web_fetch for known URLs; cite the URLs you use.";
const WEB_SEARCH_ONLY_POLICY: &str =
    "Use web_search for current information; cite the URLs you use.";
const WEB_FETCH_ONLY_POLICY: &str = "Use web_fetch for known URLs; cite the URLs you use.";
const TOOL_POLICY_REST: &str = "Do not use bash, cat, grep, rg, find, ls, sed, awk, perl, Python, or redirection to read, search, or edit workspace files. Read before editing; edit takes the anchors from read or diff output (`42sr│line`); the part before │ is the line number plus the two-letter id assigned when the line was shown. An untouched line keeps its anchor when edits shift it; use the newest anchor shown for a line, and re-read if an edit reports a stale anchor.\n\nSession resources are read-only URIs for read, grep, find, and job: `artifact://jobs/<file>` and `artifact://artifacts/<file>` (job logs, archived tool output; copy URIs from notices or `job` op=list), `local://<id>.md` (plan), `history://transcript` (JSONL transcript), `skill://<name>/SKILL.md`. A bare scheme (`artifact://`, `artifact://jobs`, `local://`, `history://`, `skill://`) lists that namespace when read. Never pass a resource URI to bash or to any other program; for a job log use `job` op=output.\n\nA background job reports its completion notice (output preview and log URI) automatically. Keep working meanwhile; use `job` op=output for a log tail or op=wait to block; never sleep, re-run, or poll a job log through bash.\n\nExecution modes are enforced at execution; session state and todo updates are allowed in every mode. A non-default mode states its rules in a tail reminder; no such reminder means default mode, which permits workspace changes.";
/// 项目指令块的开头一句：告诉模型下面是仓库规则文件，别和工具政策混着读。
const REPO_RULES_INTRO: &str = "MUST follow these context files for all tasks:";

/// Fusion lead 的分工契约：只在 Fusion 顶层会话注入，子代理与普通会话没有。
/// 静态片段，排在 ptc runtime 之后，保持静态前缀连续。
const FUSION_PIECE: &str = "<fusion>
You are the lead of a Fusion pair. A persistent sidekick agent (a cheaper model on the same machine, filesystem and workspace) works under your direction through the `sidekick` tool. The user talks only to you: do not mention the sidekick unless asked, and describe all work as your own.

Division of labor
- Keep for yourself: understanding the request, investigations whose findings you will state as fact, planning and design decisions, correctness-critical work (data analysis, numbers, queries, prompt or eval text, security-sensitive logic: anything where a wrong result looks plausible instead of failing loudly), reviewing the sidekick's work, and all communication with the user.
- Delegate by default: mechanical implementation of a settled plan, multi-file edits, environment setup and repair, builds, linters, type-checks and test runs, and reproducing results. Do a step yourself only when it is trivially small: one or two of your own turns with nothing left to verify.
- Never hand off implementation of an unsettled ask. Finish the exploration, or get the user's answer, first.
- Commits, pushes and other actions with external side effects stay with you unless you explicitly assign them.

Writing a handoff
- The sidekick sees only your messages and what it discovers itself, never the user's messages or your reasoning. State the goal, the concrete design (files, interfaces, data shapes, existing helpers to reuse), the hard edge cases, the constraints, and the exact verification: the narrowest commands that cover the change.
- Settle consequential choices yourself; never leave alternatives for the sidekick to pick. Mark anything unverified as a hypothesis for it to check.
- Pass results you already derived as settled inputs; do not ask it to redo them without reason.
- It remembers earlier handoffs, files it read and processes it started, so do not repeat context it already has. When servers or long-running processes are involved, say whether to reuse or restart them and whether to leave them running.
- Ask for evidence (diffs, command output, paths to logs and artifacts) rather than prose claims.

Running handoffs
- block=true (default) waits and returns the report. Use block=false only when you have real parallel work; then collect the report with `wait_agent` before ending your turn, never guess what it will say.
- Calling `sidekick` while it runs interrupts it with your message: use this to redirect, add information, or tell it to wrap up.
- If the user writes while a handoff is in flight, decide what it means for the running work and redirect the sidekick if needed before waiting again.

Reviewing
- The report is a claim; the evidence is the proof. Read the full diff of code it wrote before building on it or reporting it to the user.
- Review everything first, then send all defects and gaps in one consolidated rework handoff. Do not drip-feed small fixes, and do not take the work over after a single miss; take over only if it is still stuck after a couple of rounds.
- Present a cause as confirmed only when evidence shows that code path actually runs; otherwise report it as a hypothesis with the check that would settle it.
- When the user is waiting on something small and urgent, do it yourself at once and move slow verification off the critical path.

Do not use `spawn_agent` subagents unless the user asks for them.
</fusion>";

/// 提示片段名。上下文正文窗用它作标题，模型配置用它指定片段归哪个角色。
pub const PIECE_INTRO: &str = "intro";
pub const PIECE_TOOL_POLICY: &str = "tool policy";
pub const PIECE_PTC_RUNTIME: &str = "ptc runtime";
pub const PIECE_FUSION: &str = "fusion";
pub const PIECE_TOOL_DECLS: &str = "tool declarations";
pub const PIECE_MCP_SERVERS: &str = "mcp servers";
pub const PIECE_REPO_RULES: &str = "repo rules";
pub const PIECE_SKILLS: &str = "skills";
pub const PIECE_PLUGINS: &str = "plugins";
pub const PIECE_SUBAGENT: &str = "subagent";
pub const PIECE_ENVIRONMENT: &str = "environment";
pub const PIECE_APPEND: &str = "append";
/// 全部片段名，配置校验和角色分组都以此为准。
pub const PIECE_NAMES: [&str; 12] = [
    PIECE_INTRO,
    PIECE_TOOL_POLICY,
    PIECE_PTC_RUNTIME,
    PIECE_FUSION,
    PIECE_TOOL_DECLS,
    PIECE_MCP_SERVERS,
    PIECE_REPO_RULES,
    PIECE_SKILLS,
    PIECE_PLUGINS,
    PIECE_SUBAGENT,
    PIECE_ENVIRONMENT,
    PIECE_APPEND,
];

/// 把配置里的片段名转成规范名；未知名字返回 `None`，由配置加载报错。
pub fn canonical_piece_name(name: &str) -> Option<&'static str> {
    PIECE_NAMES.into_iter().find(|candidate| *candidate == name)
}

/// 片段作用域，决定缓存断点放在哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceScope {
    /// 与工作目录无关的静态前缀。缓存断点放在最后一个静态块上，不同目录和会话共用同一份缓存。
    Static,
    /// 依赖工作目录或会话的内容，排在静态前缀之后，不参与静态缓存。
    Project,
}

#[derive(Clone)]
pub struct PromptPiece {
    /// 上下文占用统计用的分类：`system`、`project`、`skills` 或 `tools`。
    pub title: &'static str,
    /// 片段短名（1-3 个英文单词），上下文正文窗直接用它作标题；与 `title` 的分类互不影响。
    pub name: &'static str,
    pub scope: PieceScope,
    pub text: String,
}

/// 组装好的指令消息。
pub struct SystemPrompt {
    /// 留在 system 的片段组成第一条消息，其余片段组成随后的 user 消息。
    /// 默认配置（指令角色为 system）下只有一条 system 消息。
    pub messages: Vec<Message>,
    /// 每条指令消息开头的静态块数，与 `messages` 一一对应；缓存断点放在
    /// `messages[i]` 的第 `static_prefix[i] - 1` 块上。
    pub static_prefix: Vec<usize>,
}

/// 变长参数全部属于同一次组装：配置、模型与按工作目录取得的项目内容。
#[allow(clippy::too_many_arguments)]
pub fn build(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
    tool_declarations: &str,
    plugins: &str,
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
            tool_declarations,
            plugins,
            subagent,
        ),
        model,
    )
}

/// 把片段拼成指令消息：system 组在前，user 组在后，组内保持片段顺序。
///
/// 分组只决定消息边界和角色，片段正文不变。默认配置（指令角色为 system）下所有片段
/// 都在 system 组，结果只有一条消息。按片段切分内容块只是为了放本地缓存断点：
/// Chat/Responses 会把各块合并成一段文本，Anthropic 没有断点时也发字符串，两种情况
/// 都由编码器在块之间补空行，最终文本与不分块时完全相同。
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

/// 生成稳定前缀的各个片段。时间、后台任务、工作区变更、MCP 服务器状态、MCP 工具索引
/// 和当前模式放在尾部提醒里，不进前缀。
#[allow(clippy::too_many_arguments)]
pub fn pieces(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
    tool_declarations: &str,
    plugins: &str,
    subagent: Option<&str>,
) -> Vec<PromptPiece> {
    let mut sections = Vec::new();
    sections.push(intro_piece(config));
    let web_policy = match (config.web_search.available(), config.web_fetch.enabled) {
        (true, true) => Some(WEB_BOTH_POLICY),
        (true, false) => Some(WEB_SEARCH_ONLY_POLICY),
        (false, true) => Some(WEB_FETCH_ONLY_POLICY),
        (false, false) => None,
    };
    let mut tool_policy = String::with_capacity(
        TOOL_POLICY.len() + TOOL_POLICY_REST.len() + web_policy.map_or(0, str::len) + 2,
    );
    tool_policy.push_str(TOOL_POLICY);
    if let Some(policy) = web_policy {
        tool_policy.push(' ');
        tool_policy.push_str(policy);
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
            "run_code works in every mode; in PTC mode it is the only native tool. MCP tools are reachable only inside run_code: `await catalog.search({{query:\"\"}})`, then `await catalog.describe(name)`, then `await tools[name](args)`; discovery grants no permission.\n\nResults are `{{content,summary,structuredContent,isError,status,artifact}}`; prefer structuredContent, else `JSON.parse(r.content)` for JSON text. Failures reject with serializable `code`, `message`, `result`; use try/catch or Promise.allSettled. Example: `const r = await tools.read({{path:\"README.md\"}}); return r.content;`\n\nIndependent read-only calls may use Promise.all. Standard tool batches allow up to {} concurrent calls; run_code child calls up to {} concurrently and {} total times per script. Writes, bash approval, and session-state updates are barriers. Await all calls before returning and return only final information; intermediate results never reach model history. `state.store(key,json)`, `state.load(key)`, `state.remove(key)`, `state.keys()` persist session JSON (missing key: undefined); writes commit immediately and later failures do not roll them back. JS globals do not persist. The worker has no file, network, process, or timer APIs; use host tools.",
            config.tools.max_concurrent_calls,
            config.tools.ptc.max_concurrent_calls,
            config.tools.ptc.max_total_calls,
        ),
    });
    // Fusion lead 的分工契约：只注入 Fusion 顶层会话，静态片段保持前缀连续。
    if model.fusion.is_some() && subagent.is_none() {
        sections.push(PromptPiece {
            title: "system",
            name: PIECE_FUSION,
            scope: PieceScope::Static,
            text: FUSION_PIECE.to_owned(),
        });
    }
    if !tool_declarations.is_empty() {
        sections.push(PromptPiece {
            title: "tools",
            name: PIECE_TOOL_DECLS,
            scope: PieceScope::Project,
            text: tool_declarations.to_owned(),
        });
    }
    if !mcp_summary.is_empty() {
        sections.push(PromptPiece {
            title: "tools",
            name: PIECE_MCP_SERVERS,
            scope: PieceScope::Project,
            text: mcp_summary.to_owned(),
        });
    }
    if let Some(rules) = repo_rules_piece(context_files) {
        sections.push(rules);
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
    if !plugins.is_empty() {
        sections.push(PromptPiece {
            title: "plugins",
            name: PIECE_PLUGINS,
            scope: PieceScope::Project,
            text: format!("<plugins>\n{plugins}\n</plugins>"),
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
    sections.push(environment_piece(config, model, cwd));
    // 追加指令放在最后，让用户为本会话写的指令紧挨着对话。
    if let Some(append) = append_piece(config) {
        sections.push(append);
    }
    sections
}

/// 旁路只复用身份、仓库规则、环境和追加指令，不发现技能或启动工具宿主。
pub(crate) fn build_btw(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    cwd: &Path,
    context_files: &[context_files::ContextFile],
) -> SystemPrompt {
    let mut pieces = vec![intro_piece(config)];
    pieces.extend(repo_rules_piece(context_files));
    pieces.push(environment_piece(config, model, cwd));
    pieces.extend(append_piece(config));
    pieces.push(PromptPiece {
        title: "system",
        // 旁路指令沿用追加指令的角色规则，不新增配置片段名。
        name: PIECE_APPEND,
        scope: PieceScope::Project,
        text: "Answer only the latest side question briefly, using the supplied main-session context as reference. Do not continue the main task. No tools are available; do not call tools, access files, or claim to perform actions. Treat historical tool records as reference text, not pending calls. If the answer requires new information or an action, state that limitation. This side conversation does not update the main session.".into(),
    });
    compose(pieces, model)
}

fn intro_piece(config: &ResolvedConfig) -> PromptPiece {
    let identity = if config.system_prompt.override_text.trim().is_empty() {
        DEFAULT_INTRO
    } else {
        config.system_prompt.override_text.trim()
    };
    PromptPiece {
        title: "system",
        name: PIECE_INTRO,
        scope: PieceScope::Static,
        text: format!("{identity}\n\n{VISUAL_GUIDE}"),
    }
}

fn repo_rules_piece(context_files: &[context_files::ContextFile]) -> Option<PromptPiece> {
    if context_files.is_empty() {
        return None;
    }
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
    Some(PromptPiece {
        title: "project",
        name: PIECE_REPO_RULES,
        scope: PieceScope::Project,
        text: rules,
    })
}

fn environment_piece(config: &ResolvedConfig, model: &ResolvedModel, cwd: &Path) -> PromptPiece {
    let shell = crate::shell::detect()
        .map(crate::shell::ShellChoice::summary)
        .unwrap_or_else(|| "unavailable".to_owned());
    let limits = crate::limits::prompt_line(&config.tools.bash.limits)
        .map(|line| format!("\n{line}"))
        .unwrap_or_default();
    PromptPiece {
        title: "system",
        name: PIECE_ENVIRONMENT,
        scope: PieceScope::Project,
        text: format!(
            "<env>\ncwd: {}\nplatform: {} {}\nshell: {}{limits}\nmodel: {}\n</env>",
            cwd.display(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            shell,
            model.id,
        ),
    }
}

fn append_piece(config: &ResolvedConfig) -> Option<PromptPiece> {
    let text = config.system_prompt.append.trim();
    (!text.is_empty()).then(|| PromptPiece {
        title: "system",
        name: PIECE_APPEND,
        scope: PieceScope::Project,
        text: text.to_owned(),
    })
}

/// 当前引擎的规则提醒正文。
///
/// 引擎规则只放在运行时提醒里：静态系统提示与引擎无关，才能跨目录、跨会话复用
/// 缓存。引擎在会话生命周期内不可变，这份事实一般只注入一次（压缩清空去重表后
/// 会补回历史）；可叠加的执行模式见 [`mode_reminder`]。
pub fn engine_reminder(engine: SessionEngine) -> String {
    let text = match engine {
        SessionEngine::Standard => {
            "Standard engine is active: native tool calls run directly; the workspace is writable unless an execution mode says otherwise."
        }
        SessionEngine::Ptc => {
            "PTC engine is active: call tools only through run_code; other native calls are rejected. Child calls still obey tool enablement, mode policy, and the bash sandbox."
        }
        SessionEngine::ClmExperimental => crate::clm::SYSTEM_INSTRUCTION,
    };
    text.to_owned()
}

/// 执行模式的规则提醒正文；默认模式返回 `None`，调用方据此发 Retract 撤掉旧条目。
///
/// 模式与引擎正交：模式不写引擎规则，只写它收紧的边界与期限。ask 是单次提示
/// 的只读声明；plan 是持续到批准/退出为止的模式。
pub fn mode_reminder(mode: crate::service::mode::Mode) -> Option<String> {
    use crate::service::mode::Mode;
    let text = match mode {
        Mode::Default => return None,
        Mode::Ask => {
            "Ask mode applies to this prompt only: the workspace is read-only, including inside run_code; read and search freely. The restriction ends when this prompt ends."
        }
        Mode::Plan => {
            "Plan mode is active: the workspace is read-only, including inside run_code. Save the plan with plan(markdown); read the saved plan file for anchors and refine it with plan(edits) — after review notes, plan(edits) on the reviewed version produces the next draft. Before calling exit_plan, make sure the plan is finished: exit_plan submits the working draft, ends this prompt immediately, and puts the session into plan review — no tool call runs after it."
        }
    };
    Some(text.to_owned())
}

/// 当前沙盒模式的事实提醒正文。
///
/// 这里只写模型需要据此调整行为的边界与升级方式，不重复配置项的逐字段说明。
pub fn sandbox_contract(permission: SessionPermission, config: &ResolvedSandboxConfig) -> String {
    let filesystem = match permission {
        SessionPermission::Full => {
            "Commands run directly on the host with no filesystem isolation."
        }
        SessionPermission::Workspace => {
            "Shell commands see the host filesystem read-only except the workspace, session temporary directories, and extra paths; `$HOME` writes use a session copy-on-write layer on Linux when `overlay_home` is enabled, otherwise `$HOME` is read-only. File tools read host files except configured deny paths and can write only allowed roots."
        }
        SessionPermission::WorkspaceOnly => {
            "Shell commands see the workspace, session temporary directories, a synthetic `$HOME`, and read-only runtime system paths; the host home and other host data are absent. File tools can read only those roots and configured extras."
        }
    };
    let network = if config.network || permission == SessionPermission::Full {
        "Shell network access is enabled."
    } else {
        "Shell network access is blocked; web tools are unaffected."
    };
    let approval = if permission == SessionPermission::Full {
        ""
    } else if config.on_denied == crate::config::OnDenied::Deny {
        "Sandbox denials are final; no approval prompt is shown."
    } else {
        "Out-of-bound access asks the user; one-time and session grants are possible."
    };
    let escalation = if permission == SessionPermission::Full {
        ""
    } else if config.allow_unsandboxed {
        "After a sandbox denial, retry the exact command once with `sandbox_permissions` (workspace or full) plus `justification`; the user approves."
    } else {
        "Escalation to full is disabled; use allowed paths or change settings."
    };
    format!(
        "Sandbox mode: {}. {filesystem} {network} {approval} {escalation}",
        permission.as_str()
    )
    .trim_end()
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreConfig;
    use std::fs;

    fn resolved(cwd: &Path, home: &Path) -> ResolvedConfig {
        resolved_with_compat(cwd, home, "")
    }

    /// 生成带 compat 覆写的配置，用来测试指令角色这类逐模型开关。
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
            ptc_worker: None,
        };
        ResolvedConfig::load(&core)
            .expect("load config")
            .for_project(cwd)
            .expect("resolve project")
    }

    /// 按发送顺序取出指令消息的全部文本块，用于断言块边界。
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

    /// 按发送顺序把指令消息拼成纯文本。
    fn prompt_text(prompt: &SystemPrompt) -> String {
        prompt
            .messages
            .iter()
            .map(Message::instruction_text)
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn btw_prompt_keeps_only_passive_pieces_and_instruction_roles() {
        let directory = tempfile::tempdir().expect("临时目录");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join("AGENTS.md"), "Keep the local rule.").unwrap();
        let mut config = resolved_with_compat(&cwd, &home, "instruction_role = \"user\"");
        config.system_prompt.override_text = "Custom identity.".into();
        config.system_prompt.append = "Keep the appendix.".into();
        let model = config.resolve_role("default").unwrap();
        let files = context_files::load_context_files(&cwd, &config.home, &config.context_files);
        let prompt = build_btw(&config, &model, &cwd, &files);
        assert_eq!(prompt.messages.len(), 2);
        assert_eq!(prompt.messages[0].role, Role::System);
        assert_eq!(prompt.messages[1].role, Role::User);
        assert_eq!(prompt.static_prefix, [1, 0]);
        let system = prompt.messages[0].instruction_text();
        let user = prompt.messages[1].instruction_text();
        assert!(system.starts_with("Custom identity."));
        assert!(system.contains(VISUAL_GUIDE));
        assert!(system.contains("<env>"));
        assert!(user.contains("Keep the local rule."));
        assert!(user.contains("Keep the appendix."));
        assert!(user.contains("No tools are available"));
        let text = prompt_text(&prompt);
        for excluded in [
            TOOL_POLICY,
            "run_code works",
            "<skills>",
            "<plugins>",
            "<mcp-servers>",
        ] {
            assert!(!text.contains(excluded), "旁路不能包含 {excluded}");
        }
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
            "",
            "",
            None,
        );
        let text = prompt_text(&prompt);
        assert!(text.starts_with(DEFAULT_INTRO));
        assert!(text.contains(VISUAL_GUIDE));
        assert!(text.contains("Follow local rule."));
        assert!(text.contains(REPO_RULES_INTRO));
        assert!(text.contains("model: p/model"));
        assert!(text.contains("<mcp-servers>"));
        assert!(text.contains("structuredContent"));
        assert!(text.contains("state.store"));
        assert!(text.contains("Standard tool batches allow up to 16"));
        assert!(!text.contains("Current mode:"));
        // 默认配置下所有片段都在 system：只有一条消息，前三块是静态前缀。
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

    /// 工具政策必须说明内部 URI 该用哪些工具读，以及后台任务的结果会自动送达。
    /// 模型只能从这里知道不该用 bash 去 tail `artifact://jobs/job_1.log`。
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
            "",
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
        ] {
            assert!(text.contains(uri), "工具政策缺少 {uri}：{text}");
        }
        assert!(
            text.contains("A bare scheme") && text.contains("lists that namespace when read"),
            "必须说明读裸 scheme 会列出该命名空间：{text}"
        );
        assert!(
            text.contains("Never pass a resource URI to bash or to any other program"),
            "必须禁止把 URI 交给 shell 或任何程序：{text}"
        );
        assert!(
            text.contains("reports its completion notice")
                && text.contains("never sleep, re-run, or poll"),
            "必须说明后台任务的完成通知会自动送达，且禁止轮询：{text}"
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
            "",
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

    /// 工具声明只在调用方渲染出来时注入：紧随 ptc runtime 之后，且不进静态前缀
    /// （依赖 MCP 集合与配置，Project 作用域）。
    #[test]
    fn tool_declarations_piece_lands_after_ptc_runtime() {
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
            "<tool-declarations>\ntools.read(args: { path: string }): Promise<ToolResult>\n</tool-declarations>",
            "",
            None,
        );
        let blocks = blocks(&prompt);
        assert!(blocks[2].contains("run_code"), "ptc runtime 在声明之前");
        assert!(
            blocks[3].starts_with("<tool-declarations>"),
            "声明片段紧随 ptc runtime：{blocks:?}"
        );
        assert_eq!(
            prompt.static_prefix,
            vec![3],
            "声明是 Project 作用域，不得进静态前缀"
        );
    }

    /// 指令角色设为 user 时，规定性片段改用 user 消息，描述性片段留在 system；
    /// 片段正文不变，只有分组和顺序不同。
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
            "",
            "",
            None,
        );
        // 两条消息：system 组放描述性片段，user 组放规定性片段。
        assert_eq!(prompt.messages.len(), 2);
        assert_eq!(prompt.messages[0].role, Role::System);
        assert_eq!(prompt.messages[1].role, Role::User);
        assert_eq!(prompt.static_prefix, vec![1, 2]);
        let system = prompt.messages[0].text();
        assert!(system.starts_with(DEFAULT_INTRO));
        assert!(system.contains("<mcp-servers>"));
        assert!(system.contains("model: p/model"));
        assert!(system.contains("shell: "), "环境块必须声明实际 shell");
        assert!(!system.contains(TOOL_POLICY), "工具政策不得留在 system 组");
        let user = prompt.messages[1].text();
        assert!(user.starts_with(TOOL_POLICY));
        assert!(user.contains("structuredContent"));
        assert!(user.contains(REPO_RULES_INTRO));
        assert!(user.contains("Follow local rule."));
        assert!(!user.contains("<env>"), "环境信息不能进 user 组");

        // 与默认配置相比，片段正文相同，只有分组和顺序不同。
        let plain_config = resolved(&cwd, &home);
        let plain_model = plain_config.resolve_role("default").expect("resolve model");
        let plain = build(
            &plain_config,
            &plain_model,
            &cwd,
            &context_files,
            &crate::skills::SkillIndex::empty(),
            "<mcp-servers>\ngithub\n</mcp-servers>",
            "",
            "",
            None,
        );
        assert_eq!(plain.messages.len(), 1);
        let mut split = blocks(&prompt);
        let mut single = blocks(&plain);
        split.sort();
        single.sort();
        assert_eq!(split, single);
    }

    /// 未知片段名在加载配置时报错，不会悄悄把整条指令都放进 user。
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
            ptc_worker: None,
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
                "",
                "",
                None,
            ))
        };
        let fetch_only = prompt(&config);
        assert!(!fetch_only.contains("web_search"));
        assert!(
            fetch_only.contains("web_fetch"),
            "无密钥时仍应提示 web_fetch"
        );

        config.web_fetch.enabled = false;
        let none = prompt(&config);
        assert!(!none.contains("web_search") && !none.contains("web_fetch"));

        config.web_fetch.enabled = true;
        config.web_search.api_key = Some("tvly-test".to_owned());
        let both = prompt(&config);
        assert!(both.contains("web_search") && both.contains("web_fetch"));

        config.web_fetch.enabled = false;
        let search_only = prompt(&config);
        assert!(search_only.contains("web_search") && !search_only.contains("web_fetch"));
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
        let prompt = build(
            &config,
            &model,
            &cwd,
            &context_files,
            &skills,
            "",
            "",
            "",
            None,
        );
        let text = prompt_text(&prompt);
        assert!(text.contains("<skills>"));
        assert!(text.contains("- demo: demo skill"));
    }

    /// 每个引擎都要发引擎提醒；模式提醒单独成条目，模式退出时靠 Retract 撤掉。
    #[test]
    fn engine_reminder_declares_every_engine() {
        for (engine, needles) in [
            (
                SessionEngine::Standard,
                ["Standard engine is active", "writable"].as_slice(),
            ),
            (
                SessionEngine::Ptc,
                ["PTC engine is active", "only through run_code"].as_slice(),
            ),
            (
                SessionEngine::ClmExperimental,
                ["<clm-experimental>", "clm_context"].as_slice(),
            ),
        ] {
            let text = engine_reminder(engine);
            for needle in needles {
                assert!(text.contains(needle), "{engine:?} 缺少「{needle}」：{text}");
            }
            assert!(
                !text.contains("follow the tool policies above"),
                "{engine:?} 不能只报引擎名、让模型去上文找规则：{text}"
            );
        }
    }

    #[test]
    fn mode_reminder_covers_modes() {
        use crate::service::mode::Mode;
        assert!(mode_reminder(Mode::Default).is_none());
        let ask = mode_reminder(Mode::Ask).expect("ask 模式提醒");
        assert!(ask.contains("read-only") && ask.contains("prompt"), "{ask}");
        let plan = mode_reminder(Mode::Plan).expect("plan 模式提醒");
        for needle in ["Plan mode is active", "plan(markdown)", "exit_plan"] {
            assert!(plan.contains(needle), "plan 模式缺少「{needle}」：{plan}");
        }
    }

    #[test]
    fn sandbox_contract_states_mode_network_and_escalation() {
        let config = ResolvedSandboxConfig {
            default_mode: SessionPermission::WorkspaceOnly,
            network: false,
            on_denied: crate::config::OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home: true,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            deny_write_paths: Vec::new(),
        };
        let text = sandbox_contract(SessionPermission::WorkspaceOnly, &config);
        assert!(text.starts_with("Sandbox mode: workspace-only."), "{text}");
        assert!(text.contains("host home and other host data are absent"));
        assert!(text.contains("Shell network access is blocked"));
        assert!(text.contains("sandbox_permissions"));
        assert!(text.contains("justification"));
    }
}
