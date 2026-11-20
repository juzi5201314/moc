use super::*;

#[test]
fn version_negotiation_compares_major_only() {
    assert!(version_compatible("1.0"));
    assert!(version_compatible("1.7"));
    assert!(version_compatible("1"));
    assert!(!version_compatible("2.0"));
    assert!(!version_compatible("0.9"));
    assert!(!version_compatible(""));
    assert_eq!(major_version("1.2.3"), "1");
    assert_eq!(major_version("1"), "1");
}

#[test]
fn prompt_cleanup_preserves_detached_successors() {
    let prompts = Arc::new(Mutex::new(HashMap::new()));
    let cancel = Arc::new(CancellationToken::new());
    let session_id = "ses_1".to_owned();
    prompts
        .lock()
        .unwrap()
        .insert(session_id.clone(), Arc::clone(&cancel));
    let rpc_guard = PromptGuard {
        prompts: Arc::clone(&prompts),
        session_id: session_id.clone(),
        cancel,
        skills: false,
    };
    let (connection, _outbound) = Connection::new(
        1,
        ServerOptions::default().connection_options(),
        CancellationToken::new(),
    );
    let ctx = RequestContext::new(
        1,
        ClientInfo {
            name: "取消登记测试".into(),
            version: "1".into(),
            frontend: "test".into(),
        },
        Arc::clone(connection.traffic()),
        CancellationToken::new(),
        Arc::clone(&prompts),
    );
    let first_cancel = Arc::new(CancellationToken::new());
    // 模拟 RPC 提示已释放提示锁，后台提示持锁接力；RPC 的登记守卫尚未销毁。
    let first = ctx.claim_detached_prompt(session_id.clone(), Arc::clone(&first_cancel));
    drop(rpc_guard);
    assert!(Arc::ptr_eq(
        prompts.lock().unwrap().get(&session_id).unwrap(),
        &first_cancel,
    ));
    let next_cancel = Arc::new(CancellationToken::new());
    let next_ctx = ctx.with_cancel((*next_cancel).clone());
    // 模拟上一轮已释放提示锁，下一轮持锁登记；上一轮登记守卫保留到函数结束。
    let next = next_ctx.claim_detached_prompt(session_id.clone(), Arc::clone(&next_cancel));
    drop(first);
    // 模拟定向取消：旧守卫退出后仍能找到新任务，且令牌与上下文一致。
    prompts.lock().unwrap().get(&session_id).unwrap().cancel();
    assert!(next_ctx.cancel().is_cancelled());
    assert!(!first_cancel.is_cancelled());
    drop(next);
    assert!(prompts.lock().unwrap().is_empty());
}
