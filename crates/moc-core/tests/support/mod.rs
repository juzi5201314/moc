pub mod content;

use moc_core::{Core, CoreConfig};
use moc_protocol::{ClientBuilder, ClientHandle, Server, ServerConnection, methods::*, transport};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Harness {
    pub root: tempfile::TempDir,
    pub core: Arc<Core>,
    pub client: ClientHandle,
    pub updates: Arc<Mutex<Vec<SessionUpdate>>>,
    pub events: tokio::sync::broadcast::Sender<SessionUpdate>,
    pub server: Arc<Server<Core>>,
    pub connection: ServerConnection,
}

impl Harness {
    pub async fn new(url: &str, api: &str, extra: &str) -> Self {
        Self::with_roles(url, api, "test/gpt-test:off", "test/gpt-test:off", extra).await
    }

    /// 指定主模型与计划模型角色的测试内核。
    ///
    /// `plan` 传 `none` 表示进入计划模式不切换模型；`extra` 追加在配置末尾，只能
    /// 新增表，不能重复定义已有的表。
    pub async fn with_roles(url: &str, api: &str, default: &str, plan: &str, extra: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("config.toml"), format!(
            "[models.roles]\ndefault='{default}'\nexplore='{default}'\nplan='{plan}'\n\
             [providers.test]\napi='{api}'\nbase_url='{url}/v1'\napi_key=''\nretry={{max_attempts=1}}\n\
             [providers.test.models.gpt-test]\ncontext_window=1000000\nmax_output_tokens=4096\n\
             [workspace]\nwatch=false\n[context_files]\nenabled=false\n{extra}\n"
        )).unwrap();
        let core = Core::new(CoreConfig {
            home,
            cwd,
            overlays: vec![],
            session_dir: Some(root.path().join("sessions")),
            client: moc_llm::Client::new(),
        })
        .unwrap();
        let server = Server::new((*core).clone());
        let (server_side, client_side) = transport::duplex(65536);
        let connection = server.spawn(server_side);
        let updates = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&updates);
        let (events, _) = tokio::sync::broadcast::channel(1024);
        let notifications = events.clone();
        let (client, _) = ClientBuilder::new(ClientInfo {
            name: "验收".into(),
            version: "1".into(),
            frontend: "test".into(),
        })
        .capabilities(ClientCapabilities {
            permission_requests: false,
            ask_requests: false,
        })
        .on_notification::<SessionUpdateMethod, _, _>(move |params, _| {
            output.lock().push(params.update.clone());
            let _ = notifications.send(params.update);
            std::future::ready(())
        })
        .connect(client_side)
        .await
        .unwrap();
        Self {
            root,
            core,
            client,
            updates,
            events,
            server,
            connection,
        }
    }

    pub async fn session(&self, mode: SessionMode) -> String {
        self.client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode,
            })
            .await
            .unwrap()
            .id
    }

    pub async fn prompt(&self, id: &str, text: &str) -> PromptResult {
        self.client
            .call::<SessionPrompt>(PromptParams {
                session_id: id.into(),
                input: vec![InputPart::Text { text: text.into() }],
                mode: None,
            })
            .await
            .unwrap()
    }

    pub async fn load(&self, id: &str) -> SessionLoadResult {
        self.client
            .call::<SessionLoad>(SessionLoadParams {
                session_id: id.into(),
                max_entries: None,
            })
            .await
            .unwrap()
    }

    pub async fn close(self) {
        self.server.shutdown();
        self.connection.wait().await;
        self.core.shutdown().await;
    }
}

pub fn text_stream(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id":"response","model":"gpt-test",
            "choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]
        })
    )
}

pub fn tool_stream(calls: Vec<(&str, &str, Value)>) -> String {
    let calls: Vec<_> = calls.into_iter().enumerate().map(|(index, (id, name, args))| json!({
        "index":index,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}
    })).collect();
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id":"tools","model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":calls},"finish_reason":"tool_calls"}]
        })
    )
}

pub fn complete(text: &str) -> Value {
    json!({"id":"summary","model":"gpt-test","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
}
