use super::{ENGINES, EngineMenu, engine_label, model_selector_label};
use moc_protocol::SessionEngine;
use ratatui::layout::Rect;

/// M17：切会话收尾上一会话的本地状态，但保留会话级在飞记账。
///
/// 不重置会让队列永久不派发（`prompt_running` 卡真）、状态栏显示上一会话的用量、
/// Esc/Ctrl+C 走进「停止当前提示」分支。
#[test]
fn switch_session_resets_local_prompt_state() {
    use super::{App, CtrlCPending};
    use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus};
    use moc_protocol::{SessionEngine, SessionInfo, Usage};
    use tokio::sync::mpsc::unbounded_channel;

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let session = |id: &str| SessionInfo {
        id: id.into(),
        title: "标题".into(),
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
    };

    // 旧会话正在跑提示：计时、用量、todo、临时模式与取消标记都在飞。
    app.session = Some(session("ses_a"));
    app.prompt_running = true;
    app.prompt_started_at = Some(std::time::Instant::now());
    app.thinking_started_at = Some(std::time::Instant::now());
    app.cancel_requested = true;
    app.transient_mode = Some(moc_protocol::methods::PromptMode::Ask);
    app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
    app.usage = Usage {
        input_tokens: 10,
        ..Usage::default()
    };
    app.cost_usd = 1.5;
    app.todos = vec![TodoPhase {
        phase: "阶段".into(),
        items: vec![TodoItem {
            status: TodoStatus::Pending,
            task: "任务".into(),
            reason: None,
        }],
    }];
    app.todo_hud.collapsed = true;
    app.todo_hud.drag_origin = Some((4, 5));
    app.todo_hud.chip_area = Rect::new(1, 2, 3, 1);
    app.inflight_prompts.insert("ses_a".into());
    app.inflight_session_ops.insert("ses_a".into());
    app.interrupt_pending.insert("ses_a".into());
    app.register_tool_batch("batch_a".into(), ["call_a".into()]);

    // 切到一个空闲会话：本地状态清空，会话级记账仍留给原会话回收。
    app.switch_session(session("ses_b"), Vec::new());
    assert!(!app.prompt_running, "新会话按空闲呈现");
    assert!(!app.cancel_requested);
    assert!(app.ctrl_c_pending.is_none());
    assert!(app.prompt_started_at.is_none());
    assert!(app.thinking_started_at.is_none());
    assert!(app.transient_mode.is_none());
    assert_eq!(app.usage, Usage::default());
    assert_eq!(app.cost_usd, 0.0);
    assert!(app.todos.is_empty());
    assert!(!app.todo_hud.collapsed, "todo HUD 状态不跨会话");
    assert!(app.todo_hud.drag_origin.is_none());
    assert!(app.todo_hud.chip_area.is_empty());
    assert!(app.tool_batches.is_empty(), "批次暂存不跨会话留存");
    assert!(app.inflight_prompts.contains("ses_a"), "在飞记账按会话保留");
    assert!(app.inflight_session_ops.contains("ses_a"));
    assert!(app.interrupt_pending.contains("ses_a"));

    // 切回仍在运行的会话：按记账继续呈现「运行中」，排队判定与停止键才对。
    app.switch_session(session("ses_a"), Vec::new());
    assert!(app.prompt_running, "切回运行中的会话应显示运行中");
}

/// 模型与模式在界面上一律用英文标识，且思考关闭时不出现在模型名里。
#[test]
fn labels_stay_english() {
    assert_eq!(model_selector_label("test/model:off"), "test/model");
    assert_eq!(model_selector_label("test/model:high"), "test/model:high");
    assert_eq!(model_selector_label("test/model"), "test/model");
    assert_eq!(
        model_selector_label("fusion:test/lead:off+test/side:high"),
        "Fusion · test/lead + test/side:high"
    );
    assert_eq!(engine_label(SessionEngine::Standard), "standard");
    assert_eq!(engine_label(SessionEngine::Ptc), "ptc");
    assert_eq!(engine_label(SessionEngine::ClmExperimental), "(clm)");
}

