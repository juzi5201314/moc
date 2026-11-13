use crate::service::CoreInner;
use crate::service::to_rpc;
use moc_llm::Message;
use moc_protocol::RpcError;

/// 解析用户输入里的 `/skill:<name> 参数` 显式调用。
///
/// skill 正文以 user 消息注入（用户指令的优先级高于工具结果）。设置了
/// `disable-model-invocation` 的 skill 只能通过这条路径使用。输入不以 `/skill:` 开头时原样返回。
pub(super) async fn apply_skill_invocation(
    inner: &CoreInner,
    message: Message,
    cwd: &str,
    workflows_active: bool,
) -> Result<Message, RpcError> {
    let text = message.text();
    let Some(rest) = text.strip_prefix("/skill:") else {
        return Ok(message);
    };
    let (name, arguments) = match rest.split_once([' ', '\n']) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest.trim_end(), ""),
    };
    if name.is_empty() {
        return Ok(message);
    }
    // 会话级索引：未激活会话对 `workflow-authoring` 一律不可见（/skill: 也不例外）。
    let skills = inner
        .session_skills(std::path::Path::new(cwd), workflows_active)
        .map_err(to_rpc)?;
    let mut loaded = std::collections::HashSet::new();
    let content = crate::skills::tool::render_for_invocation(&skills, name, &mut loaded)
        .map_err(crate::service::invalid_params)?;
    let mut injected = format!(
        "<system-reminder>The user injected this via /skill:{name}; follow it now.</system-reminder>\n{content}"
    );
    if !arguments.is_empty() {
        injected.push_str(&format!("\n\nUser instructions: {arguments}"));
    }
    Ok(Message::user_text(injected))
}
