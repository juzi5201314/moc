use super::*;
use crate::app::CtrlCPending;
use crate::event::agents::focus_agent;
use crate::event::clipboard::selection_text;
use crate::event::commands::execute_command;
use crate::event::completion::handle_completion_key;
use crate::event::context::handle_context_content_mouse;
use crate::event::keyboard::commit_prompt;
use crate::event::keyboard::handle_key;
use crate::event::modal_key::handle_modal_key;
use crate::event::mouse::handle_mouse_with_connection;
use crate::event::mouse::handle_todo_hud_mouse;
use crate::event::plugins::plugin_notice_note;
use crate::event::scrollback::handle_mouse;
use crate::event::updates::apply_update;
use crate::event::updates::apply_update_and_refresh;
use crate::history_page::HistoryClient;
use crate::modal::ContextContentState;
use crate::render::wrap::str_width;
use crate::transcript::ToolCallStart;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use moc_protocol::ConfigGet;
use moc_protocol::ConfigGetParams;
use moc_protocol::InputPart;
use moc_protocol::SessionLoad;
use moc_protocol::SessionLoadPageParams;
use moc_protocol::SessionLoadParams;
use moc_protocol::SessionNew;
use moc_protocol::SessionNewParams;
use moc_protocol::SessionUpdate;
use moc_protocol::UpstreamField;
use moc_protocol::methods::UpstreamFieldMismatch;
use moc_protocol::methods::{AskOption, AskParams, AskQuestion, AskResult};
use tokio::sync::mpsc::unbounded_channel;

fn app() -> App {
    let (events, _receiver) = unbounded_channel::<AppEvent>();
    App::new(Theme::global(), events)
}

mod agents;
mod ask_keyboard;
mod ask_mouse;
mod behavior;
mod commands;
mod context;
mod input;
mod model;
mod queue;
mod scrolling;
mod updates;

/// 搭一个连到真 core 的 app，并把事件接收端一并交出（用例要观察回执）。
async fn app_with_core() -> (
    std::sync::Arc<moc_core::Core>,
    EmbeddedConnection,
    App,
    tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    tempfile::TempDir,
) {
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\ncontext_window=200000\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    app.session = Some(session);
    (core, connection, app, receiver, root)
}

