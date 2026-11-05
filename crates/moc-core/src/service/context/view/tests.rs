use super::*;
use serde_json::json;

fn view(body: Value) -> View {
    View::new(
        ContextUsageResult {
            context_window: 100_000,
            total_tokens: 100,
            sections: vec![],
        },
        "test".into(),
        "openai_chat".into(),
        Ok(moc_llm::RequestInspection {
            body,
            sources: vec![],
        }),
        &[],
        &[],
    )
}

#[test]
fn directory_is_bounded_and_text_pages_round_trip_without_mutation() {
    let original = "中文🙂\n".repeat(20_000);
    let mut result = view(
        json!({"messages":(0..1000).map(|index|json!({"role":"user","content":if index==0 {original.as_str()} else {"短消息"}})).collect::<Vec<_>>() }),
    );
    let page = result.page(None, None).unwrap();
    assert_eq!(page.sections.len(), 128);
    assert_eq!(page.next_cursor, Some(128));
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("中文"));
    result.bound_text(0).unwrap();
    assert!(result.spool.is_some());
    assert!(result.nodes.iter().all(|node| node.text.is_none()));
    assert_eq!(serde_json::to_string(&result.page(None, None).unwrap()).unwrap(), encoded);
    let id = result
        .nodes
        .iter()
        .find(|node| node.meta.path == "/messages/0/content")
        .unwrap()
        .meta
        .id
        .clone();
    let mut params = SessionContextReadParams {
        node_id: id,
        limit: Some(1024),
        ..Default::default()
    };
    let mut text = String::new();
    loop {
        let read = result.read(&params).unwrap();
        text.push_str(read.text.as_ref().unwrap());
        assert!(read.text.as_ref().unwrap().len() <= 1024);
        params.cursor = read.next_cursor;
        if params.cursor.is_none() {
            break;
        }
    }
    assert_eq!(text, original);
    params.cursor = Some(1);
    assert!(result.read(&params).is_err());
    let id = result.id.clone();
    result.touched = Instant::now() - TTL - Duration::from_secs(1);
    let mut views = Views::default();
    views.entries.insert((1, "session".into()), result);
    assert!(views.get(1, "session", &id).is_err());
    assert!(views.entries.is_empty());
}

#[test]
fn role_messages_and_source_children_keep_wire_order() {
    let original = "默认🙂\n\n工具规则\n\n环境";
    let spans = [
        (0, "默认🙂".len()),
        ("默认🙂\n\n".len(), "默认🙂\n\n工具规则".len()),
        ("默认🙂\n\n工具规则\n\n".len(), original.len()),
    ];
    let inspection = moc_llm::RequestInspection {
        body: json!({"messages":[{"role":"system","content":original},{"role":"user","content":"一"},{"role":"user","content":"二"},{"role":"assistant","content":"三"},{"role":"user","content":"四"}]}),
        sources: spans
            .into_iter()
            .enumerate()
            .map(|(part, (start, end))| moc_llm::RequestSource {
                message_index: 0,
                part_index: part,
                pointer: "/messages/0/content".into(),
                text_range: Some(start..end),
            })
            .collect(),
    };
    let view = View::new(
        ContextUsageResult {
            context_window: 1000,
            total_tokens: 100,
            sections: vec![],
        },
        "test".into(),
        "openai_chat".into(),
        Ok(inspection),
        &["系统提示".into()],
        &[vec![
            "系统提示 · intro".into(),
            "系统提示 · tool policy".into(),
            "系统提示 · environment".into(),
        ]],
    );
    let page = view.page(None, None).unwrap();
    assert_eq!(
        page.sections
            .iter()
            .map(|node| node.role.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("system"),
            Some("user"),
            Some("user"),
            Some("assistant"),
            Some("user")
        ]
    );
    assert_eq!(
        page.sections
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        vec!["#1", "#2", "#3", "#4", "#5"]
    );
    let parent = view
        .nodes
        .iter()
        .find(|node| node.meta.path == "/messages/0/content" && node.meta.text_range.is_none())
        .unwrap();
    assert!(parent.text.is_none());
    assert_eq!(
        parent
            .children
            .iter()
            .map(|id| view.nodes[*id].meta.name.as_str())
            .collect::<Vec<_>>(),
        vec!["intro", "tool policy", "environment"]
    );
    let mut joined = String::new();
    for child in &parent.children {
        let node = &view.nodes[*child];
        assert_eq!(node.meta.path, "/messages/0/content");
        let page = view
            .read(&SessionContextReadParams {
                node_id: node.meta.id.clone(),
                ..Default::default()
            })
            .unwrap();
        joined.push_str(page.text.as_ref().unwrap());
    }
    assert_eq!(joined, original);
    assert_eq!(parent.meta.bytes, original.len() as u64);
    assert!(Arc::ptr_eq(
        &view.nodes[parent.children[0]].text.as_ref().unwrap().text,
        &view.nodes[parent.children[1]].text.as_ref().unwrap().text
    ));
}

#[test]
fn views_are_isolated_by_connection_and_session() {
    let mut views = Views::default();
    let mut first = view(json!({"messages":[]}));
    first.memory_permit = Some(Arc::clone(&views.text_budget).try_acquire_many_owned(16).unwrap());
    assert_eq!(views.text_budget.available_permits(), TEXT_CACHE_BYTES - 16);
    let id = first.id.clone();
    views.entries.insert((1, "s1".into()), first);
    assert!(views.get(2, "s1", &id).is_err());
    assert!(views.get(1, "s2", &id).is_err());
    assert!(views.get(1, "s1", &id).is_ok());
    views.remove_session("s1");
    assert_eq!(views.text_budget.available_permits(), TEXT_CACHE_BYTES);
    assert!(views.get(1, "s1", &id).is_err());
}