/// 菜单命中只认边框之间的一行一条目；边框与菜单外都不算条目。
#[test]
fn engine_menu_hit_testing_skips_borders() {
    let menu = EngineMenu {
        selected: 1,
        items: ENGINES.to_vec(),
        area: Rect::new(10, 5, 24, 6),
    };
    assert_eq!(menu.entry_at(11, 5), None, "上边框不是条目");
    assert_eq!(menu.entry_at(11, 6), Some(0));
    assert_eq!(menu.entry_at(11, 8), Some(2));
    assert_eq!(menu.entry_at(11, 9), None, "末行之下不命中");
    assert_eq!(menu.entry_at(11, 10), None, "下边框不是条目");
    assert_eq!(menu.entry_at(9, 6), None, "左缘之外不命中");
    assert_eq!(menu.entry_at(34, 6), None, "右缘之外不命中");
    assert_eq!(menu.entry_at(11, 4), None, "菜单上方不命中");
}

/// 没有活动会话就不开菜单，只提示（此时图例本身显示 `idle`）。
#[test]
fn engine_menu_requires_an_active_session() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.open_engine_menu();
    assert!(app.engine_menu.is_none());
    assert_eq!(app.notice.as_deref(), Some("当前没有活动会话"));
}

/// 权限菜单：无会话只提示；打开时按平台快照给出候选并高亮当前模式。
#[test]
fn permission_menu_highlights_current_mode() {
    use moc_protocol::{SessionInfo, SessionPermission};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.open_permission_menu();
    assert!(app.permission_menu.is_none());
    assert_eq!(app.notice.as_deref(), Some("当前没有活动会话"));

    app.session = Some(SessionInfo {
        id: "ses_perm".into(),
        title: "测试".into(),
        model: "test/model".into(),
        permission: SessionPermission::Workspace,
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
    app.sandbox_modes = vec![
        SessionPermission::Full,
        SessionPermission::Workspace,
        SessionPermission::WorkspaceOnly,
    ];
    app.open_permission_menu();
    let menu = app.permission_menu.as_ref().expect("权限菜单应打开");
    assert_eq!(menu.items, app.sandbox_modes);
    assert_eq!(menu.selected, 1, "默认高亮当前权限模式");
}

/// HUD 交互：默认展开卡片；单击卡片折叠成胶囊、单击胶囊展开，悬停不动状态。
#[test]
fn todo_hud_click_toggles_card_and_chip() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.todo_hud.card_area = Rect::new(10, 2, 50, 10);

    assert!(app.todo_hud.is_open(), "默认展开态");
    assert!(!app.todo_hud.pointer_moved((5, 20)));
    assert!(app.todo_hud.is_open(), "悬停 HUD 外不改状态");

    // 单击卡片 → 折叠成胶囊。
    assert!(app.todo_hud.pointer_down((12, 3)));
    assert!(app.todo_hud.pointer_up((12, 3)));
    assert!(!app.todo_hud.is_open());

    // 折叠态只认胶囊：悬停不展开，原卡片位置也不再命中。
    app.todo_hud.chip_area = Rect::new(70, 2, 10, 1);
    assert!(app.todo_hud.pointer_moved((72, 2)), "悬停胶囊仍算命中");
    assert!(!app.todo_hud.is_open(), "悬停不再展开");
    assert!(!app.todo_hud.pointer_down((12, 3)));

    // 单击胶囊 → 回到展开卡片。
    assert!(app.todo_hud.pointer_down((72, 2)));
    assert!(app.todo_hud.pointer_up((72, 2)));
    assert!(app.todo_hud.is_open());

    // 按下后移出 HUD 再松开：取消点击，不切换。
    app.todo_hud.card_area = Rect::new(10, 2, 50, 10);
    assert!(app.todo_hud.pointer_down((12, 3)));
    assert!(app.todo_hud.pointer_up((5, 20)));
    assert!(app.todo_hud.is_open());
}

/// 拖动阈值：小位移仍是单击；越过阈值移动卡片并夹在转写区内。
#[test]
fn todo_hud_drag_threshold_and_clamp() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.todo_hud.card_area = Rect::new(10, 2, 40, 8);
    let area = Rect::new(0, 0, 80, 24);

    assert!(app.todo_hud.pointer_down((12, 3)));
    // 按住卡片期间 Moved 落在命中区外也算在 HUD 上。
    assert!(app.todo_hud.pointer_moved((70, 20)));
    assert!(app.todo_hud.pointer_drag((14, 5), area));
    assert!(app.todo_hud.drag_origin.is_none(), "两格内不算拖动");

    assert!(app.todo_hud.pointer_drag((200, 30), area));
    assert_eq!(app.todo_hud.drag_origin, Some((40, 16)), "原点夹在转写区内");
    assert!(app.todo_hud.pointer_up((200, 30)));
    assert!(app.todo_hud.is_open(), "拖动结束保持展开");
}