/// 门控压缩摘要响应：掐住 wiremock 应答直到 [`Hold::open`]，让压缩的在飞
/// 窗口由测试显式决定，替代固定毫秒延迟。
#[derive(Clone, Default)]
struct Hold {
    state: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl Hold {
    fn open(&self) {
        let (lock, condvar) = &*self.state;
        *lock.lock().expect("门闩未中毒") = true;
        condvar.notify_all();
    }
    /// wiremock 应答闭包内调用：阻塞至 open；60s 兜底防忘放行时死锁。
    fn wait(&self) {
        let (lock, condvar) = &*self.state;
        let mut open = lock.lock().expect("门闩未中毒");
        while !*open {
            let (guard, timeout) = condvar
                .wait_timeout(open, std::time::Duration::from_secs(60))
                .expect("门闩未中毒");
            open = guard;
            if timeout.timed_out() {
                break;
            }
        }
    }
}

/// 「切换并压缩 / 手动压缩」流程的测试内核：摘要响应由 [`Hold`] 掐住，
/// 让压缩在飞期间有可观测窗口。
struct SlowCompactionHarness {
    upstream: wiremock::MockServer,
    gate: Hold,
    _root: tempfile::TempDir,
    core: std::sync::Arc<moc_core::Core>,
    connection: EmbeddedConnection,
    events: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    app: App,
    session: String,
}

impl SlowCompactionHarness {
    async fn new() -> Self {
        use moc_core::{Core, CoreConfig};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let upstream = MockServer::builder().start().await;
        let gate = Hold::default();
        let held = gate.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("请求 JSON");
                if body["stream"] == true {
                    ResponseTemplate::new(200).set_body_raw(
                        "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"答复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    )
                } else {
                    held.wait();
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({
                            "id": "summary",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "检查点"},
                                "finish_reason": "stop"
                            }]
                        }))
                }
            })
            .mount(&upstream)
            .await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), format!(
            "[models.roles]\ndefault='test/model:off'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\ncontext_window=100000\n[providers.test.models.tiny]\ncontext_window=1000\nmax_output_tokens=4096\n[compaction]\nauto=false\nmethod='moc'\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
            upstream.uri(),
        )).unwrap();
        let core = Core::new(CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            user_home: Some(root.path().join("userhome")),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .unwrap();
        let (events, events_rx) = unbounded_channel();
        let connection = connect_embedded(core.clone(), events.clone())
            .await
            .unwrap();
        let mut app = App::new(Theme::global(), events);
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                engine: SessionEngine::Standard,
                plan_active: false,
                fast_mode: false,
            })
            .await
            .unwrap();
        let id = session.id.clone();
        app.session = Some(session);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams {})
            .await
            .unwrap()
            .models;
        // 压缩需要可摘要的历史。
        connection
            .client
            .call::<moc_protocol::SessionPrompt>(moc_protocol::PromptParams {
                session_id: id.clone(),
                input: vec![InputPart::Text {
                    text: "你好".into(),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
            .unwrap();
        Self {
            upstream,
            gate,
            _root: root,
            core,
            connection,
            events: events_rx,
            app,
            session: id,
        }
    }

    /// 等到第一个满足条件的事件，丢弃无关事件。
    async fn next_event(&mut self, matches: impl Fn(&AppEvent) -> bool) -> AppEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = self.events.recv().await.expect("事件流未关闭");
                if matches(&event) {
                    break event;
                }
            }
        })
        .await
        .expect("等待事件超时")
    }

    async fn settle(&mut self) -> AppEvent {
        self.next_event(|event| matches!(event, AppEvent::SessionControlSettled { .. }))
            .await
    }

    /// 提交一条普通消息；返回它是否停在队列里（而不是立刻发出）。
    fn submit_message(&mut self, text: &str) -> bool {
        self.app.prompt.insert_str_raw(text);
        crate::queue::submit(&mut self.app, &self.connection, false);
        !self.app.prompt_running
            && !self.app.inflight_prompts.contains(&self.session)
            && self
                .app
                .queues
                .get(&self.session)
                .is_some_and(|queue| !queue.is_empty())
    }

    async fn transcript_contains(&self, needle: &str) -> bool {
        let loaded = self
            .connection
            .client
            .call::<SessionLoad>(SessionLoadParams {
                omit_auxiliary: false,
                session_id: self.session.clone(),
                max_entries: None,
                read_only: false,
            })
            .await
            .unwrap();
        loaded.entries.iter().any(|entry| {
            entry
                .context_message()
                .is_some_and(|message| message.text().contains(needle))
        })
    }

    async fn shutdown(self) {
        self.connection.shutdown().await;
        self.core.shutdown().await;
    }
}

fn app_with_completion() -> App {
    let mut app = app();
    // /r 的候选里 /retry 需要会话；补一个会话保留双候选的回绕语义。
    app.session = Some(moc_protocol::SessionInfo {
        id: "ses_completion".into(),
        title: "补全".into(),
        model: "test/model".into(),
        permission: moc_protocol::SessionPermission::Full,
        engine: SessionEngine::Standard,
        autonomy: moc_protocol::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    });
    app.prompt.insert_str_raw("/r");
    app.completion = crate::widgets::completion::CompletionState::derive(&app);
    app
}

async fn setup_test_app() -> (std::sync::Arc<moc_core::Core>, EmbeddedConnection, App) {
    let (core, connection, app, _root) = setup_test_app_keeping_root().await;
    (core, connection, app)
}

/// 同 [`setup_test_app`]，但把临时目录一并交出，供需要再建会话的用例使用。
///
/// 会话的 cwd 是那个临时目录：函数返回即删除，之后再 `session/new` 会因
/// cwd 不存在而失败，所以这些用例必须自己握住目录。
async fn setup_test_app_keeping_root() -> (
    std::sync::Arc<moc_core::Core>,
    EmbeddedConnection,
    App,
    tempfile::TempDir,
) {
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    app.session = Some(session);
    (core, connection, app, root)
}

/// 画一帧整屏 UI，让鼠标命中几何与真实绘制同源。
fn draw_frame(
    app: &mut App,
    width: u16,
    height: u16,
) -> ratatui::Terminal<ratatui::backend::TestBackend> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    let mut view = crate::render::ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, app, &mut view))
        .unwrap();
    terminal
}

