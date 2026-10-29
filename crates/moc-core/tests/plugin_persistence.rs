//! 插件持久化的协议级验收：配置来源、会话隔离、恢复和错误禁用。

mod support;

use moc_protocol::methods::{
    ConfigAction, ConfigManage, ConfigReply, ConfigScope, PluginList, PluginListParams,
    PluginRunCommand, PluginRunCommandParams, SessionEngine, SessionUnload, SessionUnloadParams,
};
use serde_json::json;
use support::Harness;

fn install(harness: &Harness, name: &str, global: bool, body: &str) {
    let root = if global {
        "home/plugins"
    } else {
        "work/.moc/plugins"
    };
    let dir = harness.root.path().join(root).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.json"), json!({"name": name}).to_string()).unwrap();
    std::fs::write(dir.join("index.js"), body).unwrap();
}

async fn command(harness: &Harness, session: &str, name: &str, args: &str) -> String {
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        harness
            .client
            .call::<PluginRunCommand>(PluginRunCommandParams {
                session_id: session.into(),
                name: name.into(),
                args: args.into(),
            }),
    )
    .await
    .expect("插件持久化不能死锁")
    .unwrap()
    .text
}

const COUNTER: &str = r#"
import { config, session } from 'moc';
config.register('prefix', { type: 'text', label: '计数前缀', description: '测试前缀', default: 'count' });
export default {
  commands: {
    counter: { description: '持久化计数', run(args) {
      const s = session();
      if (args === 'delete') { delete s.state.count; return String(s.state.count); }
      if (args === 'read') return `${config.get('prefix')}:${s.state.count ?? 0}`;
      if (args.startsWith('prefix ')) { config.set('prefix', args.slice(7)); return config.get('prefix'); }
      s.state.count = (s.state.count ?? 0) + 1;
      return `${config.get('prefix')}:${s.state.count}:${Object.keys(s.state).join(',')}`;
    }}
  }
};
"#;

#[tokio::test]
async fn state_survives_unload_and_is_scoped_to_session_and_plugin() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(&h, "counter", false, COUNTER);
    install(
        &h,
        "other",
        false,
        r#"
      import { session } from 'moc';
      export default {commands:{other:{description:'独立命名空间',run(){return String(session().state.count)}}}};
    "#,
    );
    let first = h.session(SessionEngine::Standard).await;
    let second = h.session(SessionEngine::Standard).await;
    assert_eq!(command(&h, &first, "counter", "").await, "count:1:count");
    assert_eq!(command(&h, &first, "counter", "").await, "count:2:count");
    assert_eq!(command(&h, &second, "counter", "read").await, "count:0");
    assert_eq!(command(&h, &first, "other", "").await, "undefined");
    h.client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: first.clone(),
        })
        .await
        .unwrap();
    h.load(&first).await;
    assert_eq!(command(&h, &first, "counter", "read").await, "count:2");
    assert_eq!(command(&h, &first, "counter", "delete").await, "undefined");
    assert_eq!(command(&h, &first, "counter", "read").await, "count:0");
    h.close().await;
}

