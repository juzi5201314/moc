//! 稳定系统前缀、模式约束与轮次尾部易变提醒。

pub mod context_files;
pub mod reminder;

use std::path::Path;

use moc_llm::Message;
use moc_protocol::methods::SessionMode;

use crate::config::ResolvedConfig;
use crate::config::model::ResolvedModel;

const DEFAULT_INTRO: &str = "You are a helpful software engineer assistant.";
const TOOL_POLICY: &str = "工具：bash/read/edit/write/todo/grep/find 可用；搜索代码用 grep，找文件用 find，不要用 bash 跑 grep/find；改文件前先 read（edit 需要 read 给出的行锚点）；只有需要整文件重写或新建文件时才用 write。\n改文件用 edit 的锚点：read 的输出形如 `HZ3│内容`，把锚点填进 edits 的 from/to；不确定当前内容就重新 read，不要凭记忆构造锚点。";

/// 组装稳定模型前缀；时间、job 与工作区通知不得传入此函数。
pub fn system_message(
    config: &ResolvedConfig,
    model: &ResolvedModel,
    mode: SessionMode,
    cwd: &Path,
) -> Message {
    let mut sections = Vec::new();
    let intro = if config.system_prompt.override_text.trim().is_empty() {
        DEFAULT_INTRO
    } else {
        config.system_prompt.override_text.trim()
    };
    sections.push(intro.to_owned());
    sections.push(tool_policy(mode));
    if !config.system_prompt.append.trim().is_empty() {
        sections.push(config.system_prompt.append.trim().to_owned());
    }

    let context_files = if config.context_files.enabled {
        context_files::load_context_files(cwd, &config.home, config.context_files.import_depth)
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
        sections.push(rules);
    }
    sections.push(format!(
        "<env>\ncwd: {}\nplatform: {} {}\nmodel: {}\n</env>",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        model.id,
    ));
    Message::system(sections.join("\n\n"))
}

fn tool_policy(mode: SessionMode) -> String {
    let mut policy = TOOL_POLICY.to_owned();
    if mode == SessionMode::Ask {
        policy.push_str("\n当前是问答模式：只允许读取与检索（read/grep/find/bash/todo/ask/job），严禁修改文件或系统状态；回答用户的问题即可。");
    } else if mode == SessionMode::Plan {
        policy.push_str("\n当前是计划模式：只允许读取与检索（read/grep/find/bash/todo/ask/job），严禁通过任何方式修改文件或系统状态；探索完成后调用 submit_plan 提交计划。");
    }
    policy
}

/// 把易变提醒放在消息尾部，确保 system 前缀与历史 cache key 稳定。
pub fn reminder_message(text: String) -> Message {
    Message::user_text(text)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::CoreConfig;

    fn resolved(cwd: &Path, home: &Path) -> ResolvedConfig {
        let raw = "[models.roles]\ndefault='p/model'\nsmol='p/model'\nslow='p/model'\nplan='p/model:xhigh'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid/v1'\n[providers.p.models.model]\n";
        fs::create_dir_all(home).unwrap();
        fs::write(home.join("config.toml"), raw).unwrap();
        let core = CoreConfig {
            home: home.to_owned(),
            cwd: cwd.to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
        };
        ResolvedConfig::load(&core)
            .unwrap()
            .for_project(cwd)
            .unwrap()
    }

    #[test]
    fn stable_prompt_has_rules_mode_and_model_without_volatile_time() {
        let directory = tempfile::tempdir().expect("临时目录");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(cwd.join("AGENTS.md"), "Follow local rule.\n").unwrap();
        let config = resolved(&cwd, &home);
        let model = config.resolve_role("default").unwrap();
        let normal = system_message(&config, &model, SessionMode::Normal, &cwd);
        let ask = system_message(&config, &model, SessionMode::Ask, &cwd);
        let normal = match &normal.content[0] {
            moc_llm::ContentPart::Text(text) => text,
            _ => panic!("系统提示为文本"),
        };
        let ask = match &ask.content[0] {
            moc_llm::ContentPart::Text(text) => text,
            _ => panic!("系统提示为文本"),
        };
        assert!(normal.starts_with(DEFAULT_INTRO));
        assert!(normal.contains("Follow local rule."));
        assert!(normal.contains("model: p/model"));
        assert!(!normal.contains("当前时间"));
        assert!(ask.contains("当前是问答模式"));
        assert!(!ask.contains("当前是计划模式"));
    }
    use moc_llm::Role;
    #[test]
    fn volatile_reminder_is_a_user_message() {
        let message = reminder_message("<system-reminder>job 完成</system-reminder>".into());
        assert_eq!(message.role, Role::User);
    }
}