include!("../event_model_picker_tests.rs");

/// 左键单击事件。
fn left_click(column: u16, row: u16) -> crossterm::event::MouseEvent {
    mouse_event(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column,
        row,
    )
}

/// 指定类型与坐标的鼠标事件。
fn mouse_event(
    kind: crossterm::event::MouseEventKind,
    column: u16,
    row: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// 单问 ask 模态：选项一个 + 自定义行（`z` 进入编辑态）。
fn ask_modal() -> Modal {
    ask_modal_with(vec![ask_question("q1")], None)
}

fn ask_question(id: &str) -> AskQuestion {
    AskQuestion {
        id: id.into(),
        question: "选哪个？".into(),
        header: None,
        multi: false,
        recommended: Vec::new(),
        options: vec![AskOption {
            label: "选项一".into(),
            description: None,
            preview: None,
        }],
    }
}

fn multi_ask_question() -> AskQuestion {
    AskQuestion {
        id: "features".into(),
        question: "开启哪些特性？".into(),
        header: None,
        multi: true,
        recommended: vec![0, 2],
        options: vec![
            AskOption {
                label: "缓存".into(),
                description: None,
                preview: None,
            },
            AskOption {
                label: "日志".into(),
                description: None,
                preview: None,
            },
            AskOption {
                label: "监控".into(),
                description: None,
                preview: None,
            },
        ],
    }
}

fn ask_modal_with(
    questions: Vec<AskQuestion>,
    reply: Option<tokio::sync::oneshot::Sender<Result<AskResult, moc_protocol::RpcError>>>,
) -> Modal {
    let count = questions.len();
    Modal::Ask {
        params: AskParams {
            session_id: "ses_test".into(),
            prompt_id: "prompt_test".into(),
            request_id: "ask_test".into(),
            questions,
        },
        question: 0,
        highlight: vec![0; count],
        selected: vec![Vec::new(); count],
        custom: vec![crate::modal::CustomAnswer::default(); count],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply,
    }
}

/// 当前问自定义输入的文本与光标。
fn ask_custom(app: &App) -> (String, usize) {
    match app.modal.as_ref() {
        Some(Modal::Ask {
            custom, question, ..
        }) => custom
            .get(*question)
            .map(|answer| (answer.text.clone(), answer.cursor))
            .unwrap_or_default(),
        _ => unreachable!("测试期间应保持 ask 模态"),
    }
}

/// 走一遍模态渲染路径（越界切分会在这里 panic）。
fn draw_ask(app: &mut App) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    terminal
        .draw(|frame| crate::render::modal::draw_modal(frame, app))
        .expect("绘制提问模态");
}

/// 造一个子代理视图：`session/new` 出来的会话 ID 就是代理 ID。
fn agent_view(session: &moc_protocol::methods::SessionInfo) -> crate::subagent::SubagentView {
    crate::subagent::SubagentView::new(moc_protocol::methods::AgentInfo {
        id: session.id.clone(),
        parent_id: session.parent_session_id.clone().unwrap_or_default(),
        title: session.title.clone(),
        model: session.model.clone(),
        cwd: session.cwd.clone(),
        preset: None,
        status: moc_protocol::methods::AgentStatus::Running,
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        last_message: None,
    })
}

async fn load_page(
    connection: &EmbeddedConnection,
    session_id: &str,
) -> anyhow::Result<moc_protocol::methods::SessionLoadPageResult> {
    connection
        .client
        .history_page(SessionLoadPageParams {
            full_messages: true,
            plan_page: Some(Default::default()),
            session_id: session_id.to_owned(),
            before_entry_id: None,
            max_entries: 128,
            max_message_bytes: Some(64 * 1024),
            redo_page: Some(Default::default()),
        })
        .await
        .map_err(anyhow::Error::from)
}

/// 子代理会话在测试里的替身：`session/new` 起的空会话即可承载聚焦语义。
async fn new_child(
    connection: &EmbeddedConnection,
    title: &str,
) -> moc_protocol::methods::SessionInfo {
    connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: Some(title.to_owned()),
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .expect("创建子会话")
}