#[tokio::test]
async fn config_registration_appears_in_settings_and_writes_installation_scope() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(&h, "counter", false, COUNTER);
    let session = h.session(SessionEngine::Standard).await;
    let cwd = h.root.path().join("work").to_string_lossy().into_owned();
    let ConfigReply::Settings(view) = h
        .client
        .call::<ConfigManage>(ConfigAction::Read {
            scope: ConfigScope::Project,
            cwd: cwd.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("设置文档")
    };
    let field = view
        .fields
        .iter()
        .find(|f| f.path == ["plugins", "counter", "config", "prefix"])
        .expect("未写入的默认项也要显示");
    assert_eq!(field.label, "计数前缀");
    assert_eq!(
        view.effective["plugins"]["counter"]["config"]["prefix"],
        "count"
    );
    assert!(
        view.document.get("plugins").is_none(),
        "显示默认值不能固化到文件"
    );
    assert_eq!(
        command(&h, &session, "counter", "prefix changed").await,
        "changed"
    );
    let saved: toml::Value = toml::from_str(
        &std::fs::read_to_string(h.root.path().join("work/.moc/config.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved["plugins"]["counter"]["config"]["prefix"].as_str(),
        Some("changed")
    );
    assert!(
        !std::fs::read_to_string(h.root.path().join("home/config.toml"))
            .unwrap()
            .contains("changed")
    );
    h.close().await;
}

#[tokio::test]
async fn settings_reject_invalid_value_without_disabling_plugin() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(&h, "counter", false, COUNTER);
    let session = h.session(SessionEngine::Standard).await;
    let cwd = h.root.path().join("work").to_string_lossy().into_owned();
    let ConfigReply::Settings(view) = h
        .client
        .call::<ConfigManage>(ConfigAction::Read {
            scope: ConfigScope::Project,
            cwd: cwd.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("设置文档")
    };
    let mut document = view.document;
    document["plugins"] = json!({"counter":{"config":{"prefix":42}}});
    assert!(
        h.client
            .call::<ConfigManage>(ConfigAction::Save {
                scope: ConfigScope::Project,
                cwd,
                document,
                revision: view.revision,
            })
            .await
            .is_err()
    );
    assert_eq!(command(&h, &session, "counter", "read").await, "count:0");
    let plugins = h
        .client
        .call::<PluginList>(PluginListParams {
            session_id: session,
        })
        .await
        .unwrap()
        .plugins;
    assert!(plugins[0].error.is_none());
    assert!(!h.root.path().join("work/.moc/config.toml").exists());
    h.close().await;
}

#[tokio::test]
async fn global_plugin_writes_global_config() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(&h, "counter", true, COUNTER);
    let session = h.session(SessionEngine::Standard).await;
    assert_eq!(
        command(&h, &session, "counter", "prefix shared").await,
        "shared"
    );
    let saved: toml::Value =
        toml::from_str(&std::fs::read_to_string(h.root.path().join("home/config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        saved["plugins"]["counter"]["config"]["prefix"].as_str(),
        Some("shared")
    );
    assert!(!h.root.path().join("work/.moc/config.toml").exists());
    h.close().await;
}

#[tokio::test]
async fn invalid_plugin_write_disables_only_the_writer_and_preserves_old_value() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(
        &h,
        "bad",
        false,
        r#"
      import { config } from 'moc';
      config.register('count', {type:'integer',default:1});
      export default {commands:{bad:{description:'无效写入',run(){config.set('count','wrong')}}}};
    "#,
    );
    install(
        &h,
        "good",
        false,
        "export default {commands:{good:{description:'正常插件',run(){return 'ok'}}}};",
    );
    let session = h.session(SessionEngine::Standard).await;
    assert!(
        h.client
            .call::<PluginRunCommand>(PluginRunCommandParams {
                session_id: session.clone(),
                name: "bad".into(),
                args: "".into(),
            })
            .await
            .is_err()
    );
    let list = h
        .client
        .call::<PluginList>(PluginListParams {
            session_id: session.clone(),
        })
        .await
        .unwrap()
        .plugins;
    assert!(
        list.iter()
            .find(|p| p.name == "bad")
            .unwrap()
            .error
            .is_some()
    );
    assert!(
        list.iter()
            .find(|p| p.name == "good")
            .unwrap()
            .error
            .is_none()
    );
    assert_eq!(command(&h, &session, "good", "").await, "ok");
    assert!(
        !h.root.path().join("work/.moc/config.toml").exists(),
        "错误值不能发布"
    );
    h.session(SessionEngine::Standard).await;
    h.close().await;
}

#[tokio::test]
async fn caught_invalid_state_write_still_disables_the_plugin() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(
        &h,
        "bad-state",
        false,
        r#"
      import { session } from 'moc';
      export default {commands:{bad:{description:'捕获无效写入',run(){
        try { session().state.invalid = { nested: Infinity }; } catch (_) {}
        return 'caught';
      }}}};
    "#,
    );
    let session = h.session(SessionEngine::Standard).await;
    let _ = h
        .client
        .call::<PluginRunCommand>(PluginRunCommandParams {
            session_id: session.clone(),
            name: "bad".into(),
            args: "".into(),
        })
        .await;
    let plugins = h
        .client
        .call::<PluginList>(PluginListParams {
            session_id: session.clone(),
        })
        .await
        .unwrap()
        .plugins;
    assert!(
        plugins[0].error.is_some(),
        "插件不能catch后绕过非法持久化写入的禁用"
    );
    h.session(SessionEngine::Standard).await;
    h.close().await;
}

#[tokio::test]
async fn invalid_persisted_plugin_value_does_not_break_core_and_remains_repairable() {
    let h = Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    install(&h, "counter", false, COUNTER);
    std::fs::write(
        h.root.path().join("work/.moc/config.toml"),
        "[plugins.counter.config]\nprefix=42\n",
    )
    .unwrap();
    let session = h.session(SessionEngine::Standard).await;
    let list = h
        .client
        .call::<PluginList>(PluginListParams {
            session_id: session.clone(),
        })
        .await
        .unwrap()
        .plugins;
    assert!(list[0].error.is_some(), "坏配置只能禁用该插件");
    let ConfigReply::Settings(view) = h
        .client
        .call::<ConfigManage>(ConfigAction::Read {
            scope: ConfigScope::Project,
            cwd: h.root.path().join("work").to_string_lossy().into_owned(),
        })
        .await
        .unwrap()
    else {
        panic!("设置文档")
    };
    assert!(
        view.fields
            .iter()
            .any(|f| f.path == ["plugins", "counter", "config", "prefix"]),
        "禁用插件的声明必须保留用于修复"
    );
    let mut document = view.document.clone();
    document["plugins"]["counter"]["config"]["prefix"] = json!("repaired");
    h.client
        .call::<ConfigManage>(ConfigAction::Save {
            scope: ConfigScope::Project,
            cwd: h.root.path().join("work").to_string_lossy().into_owned(),
            document,
            revision: view.revision,
        })
        .await
        .unwrap();
    assert_eq!(command(&h, &session, "counter", "read").await, "repaired:0");
    h.close().await;
}
