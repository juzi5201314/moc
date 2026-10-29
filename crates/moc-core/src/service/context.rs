//! 上下文快照：占用估算（`session/context`）与实际内容（`session/context_content`）。
//!
//! 两个方法共用同一套组装逻辑：系统提示片段、工具定义、注入消息和可见历史里的
//! transcript 消息。占用和内容都从这里算，避免两边各拼一遍导致数字和内容对不上。

use std::path::PathBuf;
use std::sync::Arc;

use moc_llm::Message;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::methods::{
    ContextContentResult, ContextContentSection, ContextUsageResult, ContextUsageSection,
    SessionContextContentParams, SessionContextParams,
};

use super::{CoreInner, to_rpc};
use crate::compact;
use crate::session::state::SessionState;
use crate::system_prompt::{PromptPiece, SystemPrompt};

/// 上下文里的一条消息分块。
struct MessageBlock {
    /// 分块短名（1-3 个英文单词）。
    name: &'static str,
    message: Message,
    /// 估算的 token 数。transcript 消息直接用会话里逐条累计的估算值，与占用统计完全一致。
    tokens: u64,
}

/// 一次上下文快照。
struct Snapshot {
    /// 模型的上下文窗口大小。
    context_window: u64,
    /// 系统提示片段，按发送顺序排列。
    pieces: Vec<PromptPiece>,
    /// 由 `pieces` 拼成的指令消息。
    system: SystemPrompt,
    /// 当前模型可见的工具定义。
    tools: Vec<moc_llm::ToolDef>,
    /// 压缩摘要、活动计划、todo 快照等注入消息。
    injected: Vec<MessageBlock>,
    /// 可见历史里的 transcript 消息。
    transcript: Vec<MessageBlock>,
}

impl Snapshot {
    /// 注入消息与 transcript 消息的 token 总数。
    fn history_tokens(&self) -> u64 {
        self.injected
            .iter()
            .chain(&self.transcript)
            .map(|block| block.tokens)
            .sum()
    }

    /// 按分块估算占用；`session/context` 和正文窗的合计都用它。
    fn usage(&self) -> ContextUsageResult {
        // 各片段的 token 必须在拼接前算好，拼接后片段就不再单独存在了。
        let directory_tokens = self
            .pieces
            .iter()
            .filter(|piece| piece.title == "tools")
            .map(|piece| compact::estimate_tokens(&piece.text))
            .sum::<u64>();
        let mut project_tokens = 0u64;
        let mut skills_tokens = 0u64;
        for piece in &self.pieces {
            match piece.title {
                "project" => {
                    project_tokens =
                        project_tokens.saturating_add(compact::estimate_tokens(&piece.text));
                }
                "skills" => {
                    skills_tokens =
                        skills_tokens.saturating_add(compact::estimate_tokens(&piece.text));
                }
                _ => {}
            }
        }
        let tools = &self.tools;
        let history_tokens = self.history_tokens();
        let system_tokens = compact::messages_tokens(&self.system.messages);
        let tool_tokens = compact::tools_tokens(tools).saturating_add(directory_tokens);
        let total_tokens = system_tokens
            .saturating_add(compact::tools_tokens(tools))
            .saturating_add(history_tokens);
        let system_base_tokens = system_tokens
            .saturating_sub(project_tokens)
            .saturating_sub(skills_tokens)
            .saturating_sub(directory_tokens);

        let mut sections = Vec::with_capacity(5);
        sections.push(ContextUsageSection {
            name: "system".into(),
            tokens: system_base_tokens,
        });
        if project_tokens > 0 {
            sections.push(ContextUsageSection {
                name: "project".into(),
                tokens: project_tokens,
            });
        }
        if skills_tokens > 0 {
            sections.push(ContextUsageSection {
                name: "skills".into(),
                tokens: skills_tokens,
            });
        }
        sections.push(ContextUsageSection {
            name: "tools".into(),
            tokens: tool_tokens,
        });
        sections.push(ContextUsageSection {
            name: "messages".into(),
            tokens: history_tokens,
        });
        ContextUsageResult {
            context_window: self.context_window,
            total_tokens,
            sections,
        }
    }
}

/// `session/context`：按分块估算 token。
pub(crate) async fn usage(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: SessionContextParams,
) -> Result<ContextUsageResult, RpcError> {
    Ok(snapshot(inner, &ctx, &params.session_id).await?.usage())
}