/// 非空更新保留折叠状态；清空 todo 时 HUD 全量复位回展开态。
#[test]
fn todo_hud_resets_only_when_todos_clear() {
    use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.todo_hud.collapsed = true;
    app.todo_hud.drag_origin = Some((4, 5));
    app.update_todos(vec![TodoPhase {
        phase: "阶段".into(),
        items: vec![TodoItem {
            status: TodoStatus::Pending,
            task: "任务".into(),
            reason: None,
        }],
    }]);
    assert!(app.todo_hud.collapsed, "todo 仍在时保留折叠状态");
    assert_eq!(app.todo_hud.drag_origin, Some((4, 5)));

    app.update_todos(Vec::new());
    assert!(!app.todo_hud.collapsed);
    assert!(app.todo_hud.drag_origin.is_none());
}

#[test]
fn finish_turn_reports_stop_after_cancel_request() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.prompt_running = true;
    app.cancel_requested = true;
    app.finish_prompt();

    assert!(!app.prompt_running);
    assert!(!app.cancel_requested);
    assert_eq!(app.notice.as_deref(), Some("已停止"));
}

/// 思考 token 只回填本次响应的思考块：下一响应若没有思考块，
/// 上游报 0 不得把上一块已记下的 token 数抹掉。
#[test]
fn usage_reasoning_tokens_stay_with_their_response() {
    use super::{App, Usage};
    use crate::transcript::Block;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let tokens = |app: &App| match &app.blocks[0].block {
        Block::Thinking {
            reasoning_tokens, ..
        } => *reasoning_tokens,
        _ => panic!("期望思考块"),
    };

    app.start_thinking();
    app.finish_thinking();
    app.apply_usage(Usage {
        reasoning_tokens: Some(120),
        ..Usage::default()
    });
    assert_eq!(tokens(&app), Some(120));

    app.start_generation();
    app.apply_usage(Usage {
        reasoning_tokens: Some(0),
        ..Usage::default()
    });
    assert_eq!(tokens(&app), Some(120), "没有思考块的响应不能覆盖上一块");
}

/// 会话累计用量：请求级 usage 与累计统计分开存，状态栏的 in/out/tps 只读后者。
#[test]
fn session_usage_is_authoritative_and_ordered() {
    use super::{App, Usage};
    use moc_protocol::{ModelUsageSummary, SessionUsageResult};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let summary = |output: u64, active_ms: u64| ModelUsageSummary {
        model: "p/m".into(),
        source: moc_protocol::methods::UsageSource::Session,
        calls: 2,
        input_tokens: 12_000,
        output_tokens: output,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        reasoning_tokens: None,
        active_ms,
        ttft_ms: 12_000,
        ttft_calls: 2,
        gen_ms: active_ms.saturating_sub(12_000),
    };
    let usage = |output: u64, active_ms: u64| SessionUsageResult {
        models: Vec::new(),
        total: summary(output, active_ms),
        tool_ms: 0,
    };

    let first = app.begin_usage_request();
    app.apply_session_usage(first, usage(1_200, 60_000));
    assert_eq!(app.session_tokens(), (12_000, 1_200));
    // TPS 只除生成阶段（60s - 12s），不把首 token 等待算进去。
    assert!((app.session_tokens_per_second() - 25.0).abs() < 1e-9);

    // 旧会话没有首 token 数据：退回整段用时。
    let mut legacy = usage(1_200, 60_000);
    legacy.total.gen_ms = 0;
    legacy.total.ttft_ms = 0;
    legacy.total.ttft_calls = 0;
    let legacy_request = app.begin_usage_request();
    app.apply_session_usage(legacy_request, legacy);
    assert!((app.session_tokens_per_second() - 20.0).abs() < 1e-9);
    let refreshed = app.begin_usage_request();
    app.apply_session_usage(refreshed, usage(1_200, 60_000));

    // 请求级 usage 不参与累计口径。
    app.apply_usage(Usage {
        input_tokens: 400,
        output_tokens: 80,
        ..Usage::default()
    });
    assert_eq!(app.session_tokens(), (12_000, 1_200));

    // 更旧的请求迟到：不得回退统计。
    app.apply_session_usage(first, usage(1, 1));
    assert_eq!(app.session_tokens(), (12_000, 1_200));

    let second = app.begin_usage_request();
    app.apply_session_usage(second, usage(2_400, 120_000));
    assert_eq!(app.session_tokens(), (12_000, 2_400));

    // 切会话：旧统计清空，在飞回执作废。
    app.switch_session(
        moc_protocol::SessionInfo {
            id: "ses_next".into(),
            title: "下一个".into(),
            model: "p/m".into(),
            permission: moc_protocol::SessionPermission::Full,
            engine: moc_protocol::SessionEngine::Standard,
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
        },
        Vec::new(),
    );
    assert_eq!(app.session_tokens(), (0, 0));
    app.apply_session_usage(second, usage(9_999, 1_000));
    assert_eq!(app.session_tokens(), (0, 0), "跨会话迟到的回执必须丢弃");
}

