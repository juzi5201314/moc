use super::*;

fn view() -> View {
    View::new(
        ContextUsageResult {
            context_window: 100_000,
            total_tokens: 100,
            sections: vec![ContextUsageSection {
                name: "messages".into(),
                tokens: 100,
            }],
        },
        "test".into(),
    )
}

#[test]
fn directory_is_bounded_and_visible_text_pages_round_trip() {
    let original = "中文🙂\n".repeat(20_000);
    let mut view = view();
    for index in 0..1000 {
        view.add(
            "messages",
            ContextContentSection {
                name: index.to_string(),
                ..Default::default()
            },
            Some(if index == 0 {
                original.clone()
            } else {
                "短消息".into()
            }),
        );
    }
    let directory = SessionContextReadParams {
        node_id: "0".into(),
        ..Default::default()
    };
    let page = view.read(&directory).unwrap();
    assert_eq!(page.children.len(), 128);
    assert_eq!(page.next_cursor, Some(128));
    assert!(page.text.is_none());
    view.bound_text(0).unwrap();
    assert!(view.nodes.iter().all(|node| node.text.is_none()));
    let mut params = SessionContextReadParams {
        node_id: "1".into(),
        limit: Some(1024),
        ..Default::default()
    };
    let mut joined = String::new();
    loop {
        let page = view.read(&params).unwrap();
        let text = page.text.unwrap();
        assert!(text.len() <= 1024);
        joined.push_str(&text);
        params.cursor = page.next_cursor;
        if params.cursor.is_none() {
            break;
        }
    }
    assert_eq!(joined, original);
    params.cursor = Some(1);
    assert!(view.read(&params).is_err());
}

#[test]
fn hidden_entries_cannot_be_read_even_with_forged_cursor() {
    let mut view = view();
    view.add(
        "messages",
        ContextContentSection {
            hidden: Some("工具输出已隐藏".into()),
            tokens: 100,
            ..Default::default()
        },
        None,
    );
    for cursor in [None, Some(0), Some(1), Some(u64::MAX)] {
        assert!(
            view.read(&SessionContextReadParams {
                node_id: "1".into(),
                cursor,
                ..Default::default()
            })
            .is_err()
        );
    }
    let parent = view
        .read(&SessionContextReadParams {
            node_id: "0".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(parent.text.is_none());
    assert_eq!(parent.children[0].tokens, 100);
    assert_eq!(view.text_bytes(), 0);
}

#[test]
fn views_are_isolated_and_expire() {
    let mut views = Views::default();
    let mut view = view();
    view.memory_permit = Some(
        Arc::clone(&views.text_budget)
            .try_acquire_many_owned(16)
            .unwrap(),
    );
    let id = view.id.clone();
    views.entries.insert((1, "s1".into()), view);
    assert!(views.get(2, "s1", &id).is_err());
    assert!(views.get(1, "s2", &id).is_err());
    assert!(views.get(1, "s1", &id).is_ok());
    views.entries.get_mut(&(1, "s1".into())).unwrap().touched =
        Instant::now() - TTL - Duration::from_secs(1);
    assert!(views.get(1, "s1", &id).is_err());
    assert_eq!(views.text_budget.available_permits(), TEXT_CACHE_BYTES);
}