/// `session/context_content`：返回上下文里的实际内容。
pub(crate) async fn content(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: SessionContextContentParams,
) -> Result<ContextContentResult, RpcError> {
    let snapshot = snapshot(inner, &ctx, &params.session_id).await?;
    let usage = snapshot.usage();
    let budget = params.max_section_bytes.map(|bytes| bytes as usize);
    let mut sections = Vec::with_capacity(
        snapshot.pieces.len()
            + snapshot.tools.len()
            + snapshot.injected.len()
            + snapshot.transcript.len(),
    );
    for piece in &snapshot.pieces {
        sections.push(content_section(
            piece.name,
            piece.text.clone(),
            compact::estimate_tokens(&piece.text),
            budget,
        ));
    }
    for tool in &snapshot.tools {
        let text = tool_text(tool);
        let tokens = compact::tools_tokens(std::slice::from_ref(tool));
        sections.push(content_section(&tool.name, text, tokens, budget));
    }
    for block in snapshot.injected.iter().chain(&snapshot.transcript) {
        sections.push(content_section(
            block.name,
            message_text(&block.message),
            block.tokens,
            budget,
        ));
    }
    Ok(ContextContentResult {
        context_window: usage.context_window,
        // 合计沿用占用统计的结果：正文窗标题栏、顶部进度条和占用窗必须显示同一个数。
        total_tokens: usage.total_tokens,
        sections,
    })
}

/// 一个正文分块；超出 `budget` 时按字符边界截断并记录丢弃的字节数。
fn content_section(
    name: &str,
    mut text: String,
    tokens: u64,
    budget: Option<usize>,
) -> ContextContentSection {
    let mut truncated_bytes = 0u64;
    if let Some(budget) = budget
        && text.len() > budget
    {
        let mut end = budget;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        truncated_bytes = (text.len() - end) as u64;
        text.truncate(end);
        text.push('…');
    }
    ContextContentSection {
        name: name.to_owned(),
        text,
        tokens,
        truncated_bytes,
    }
}

/// 工具定义的可读 JSON：与线上发送的字段一一对应。
fn tool_text(tool: &moc_llm::ToolDef) -> String {
    let mut value = serde_json::Map::new();
    value.insert("name".into(), tool.name.clone().into());
    if let Some(description) = &tool.description {
        value.insert("description".into(), description.clone().into());
    }
    value.insert("parameters".into(), tool.parameters.clone());
    if let Some(strict) = tool.strict {
        value.insert("strict".into(), strict.into());
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(value))
        .expect("工具定义只含可序列化的 JSON 值")
}

/// 一条消息的纯文本形态：内容块各占一段，图片与工具调用都显式标注。
fn message_text(message: &Message) -> String {
    let mut text = String::new();
    for part in &message.content {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        match part {
            moc_llm::ContentPart::Text(body) => text.push_str(body),
            moc_llm::ContentPart::Image { source, .. } => {
                text.push_str(&image_label(source));
            }
            moc_llm::ContentPart::Reasoning(reasoning) => {
                text.push_str("<reasoning>\n");
                text.push_str(reasoning.display_text().as_deref().unwrap_or("(不透明)"));
                text.push_str("\n</reasoning>");
            }
            moc_llm::ContentPart::ToolCall(call) => {
                text.push_str(&format!("<tool-call name=\"{}\">\n", call.name));
                text.push_str(call.arguments.trim());
                text.push_str("\n</tool-call>");
            }
            moc_llm::ContentPart::ToolResult(result) => {
                text.push_str(&format!(
                    "<tool-result id=\"{}\" error=\"{}\">\n",
                    result.tool_call_id, result.is_error
                ));
                text.push_str(&result.content);
                for source in &result.images {
                    text.push('\n');
                    text.push_str(&image_label(source));
                }
                text.push_str("\n</tool-result>");
            }
        }
    }
    text
}

/// 图片的占位标注：不把 base64 载荷摊进正文窗。
fn image_label(source: &moc_llm::ImageSource) -> String {
    match source {
        moc_llm::ImageSource::Url(url) => format!("<image url=\"{url}\">"),
        // base64 每 4 个字符还原 3 字节，正文窗只报还原后的体量。
        moc_llm::ImageSource::Base64 { media_type, data } => format!(
            "<image type=\"{media_type}\" bytes=\"{}\">",
            data.len() / 4 * 3
        ),
    }
}