/// 过期反馈仍需唤醒循环擦除；消费后不再安排唤醒，避免空转。
#[test]
fn overdue_selection_copy_keeps_wakeup_until_consumed() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.selection_copied_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(600));
    let deadline = app.selection_copy_deadline().expect("过期反馈仍需擦除");
    assert!(deadline < std::time::Instant::now());
    app.selection_copied_at = None;
    assert!(app.selection_copy_deadline().is_none());
}

/// 告警与 notice 的 TTL：期内可见且给出未来的截止时刻，过期后两者都消失。
#[test]
fn warning_and_notice_expire_after_their_ttl() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);

    app.set_warning("effort max → high");
    app.set_notice("已停止");
    assert!(app.warning_visible());
    assert!(app.notice_visible());
    // 截止时刻必须落在未来，事件循环才能安全 sleep_until 而非空转
    assert!(app.warning_deadline().unwrap() > std::time::Instant::now());
    assert!(app.notice_deadline().unwrap() > std::time::Instant::now());

    let stale = std::time::Instant::now() - std::time::Duration::from_secs(600);
    app.warning_at = Some(stale);
    app.notice_at = Some(stale);
    assert!(!app.warning_visible());
    assert!(!app.notice_visible());
    assert!(app.warning_deadline().is_none());
    assert!(app.notice_deadline().is_none());
    // 过期的槽位不再需要重绘，事件循环据此不再安排超时分支
    assert_eq!(app.warning.as_deref(), Some("effort max → high"));
}

/// `/retry` 已把失败的那一轮从会话里剪掉：转写里对应的块也必须撤掉。
#[test]
/// `/retry` 撤掉的是「没跑完」的那一轮，已经落库的内容一律留着。
fn rewind_drops_the_failed_tail_of_the_last_turn() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.push_block(crate::transcript::Block::User {
        text: "先看看仓库".into(),
    });
    app.push_block(crate::transcript::Block::Assistant {
        text: "上一轮的回答".into(),
    });
    app.push_block(crate::transcript::Block::User {
        text: "继续".into(),
    });
    app.push_block(crate::transcript::Block::Assistant {
        text: "半截正文".into(),
    });
    app.push_block(crate::transcript::Block::Note {
        text: "错误：连接中断".into(),
        error: true,
    });

    assert!(app.rewind_failed_turn(false));

    assert_eq!(app.blocks.len(), 3, "只撤最后一条用户消息之后的块");
    assert!(
        matches!(&app.blocks[2].block, crate::transcript::Block::User { text } if text == "继续")
    );
    // 尾部本来就没有可撤的块：不报撤过，免得把上一轮的正文当成失败回合吃掉。
    assert!(!app.rewind_failed_turn(false));
    assert_eq!(app.blocks.len(), 3);
}

