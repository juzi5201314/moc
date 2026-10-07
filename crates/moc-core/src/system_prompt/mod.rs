//! 稳定系统前缀、模式约束与提示尾部易变提醒。

pub mod context_files;
pub mod reminder;

use std::path::Path;

use moc_llm::Message;
use moc_protocol::methods::SessionMode;

use crate::config::ResolvedConfig;
use crate::config::model::ResolvedModel;

const DEFAULT_INTRO: &str = "You are a helpful software engineer assistant.";
const TOOL_POLICY: &str = "工具：bash/read/edit/write/todo/grep/find 可用；搜索代码用 grep，找文件用 find，不要用 bash 跑 grep/find；改文件前先 read（edit 需要 read 给出的行锚点）；只有需要整文件重写或新建文件时才用 write。\n改文件用 edit 的锚点：read 的输出形如 `HZ3│内容`，把锚点填进 edits 的 from/to；不确定当前内容就重新 read，不要凭记忆构造锚点。";

pub struct PromptPiece {
    /// 分块所属分类（`system`、`project`、`skills`），用于上下文占用聚合。
    pub title: &'static str,
    pub text: String,
}

pub fn build(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    mode: SessionMode,
    cwd: &Path,
    skills: &crate::skills::SkillIndex,
    mcp_summary: &str,
) -> Message {
    compose(&pieces(config, model, mode, cwd, skills, mcp_summary))
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

/// 组装稳定模型前缀；时间、job 与工作区通知不得传入此函数。
pub fn pieces(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    mode: SessionMode,
    cwd: &Path,
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
        text: tool_policy(mode, config.tools.ptc.max_concurrent_calls),
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

    let context_files = if config.context_files.enabled {
        context_files::load_context_files(cwd, &config.home, &config.context_files)
    } else {
        Vec::new()
    };
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
    // skill 目录属于稳定前缀（会话内索引不变），保持提示 cache 命中。
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

fn tool_policy(mode: SessionMode, concurrency: usize) -> String {
    let mut policy = TOOL_POLICY.to_owned();
    if mode == SessionMode::Ask {
        policy.push_str("\n当前是问答模式：允许读取与检索，禁止修改工作区；run_code 子工具继承本次提示权限。state/todo 只写会话数据。");
    } else if mode == SessionMode::Plan {
        policy.push_str("\n当前是计划模式：允许读取与检索，禁止修改工作区；探索完成后调用 submit_plan 提交计划并结束本轮，run_code 子工具继承本次提示权限。state/todo/计划存储只写会话数据。");
    } else if mode == SessionMode::Ptc {
        policy.push_str("\n当前是 PTC 模式：通过 run_code 编写 JavaScript 并调用列出的工具；仅 run_code 可直接调用，子工具仍受各自启用状态、当前模式与 bash 沙盒策略控制。");
    }
    policy.push_str(&format!("\n所有模式支持 run_code；PTC 仅此原生入口，其他模式内置工具可直接使用。MCP 仅在代码内经 catalog 发现后调用，不提供 MCP 原生入口。先 await catalog.search({{query:\"\"}}) 获取分页摘要，再 await catalog.describe(name) 获取原始 schema，最后 await tools[name](args)。发现不授予权限，子工具按当前模式与 bash 沙盒策略执行。\nrun_code 的工具结果始终为 {{content,summary,structuredContent,isError,status,artifact}}，优先读取 structuredContent；text-only 的 JSON 可显式 JSON.parse(r.content)。失败 reject Error，含可序列化 code/message/result，可用 try/catch 或 Promise.allSettled。内置例子：const r = await tools.read({{path:\"README.md\"}}); return r.content;\n独立只读任务可 Promise.all，实际并发上限 {concurrency}；写操作和 bash 审批是屏障，返回前必须等待所有调用。只返回最终需要的信息，中间结果不进入模型历史。state.store(key,json)、state.load(key)、state.remove(key)、state.keys() 保存会话 JSON；缺失返回 undefined，写入立即提交，后续失败不撤销；普通 JS 全局变量不会跨次保留。WASM 无文件/网络/进程/定时器 API，必须通过宿主工具。"));
    policy
}

/// 把易变提醒放在消息尾部，确保 system 前缀与历史 cache key 稳定。
pub fn reminder_message(text: String) -> Message {
    Message::user_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreConfig;
    use std::fs;

    fn resolved(cwd: &Path, home: &Path) -> ResolvedConfig {
        let raw = "[models.roles]\ndefault='p/model'\nsmol='p/model'\nslow='p/model'\nplan='p/model:xhigh'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model]\n";
        fs::create_dir_all(home).expect("创建 home");
        fs::write(home.join("config.toml"), raw).expect("写入配置");
        let core = CoreConfig {
            home: home.to_owned(),
            cwd: cwd.to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
        };
        ResolvedConfig::load(&core)
            .expect("加载配置")
            .for_project(cwd)
            .expect("解析项目配置")
    }

    #[test]
    fn stable_prompt_has_rules_mode_model_and_code_contract() {
        let directory = tempfile::tempdir().expect("临时目录");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).expect("创建 repo");
        fs::write(cwd.join("AGENTS.md"), "Follow local rule.\n").expect("写规则");
        let config = resolved(&cwd, &home);
        let model = config.resolve_role("default").expect("解析模型");
        let skills = crate::skills::SkillIndex::empty();
        let message = build(
            &config,
            &model,
            SessionMode::Normal,
            &cwd,
            &skills,
            "<mcp-servers>\ngithub connected 1\n</mcp-servers>",
        );
        let text = message.text();
        assert!(text.starts_with(DEFAULT_INTRO));
        assert!(text.contains("Follow local rule."));
        assert!(text.contains("model: p/model"));
        assert!(text.contains("<mcp-servers>"));
        assert!(text.contains("structuredContent"));
        assert!(text.contains("state.store"));
        assert!(!text.contains("当前时间"));
    }

    #[test]
    fn skill_catalog_is_included_when_present() {
        let directory = tempfile::tempdir().expect("临时目录");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(cwd.join(".agents/skills/demo")).expect("创建 skill");
        fs::write(
            cwd.join(".agents/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: 演示 skill\n---\n正文",
        )
        .expect("写 skill");
        let config = resolved(&cwd, &home);
        let model = config.resolve_role("default").expect("解析模型");
        let skills =
            crate::skills::discovery::discover_index(&cwd, &home, Some(&home), &config.skills);
        let message = build(&config, &model, SessionMode::Normal, &cwd, &skills, "");
        assert!(message.text().contains("<skills>"));
        assert!(message.text().contains("- demo: 演示 skill"));
    }
}