/// 组装一次上下文快照。
///
/// 异步读取（配置、技能、上下文文件、MCP 摘要、子代理提示）只在会话状态锁外
/// 进行；正文渲染全部在锁内完成，不复制整份 [`SessionState`]。
async fn snapshot(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session_id: &str,
) -> Result<Snapshot, RpcError> {
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let (cwd, model_id, engine) = {
        let state = record.state.lock().await;
        (
            PathBuf::from(&state.session.cwd),
            state.model.clone(),
            state.engine,
        )
    };
    let config = inner.project_config(&cwd).map_err(to_rpc)?;
    let model = config.resolve_model(&model_id).map_err(to_rpc)?;
    let skills = inner.project_skills(&cwd).map_err(to_rpc)?;
    let context_files = inner.project_context_files(session_id, &cwd).await;
    let mcp_summary = inner.mcp_summary(&config).await.map_err(to_rpc)?;
    let subagent = crate::agents::subagent_prompt(inner, session_id).await;
    // 插件宿主首次加载可能耗到 load_timeout，必须在拿会话锁之前完成。
    let plugins = inner.project_plugins(&cwd).await.map_err(to_rpc)?;
    let plugin_tools = crate::service::plugin_tools_of(&plugins);
    let mut plugin_context = {
        let state = record.state.lock().await;
        crate::service::plugin_context(&state, &model.selector)
    };
    plugin_context.writable = false;
    // 预览/上下文构建不能持有会话锁等待插件，也不能产生持久化副作用。
    let plugin_prompt = plugins.prompt_text(&plugin_context).await;
    // 快照与声明都要等待 MCP/插件，必须在拿会话锁之前完成。
    let tool_declarations = inner
        .tool_declarations(
            engine,
            &config,
            inner.ask_available(
                ctx.connection_id(),
                record.state.lock().await.session.autonomy,
            ),
        )
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await;
    let pieces = crate::system_prompt::pieces(
        &config,
        &model,
        &cwd,
        &context_files,
        &skills,
        &mcp_summary,
        &tool_declarations,
        &plugin_prompt,
        subagent.as_ref().map(|prompt| prompt.block.as_str()),
    );
    let system = crate::system_prompt::compose(pieces.clone(), &model);
    let tools = inner.tools.definitions(
        state.engine,
        inner.ask_available(ctx.connection_id(), state.session.autonomy),
        &config,
        crate::tools::ToolScope::for_session(&state, config.agents.max_depth),
        &plugin_tools,
    );
    let (injected, transcript) = messages(&state, &model);
    Ok(Snapshot {
        context_window: model.context_window,
        pieces,
        system,
        tools,
        injected,
        transcript,
    })
}

/// 上下文里的消息分块：注入消息与可见历史。
fn messages(
    state: &SessionState,
    model: &crate::ResolvedModel,
) -> (Vec<MessageBlock>, Vec<MessageBlock>) {
    let injected = crate::compact::replay::synthetic_messages(state, None)
        .into_iter()
        .filter_map(|message| {
            let name = crate::compact::replay::injected_name(&message)?;
            let tokens = compact::message_tokens(&message);
            Some(MessageBlock {
                name,
                message,
                tokens,
            })
        })
        .collect();
    let start = crate::compact::replay::visible_history_start(state);
    debug_assert_eq!(
        state.transcript.len(),
        state.transcript_token_counts.len(),
        "转写条目与逐条 token 估算必须一一对应"
    );
    let transcript = state.transcript[start..]
        .iter()
        .enumerate()
        .filter_map(|(offset, entry)| {
            let message = entry.context_message()?;
            Some(MessageBlock {
                name: message_name(entry),
                message: compact::replay::repair_tool_history(message, Some(model)),
                tokens: state.transcript_token_counts[start + offset],
            })
        })
        .collect();
    (injected, transcript)
}

/// 转写消息的正文分块短名：按来源区分用户、模型、工具、命令与代理消息。
fn message_name(entry: &moc_protocol::methods::TranscriptEntry) -> &'static str {
    use moc_protocol::Role;
    if entry.agent.is_some() {
        return "agent";
    }
    if entry.bash.is_some() {
        return "bash";
    }
    match entry.message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool result",
        Role::Custom => "reminder",
        Role::System | Role::Developer => "system",
    }
}
