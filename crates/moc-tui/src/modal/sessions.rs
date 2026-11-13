use super::SessionScope;
use moc_protocol::methods::SessionInfo;

pub fn filtered_sessions<'a>(
    sessions: &'a [SessionInfo],
    query: &str,
    scope: SessionScope,
    cwd: &str,
) -> Vec<(usize, &'a SessionInfo)> {
    let query = query.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| scope == SessionScope::All || session.cwd == cwd)
        .filter(|(_, session)| {
            query.is_empty()
                || session.title.to_lowercase().contains(&query)
                || session.cwd.to_lowercase().contains(&query)
        })
        .collect()
}

/// 分叉会话的来源标注：优先用来源会话的标题，来源已从列表消失时退回短 id。
pub fn fork_source_label<'a>(
    sessions: &'a [SessionInfo],
    session: &'a SessionInfo,
) -> Option<&'a str> {
    let source = session.forked_from.as_deref()?;
    Some(
        sessions
            .iter()
            .find(|candidate| candidate.id == source)
            .map_or_else(
                || short_session_id(source),
                |candidate| candidate.title.as_str(),
            ),
    )
}

/// 会话 id 的展示短形式；短于约定长度时原样返回。
pub fn short_session_id(id: &str) -> &str {
    id.get(..16).unwrap_or(id)
}
