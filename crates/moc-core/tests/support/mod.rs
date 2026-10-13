pub mod content;

use moc_core::{Core, CoreConfig};
use moc_protocol::{ClientBuilder, ClientHandle, Server, ServerConnection, methods::*, transport};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::method,
};

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

/// 剪枝类压缩用例的长工具输出标记：剪枝前必须出现在 `read` 结果里，剪枝后消失。
pub const PRUNE_MARKER: &str = "PRUNE-ME-OMEGA";

/// 挂载「首轮按 `read` 读取长文件、随后收尾」的 chat 上游。
///
/// 非流式请求返回 `summary`（压缩摘要）：留空会让 soft 压缩以空摘要失败，
/// 用于验证压缩链的失败分支。`long_file` 需要事先写入超过剪枝阈值的内容。
pub async fn mount_read_round(
    upstream: &MockServer,
    long_file: &std::path::Path,
    summary: &str,
) {
    let path = long_file.to_string_lossy().into_owned();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let summary = summary.to_owned();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] != true {
                return ResponseTemplate::new(200).set_body_json(complete(&summary));
            }
            let has_tool_result = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if counter.fetch_add(1, Ordering::SeqCst) == 0 && !has_tool_result {
                return ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![(
                        "read_long",
                        "read",
                        json!({ "path": path.clone() }),
                    )]),
                    "text/event-stream",
                );
            }
            ResponseTemplate::new(200).set_body_raw(text_stream("已读取"), "text/event-stream")
        })
        .mount(upstream)
        .await;
}

/// 从会话加载结果里取出全部工具结果正文。
pub fn tool_result_texts(loaded: &SessionLoadResult) -> Vec<String> {
    loaded
        .entries
        .iter()
        .flat_map(|entry| {
            entry
                .message
                .tool_results()
                .map(|result| result.content.clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// 读取磁盘上的 transcript entries。
pub async fn transcript_entries(harness: &Harness, session_id: &str) -> Vec<moc_core::session::entries::Entry> {
    let store = moc_core::session::store::SessionStore::new(harness.root.path().join("sessions"));
    let paths = store
        .find(session_id)
        .await
        .expect("查找会话")
        .expect("会话已落盘");
    store.read_entries(&paths).await.expect("读取 JSONL")
}