#[test]
fn failed_generation_removes_only_uncommitted_preview() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.push_block(crate::transcript::Block::Assistant {
        text: "已提交正文".into(),
    });
    app.start_generation();
    assert!(
        !app.has_streaming(|block| matches!(block, crate::transcript::Block::Assistant { .. }))
    );
    app.push_block(crate::transcript::Block::Assistant {
        text: "异常重复预览".into(),
    });
    app.discard_generation();
    assert_eq!(app.blocks.len(), 1);
    assert!(
        matches!(&app.blocks[0].block, crate::transcript::Block::Assistant { text } if text == "已提交正文")
    );
}

/// 正文继续流式增长时，旧图表状态必须立刻作废，避免 index 漂移后画旧图。
#[test]
fn streaming_append_invalidates_chart_state() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = super::App::new(crate::theme::Theme::global(), events);
    app.scrollback_area = Rect::new(3, 0, 80, 30);
    app.push_block(crate::transcript::Block::Assistant {
        text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
    });
    app.toggle_table_chart(0, 0);
    assert!(app.blocks[0].table_chart(0).is_some());
    app.append_streaming(
        |block| match block {
            crate::transcript::Block::Assistant { text } => Some(text),
            _ => None,
        },
        "\n\n追加正文",
    );
    assert!(
        app.blocks[0].table_chart(0).is_none(),
        "正文变化后旧图必须清掉"
    );
    assert!(
        matches!(&app.blocks[0].block, crate::transcript::Block::Assistant { text } if text.ends_with("追加正文"))
    );
}

/// Fusion 会话的定价与上下文窗口都按 lead 侧模型取。
#[test]
fn fusion_session_bills_the_lead_model() {
    use super::App;
    use moc_protocol::methods::{ModelInfo, SessionInfo};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.models = vec![ModelInfo {
        id: "test/lead".into(),
        display_name: "Lead".into(),
        thinking_levels: None,
        context_window: Some(100_000),
        max_output_tokens: None,
        pricing: Some(moc_protocol::methods::ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: None,
        }),
        inputs: Vec::new(),
        fast_mode: false,
    }];
    app.session = Some(SessionInfo {
        id: "ses_fusion".into(),
        title: "Fusion".into(),
        model: "fusion:test/lead:off+test/side:off".into(),
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
    assert_eq!(
        app.current_pricing().map(|pricing| pricing.input),
        Some(3.0)
    );
    assert_eq!(app.context_window(), Some(100_000));
}

/// 用量快照的成本按每一行各自的模型定价折算；无定价信息的行按 0 计。
#[test]
fn session_usage_snapshot_prices_rows_by_their_own_model() {
    use super::App;
    use moc_protocol::methods::{ModelInfo, UsageSource};
    use moc_protocol::{ModelUsageSummary, SessionUsageResult};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.models = vec![ModelInfo {
        id: "p/m".into(),
        display_name: "M".into(),
        thinking_levels: None,
        context_window: None,
        max_output_tokens: None,
        pricing: Some(moc_protocol::methods::ModelPricing {
            input: 2.0,
            output: 10.0,
            cache_read: Some(0.2),
            cache_write: None,
        }),
        inputs: Vec::new(),
        fast_mode: false,
    }];
    let summary = |model: &str, source, input: u64, cached: u64, output: u64| ModelUsageSummary {
        model: model.into(),
        source,
        calls: 1,
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: Some(cached),
        cache_creation_input_tokens: None,
        reasoning_tokens: None,
        active_ms: 0,
        ttft_ms: 0,
        ttft_calls: 0,
        gen_ms: 0,
    };
    app.apply_session_usage(
        1,
        SessionUsageResult {
            models: vec![
                summary("p/m:off", UsageSource::Session, 1_000_000, 400_000, 100_000),
                summary("p/m:off", UsageSource::Sidekick, 500_000, 0, 50_000),
                // 无定价信息的行按 0 计。
                summary("x/unknown", UsageSource::Subagent, 9_000_000, 0, 9_000_000),
            ],
            total: summary("total", UsageSource::Session, 0, 0, 0),
            tool_ms: 0,
        },
    );
    // 会话行 0.6M*2 + 0.4M*0.2 + 0.1M*10 = 2.28；sidekick 行 0.5M*2 + 0.05M*10 = 1.5。
    assert!((app.cost_usd - 3.78).abs() < 1e-9, "{}", app.cost_usd);
}
