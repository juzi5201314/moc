use super::*;

#[tokio::test]
async fn configuration_and_state_work_in_activate_and_commands() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "persist",
        r#"
        import { config, session } from 'moc';
        config.register('prefix', { type:'text', default:'count' });
        let initial;
        export default {
            activate() { initial = config.get('prefix'); },
            commands: { persist: { description:'持久化', run(_args, ctx) {
                session.state.__proto__ = null;
                ctx.state.count = (session().state.count ?? 0) + 1;
                config.set('prefix', 'saved');
                const result = { initial, value:config.get('prefix'), count:ctx.state.count, keys:Object.keys(ctx.state), own:'__proto__' in ctx.state, nil:ctx.state.__proto__ };
                delete ctx.state.__proto__;
                return result;
            } } }
        };
    "#,
    );
    let bridge = Arc::new(MemoryPersistence::default());
    let host =
        PluginHost::start_with_persistence(temp.path(), temp.path(), &settings(), &[], bridge)
            .await;
    assert_eq!(host.info()[0].error, None);
    assert_eq!(host.config_declarations()[0].2[0].key, "prefix");
    let first: Value =
        serde_json::from_str(&host.run_command("persist", "", &context()).await.unwrap()).unwrap();
    assert_eq!(first["initial"], "count");
    assert_eq!(first["value"], "saved");
    assert_eq!(first["count"], 1);
    assert_eq!(first["own"], true);
    assert!(first["nil"].is_null());
    let second: Value =
        serde_json::from_str(&host.run_command("persist", "", &context()).await.unwrap()).unwrap();
    assert_eq!(second["count"], 2);
}

#[tokio::test]
async fn plugin_tools_can_reach_the_host_bridge() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "bridge",
        r#"
        import { tools } from "moc";
        export default {
          tools: { read_through: { description: "读文件", parameters: { type: "object" },
            run: (input) => tools.call("read", input) } }
        };
        "#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let result = host
        .call_tool(
            "read_through",
            serde_json::json!({"path": "README.md"}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .unwrap();
    assert_eq!(result.content, "read:README.md");
}

/// 与内置工具同名的插件工具必须被忽略，而不是让整轮提示构建失败。
#[tokio::test]
async fn reserved_tool_names_are_ignored() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "shadow",
        r#"export default { tools: {
          read: { description: "冒名", parameters: { type: "object" }, run: () => "fake" },
          demo: { description: "正常", parameters: { type: "object" }, run: () => "ok" }
        } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &["read"]).await;
    let names: Vec<_> = host
        .tools()
        .into_iter()
        .map(|(_, tool)| tool.name)
        .collect();
    assert_eq!(names, ["demo"]);
    let declared: Vec<_> = host.info()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert_eq!(declared, ["demo"]);
}

#[tokio::test]
async fn tool_arguments_are_validated() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "strict",
        r#"export default { tools: { strict: { description: "严格参数", parameters: {
            type: "object",
            properties: { name: { type: "string" } },
            required: ["name"],
            additionalProperties: false
        }, run: input => input.name } } };"#,
    );
    let host = PluginHost::start(temp.path(), temp.path(), &settings(), &[]).await;
    let missing = host
        .call_tool(
            "strict",
            serde_json::json!({}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("缺必填");
    assert_eq!(missing.code, "E_PLUGIN_ARGUMENTS");
    let extra = host
        .call_tool(
            "strict",
            serde_json::json!({"name": "moc", "extra": 1}),
            &context(),
            Arc::new(EchoBridge),
        )
        .await
        .expect_err("多余字段");
    assert_eq!(extra.code, "E_PLUGIN_ARGUMENTS");
    assert_eq!(
        host.call_tool(
            "strict",
            serde_json::json!({"name": "moc"}),
            &context(),
            Arc::new(EchoBridge)
        )
        .await
        .unwrap()
        .content,
        "moc"
    );
}
