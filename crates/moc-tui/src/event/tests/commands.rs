use super::*;

#[tokio::test]
async fn export_command_writes_markdown_and_reports_existing_path_without_overwrite() {
    let (_core, connection, mut app, mut receiver, root) = app_with_core().await;
    for (command, success) in [
        ("/export 会话.md", true),
        ("/export 会话.md", false),
        ("/export", true),
    ] {
        execute_command(&mut app, &connection, command.into());
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(event @ AppEvent::CommandNote { .. }) = receiver.recv().await {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        let AppEvent::CommandNote { result, .. } = event else {
            unreachable!()
        };
        assert_eq!(result.is_ok(), success);
        if let Ok(note) = result {
            assert!(note.contains(root.path().to_str().unwrap()));
        }
    }
    let content = std::fs::read_to_string(root.path().join("会话.md")).unwrap();
    assert!(content.contains("test/model"));
    assert!(content.contains("暂无对话历史"));
}

#[tokio::test]
async fn compact_empty_session_settles_with_friendly_notice_without_model_calls() {
    let mut h = SlowCompactionHarness::new().await;
    let session = h
        .connection
        .client
        .call::<moc_protocol::SessionNew>(moc_protocol::SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    h.session = session.id.clone();
    h.app.session = Some(session);
    let before = h.upstream.received_requests().await.unwrap().len();
    execute_command(&mut h.app, &h.connection, "/compact".into());
    let event = h.settle().await;
    assert!(
        matches!(&event, AppEvent::SessionControlSettled { result: Ok(_), notice: Some(notice), .. } if notice == "暂无可压缩的对话历史")
    );
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert_eq!(h.app.notice.as_deref(), Some("暂无可压缩的对话历史"));
    assert!(!h.app.inflight_session_ops.contains(&h.session));
    assert_eq!(h.upstream.received_requests().await.unwrap().len(), before);
    h.shutdown().await;
}

/// `/usage` 输出会话统计与对齐表格：列宽一致、数字右对齐、token 走 k/m/b 单位，
/// 超宽模型名截断，末行是全部模型的合计。
#[test]
fn usage_table_aligns_columns_with_units() {
    use moc_protocol::{ModelUsageSummary, SessionUsageResult};

    let summary = |model: &str, calls| ModelUsageSummary {
        model: model.into(),
        source: moc_protocol::methods::UsageSource::Session,
        calls,
        input_tokens: 1_200_000,
        output_tokens: 12_345,
        cached_input_tokens: Some(1_100_000),
        cache_creation_input_tokens: None,
        reasoning_tokens: Some(1_200),
        active_ms: 62_000,
        ttft_ms: 12_000,
        ttft_calls: 2,
        gen_ms: 50_000,
    };
    let result = SessionUsageResult {
        models: vec![
            summary("cpa-chat/deepseek-v4.1-flash:max", 12),
            summary("p/m", 3),
        ],
        total: summary("total", 15),
        tool_ms: 248_000,
    };
    let table = format_usage(&result, 74);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(
        lines.len(),
        12,
        "会话统计标题 + 四行统计 + 空行 + 表头/分隔线/两个模型/分隔线/合计：\n{table}"
    );
    assert_eq!(lines[0], "会话统计");
    assert!(
        lines[1].contains("1m02s")
            && lines[1].contains("生成 50.0s")
            && lines[1].contains("网络等待 12.0s"),
        "模型用时必须拆成生成与网络等待：{table}"
    );
    assert!(lines[2].contains("4m08s"), "工具调用用时：{table}");
    assert!(lines[3].contains("6.0s"), "平均 TTFT = 12s / 2 次：{table}");
    assert!(
        lines[4].contains("246.9 tok/s"),
        "TPS 只除生成时间：{table}"
    );
    assert_eq!(lines[5], "", "统计和表格之间留一个空行");
    let table_lines = &lines[6..];
    let width = str_width(table_lines[0]);
    for line in table_lines {
        assert_eq!(str_width(line), width, "列宽必须一致：{table}");
        assert!(
            str_width(line) <= 71,
            "表格不得超过 80 列终端里 note 的可用宽度：{table}"
        );
    }
    assert!(
        table_lines[0].contains("模型")
            && table_lines[0].contains("生成")
            && table_lines[0].contains("TTFT")
            && table_lines[0].contains("TPS")
    );
    assert!(
        table.contains("1.2m") && table.contains("12.35k"),
        "{table}"
    );
    assert!(table.contains('…'), "超宽模型名应截断：{table}");
    let total = table_lines.last().expect("合计行");
    assert!(total.trim_start().starts_with("合计"), "{table}");
    assert!(total.contains("15"), "合计行用 total 的调用次数：{table}");
    // note 渲染时按 71 列折行：换行必须是硬断行，表格才能逐行保持对齐。
    assert_eq!(
        crate::render::wrap::wrap_text(&table, 71),
        lines
            .iter()
            .map(|line| (*line).to_owned())
            .collect::<Vec<_>>(),
        "表格在 note 里不得被折行拆散：{table}"
    );

    // 窄终端按优先级丢列：先丢思考、再丢缓存率，保住模型名与 TTFT。
    let narrow = format_usage(&result, 60);
    let narrow_lines: Vec<&str> = narrow.lines().collect();
    let narrow_table = &narrow_lines[narrow_lines.len() - 6..];
    for line in narrow_table {
        assert_eq!(str_width(line), str_width(narrow_table[0]), "{narrow}");
        assert!(str_width(line) <= 57, "{narrow}");
    }
    assert!(
        narrow_table[0].contains("TTFT")
            && !narrow_table[0].contains("思考")
            && !narrow_table[0].contains("缓存率"),
        "窄终端保留 TTFT，丢掉低优先级列：{narrow}"
    );

    // 新会话没有明细：不输出会话统计，表头、分隔线、合计三行。
    let empty = format_usage(
        &SessionUsageResult {
            models: Vec::new(),
            total: ModelUsageSummary {
                model: "total".into(),
                source: moc_protocol::methods::UsageSource::Session,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
                reasoning_tokens: None,
                active_ms: 0,
                ttft_ms: 0,
                ttft_calls: 0,
                gen_ms: 0,
            },
            tool_ms: 0,
        },
        74,
    );
    assert_eq!(empty.lines().count(), 3, "{empty}");
}

/// `/usage` 与状态栏后台刷新走同一条 core 统计：回执带请求序号，
/// `show` 决定是否把明细写进转写。
#[tokio::test]
async fn usage_refresh_reads_session_totals_from_core() {
    let (_core, connection, mut app, mut receiver, _root) = app_with_core().await;

    execute_command(&mut app, &connection, "/usage".into());
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("用量回执应在超时前返回")
        .expect("事件队列未关闭");
    let AppEvent::Usage {
        request,
        result,
        show,
    } = event
    else {
        panic!("应收到用量事件");
    };
    assert!(show, "/usage 要把明细写进转写");
    assert_eq!(request, 1, "回执带回发起时的请求序号");
    assert_eq!(result.total.calls, 0, "新会话还没有模型调用");
    app.apply_session_usage(request, result);
    assert_eq!(app.session_tokens(), (0, 0));

    // 提示收尾后的后台刷新同样取统计，但不写转写。
    let blocks = app.blocks.len();
    refresh_usage(&mut app, &connection, false);
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("后台用量回执应在超时前返回")
        .expect("事件队列未关闭");
    let AppEvent::Usage {
        request,
        result,
        show,
    } = event
    else {
        panic!("应收到用量事件");
    };
    assert!(!show, "后台刷新静默");
    assert!(request > 1);
    app.apply_session_usage(request, result);
    assert_eq!(app.blocks.len(), blocks, "静默刷新不写转写");
}

/// 点状态栏右侧统计段 = 输入 `/usage`：命中区正好盖住渲染的统计文本，
/// 点击发出带 `show` 的刷新请求（明细写进转写）。
#[tokio::test]
async fn statusline_usage_click_runs_usage_command() {
    use moc_protocol::{ModelUsageSummary, SessionUsageResult};

    let (core, connection, mut app, mut receiver, _root) = app_with_core().await;
    let summary = || ModelUsageSummary {
        model: "p/m".into(),
        source: moc_protocol::methods::UsageSource::Session,
        calls: 1,
        input_tokens: 1_200,
        output_tokens: 340,
        cached_input_tokens: None,
        cache_creation_input_tokens: None,
        reasoning_tokens: None,
        active_ms: 1_000,
        ttft_ms: 0,
        ttft_calls: 0,
        gen_ms: 0,
    };
    let request = app.begin_usage_request();
    app.apply_session_usage(
        request,
        SessionUsageResult {
            models: Vec::new(),
            total: summary(),
            tool_ms: 0,
        },
    );

    let terminal = draw_frame(&mut app, 100, 24);
    let area = app.usage_area;
    assert!(!area.is_empty(), "有用量时统计段应登记命中区");
    let text: String = (area.x..area.x.saturating_add(area.width))
        .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains('⇅'), "命中区应正好盖住统计段：{text:?}");

    handle_mouse_with_connection(&mut app, &connection, left_click(area.x, area.y));
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("点击应触发用量回执")
        .expect("事件队列未关闭");
    let AppEvent::Usage { show, .. } = event else {
        panic!("应收到用量事件");
    };
    assert!(show, "点击统计段等价 /usage：明细要写进转写");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 插件命令参与补全与分派：`/plugins` 读回清单，插件命令的结果写进转写。
#[tokio::test]
async fn plugin_commands_complete_and_run_through_core() {
    let (core, connection, mut app, mut receiver, root) = app_with_core().await;
    let dir = root.path().join(".moc/plugins/demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
    std::fs::write(
        dir.join("index.js"),
        r#"
        export default {
          commands: { demo: { description: "示例命令", run: args => `cmd:${args}` } }
        };
        "#,
    )
    .unwrap();

    execute_command(&mut app, &connection, "/plugins".into());
    let mut plugins = None;
    let mut listing = None;
    while plugins.is_none() || listing.is_none() {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("插件清单应在超时前返回")
            .expect("事件队列未关闭");
        match event {
            AppEvent::Plugins(list) => plugins = Some(list),
            AppEvent::PluginOutput { text, error, .. } => {
                assert!(!error, "{text}");
                listing = Some(text);
            }
            _ => {}
        }
    }
    let plugins = plugins.expect("收到清单");
    // 主循环收到清单时会同步命令注册表；这里手动同步一次，不重复跑整个事件循环。
    app.commands.set_plugins(&plugins);
    app.plugins = plugins.clone();
    assert_eq!(plugins[0].name, "demo");
    assert_eq!(plugins[0].commands[0].name, "demo");
    assert!(
        listing.expect("列出插件").contains("命令 /demo"),
        "清单里要列出插件命令"
    );
    // 清单顺带刷新补全表；插件命令与内置命令一起参与前缀匹配。
    app.prompt.insert_str_raw("/demo");
    let completion =
        crate::widgets::completion::CompletionState::derive(&app).expect("插件命令可补全");
    assert_eq!(completion.candidates[0].value, "/demo");

    app.blocks.clear();
    execute_command(&mut app, &connection, "/demo 参数".into());
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("命令结果应在超时前返回")
            .expect("事件队列未关闭");
        if let AppEvent::PluginOutput { name, text, error } = event {
            assert_eq!(name, "demo");
            assert!(!error, "{text}");
            assert_eq!(text, "cmd:参数");
            break;
        }
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// 插件停用通知保留多行原因（JS 调用栈），并附上恢复方式。
#[test]
fn plugin_notice_note_keeps_cause_and_recovery_hint() {
    assert_eq!(
        plugin_notice_note("demo", "E_PLUGIN_DISABLED", "TypeError: x\n  at y"),
        "插件 `demo` 已停用（E_PLUGIN_DISABLED）：TypeError: x\n  at y\n修复后 /plugins reload 可重载"
    );
}

#[tokio::test]
async fn alt_m_cycles_the_session_engine() {
    moc_i18n::set_locale("zh");
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
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
    let session_id = session.id.clone();
    app.session = Some(session);

    handle_key(&mut app, &connection, KeyModifiers::ALT, KeyCode::Char('m'));

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let session = connection
                .client
                .call::<SessionLoad>(SessionLoadParams {
                    omit_auxiliary: false,
                    session_id: session_id.clone(),
                    max_entries: None,
                    read_only: false,
                })
                .await
                .unwrap()
                .session;
            if session.engine == SessionEngine::Ptc {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Alt+M 应切换到 PTC 引擎");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 点图例弹出就地菜单：悬停高亮、点条目切换引擎、再点图例关闭，键位同样可用。
#[tokio::test]
async fn engine_menu_switches_the_session_engine() {
    moc_i18n::set_locale("zh");
    use moc_core::{Core, CoreConfig};

    async fn wait_for_engine(
        connection: &EmbeddedConnection,
        session_id: &str,
        expected: SessionEngine,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let session = connection
                    .client
                    .call::<SessionLoad>(SessionLoadParams {
                        omit_auxiliary: false,
                        session_id: session_id.to_owned(),
                        max_entries: None,
                        read_only: false,
                    })
                    .await
                    .unwrap()
                    .session;
                if session.engine == expected {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("引擎应切换为期望值");
    }

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
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
    let session_id = session.id.clone();
    app.session = Some(session);

    // 图例命中区与菜单区域由绘制回填；这里摆出与渲染一致的坐标。
    app.engine_area = ratatui::layout::Rect::new(60, 20, 4, 1);
    let menu_area = ratatui::layout::Rect::new(44, 14, 24, 5);
    let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // 点图例开菜单：默认高亮当前模式。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
    );
    assert!(app.engine_menu.is_some(), "点模式图例应弹出菜单");
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(0));

    // 悬停在条目上即高亮该条，移出菜单不改选择。
    app.engine_menu.as_mut().unwrap().area = menu_area;
    handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 45, 17));
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
    handle_mouse_with_connection(&mut app, &connection, mouse(MouseEventKind::Moved, 10, 5));
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));

    // 菜单内滚轮与 ↑↓ 同语义循环换高亮；菜单外照旧穿透、菜单不关。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::ScrollDown, 45, 16),
    );
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(0));
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::ScrollUp, 45, 16),
    );
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::ScrollDown, 10, 5),
    );
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
    assert!(app.engine_menu.is_some(), "菜单外滚轮不关菜单");

    // 点第二条（ptc）直达该引擎。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::Down(MouseButton::Left), 45, 16),
    );
    assert!(app.engine_menu.is_none(), "选中条目后菜单关闭");
    wait_for_engine(&connection, &session_id, SessionEngine::Ptc).await;
    // core 通过 `EngineChanged` 推送新引擎，事件循环据此更新本地会话；此处手动对齐。
    app.session.as_mut().unwrap().engine = SessionEngine::Ptc;

    // 再点图例开菜单，再点一次图例应关闭而不是重开。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
    );
    assert!(app.engine_menu.is_some());
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(1));
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
    );
    assert!(app.engine_menu.is_none(), "再次点图例关闭菜单");

    // 键盘路径：开菜单后 ↓ 选到 clm，Enter 应用。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
    );
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(2));
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert!(app.engine_menu.is_none());
    wait_for_engine(&connection, &session_id, SessionEngine::ClmExperimental).await;

    // 普通输入先关掉菜单再照常处理：菜单不会吞键。
    app.session.as_mut().unwrap().engine = SessionEngine::Ptc;
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse(MouseEventKind::Down(MouseButton::Left), 61, 20),
    );
    assert_eq!(app.engine_menu.as_ref().map(|menu| menu.selected), Some(1));
    let revision = app.prompt_revision;
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('h'),
    );
    assert!(app.engine_menu.is_none(), "输入字符只关菜单");
    assert_eq!(app.prompt.text, "h", "字符照常进入输入框");
    assert_ne!(app.prompt_revision, revision);

    connection.shutdown().await;
    core.shutdown().await;
}

/// 补全表里的每条命令都必须有实际处理分支：输入后不得回落到「未知命令」。
///
/// 补全表、帮助与实际处理三者漂移过一次（`/context`、`/quit` 曾在表里但没有分支），
/// 这条用例把「表 ⊆ 处理」钉住。
#[tokio::test]
async fn every_slash_command_has_an_execution_branch() {
    let (core, connection, mut app) = setup_test_app().await;
    for spec in crate::commands::COMMANDS {
        // 逐条重置：模态、设置页、提示、退出标记与忙标记都不该影响下一条
        app.modal = None;
        app.settings = None;
        app.notice = None;
        app.should_quit = false;
        app.inflight_session_ops.clear();

        execute_command(&mut app, &connection, format!("/{}", spec.name));
        let notice = app.notice.clone().unwrap_or_default();
        assert!(
            !notice.starts_with("未知命令"),
            "/{} 没有处理分支（提示：{notice}）",
            spec.name
        );
        if spec.name == "quit" {
            assert!(app.should_quit, "/quit 应走既有退出路径");
        }
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// `/retry` 只重发上下文：不往转写里塞用户块，但占用提示通道。
#[tokio::test]
async fn retry_starts_a_prompt_without_adding_a_user_block() {
    let (core, connection, mut app) = setup_test_app().await;
    let session_id = app.session.as_ref().unwrap().id.clone();
    let user_blocks = app
        .blocks
        .iter()
        .filter(|entry| matches!(entry.block, crate::transcript::Block::User { .. }))
        .count();

    execute_command(&mut app, &connection, "/retry".to_owned());

    assert_eq!(
        app.blocks
            .iter()
            .filter(|entry| matches!(entry.block, crate::transcript::Block::User { .. }))
            .count(),
        user_blocks,
        "重试不得追加用户消息"
    );
    assert!(app.prompt_running, "重试应占用提示通道");
    assert!(app.inflight_prompts.contains(&session_id));

    connection.shutdown().await;
    core.shutdown().await;
}

/// 生成中不能重试：原地提示，不占通道。
#[tokio::test]
async fn retry_is_rejected_while_a_prompt_is_running() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt_running = true;

    execute_command(&mut app, &connection, "/retry".to_owned());

    assert_eq!(
        app.notice.as_deref(),
        Some("正在运行中，暂时不能执行 /retry")
    );
    assert!(app.inflight_prompts.is_empty(), "被拒的重试不得占通道");

    connection.shutdown().await;
    core.shutdown().await;
}

/// `.` 独占整条输入时是 `/retry` 的快捷写法：走同一分派，不追加用户块。
#[tokio::test]
async fn lone_dot_dispatches_retry() {
    let (core, connection, mut app) = setup_test_app().await;
    let session_id = app.session.as_ref().unwrap().id.clone();
    app.prompt.insert_str_raw(".");

    commit_prompt(&mut app, &connection);

    assert!(app.prompt.is_empty(), "`. ` 提交后输入框应清空");
    assert!(app.prompt_running, "`. ` 应占用提示通道");
    assert!(app.inflight_prompts.contains(&session_id));
    assert!(
        !app.blocks
            .iter()
            .any(|entry| matches!(entry.block, crate::transcript::Block::User { .. })),
        "`. ` 不得追加用户消息"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

/// `.` 只在独占整条输入时触发：后面还有内容按普通消息发送。
#[tokio::test]
async fn dot_with_more_text_sends_a_normal_prompt() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_str_raw(".tail");

    commit_prompt(&mut app, &connection);

    assert!(
        app.blocks
            .iter()
            .any(|entry| matches!(entry.block, crate::transcript::Block::User { .. })),
        "`.tail` 应作为普通消息发出"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

/// `.` 在生成中同样被 `/retry` 的运行中门控拦下。
#[tokio::test]
async fn lone_dot_is_rejected_while_running() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt_running = true;
    app.prompt.insert_str_raw(".");

    commit_prompt(&mut app, &connection);

    assert_eq!(
        app.notice.as_deref(),
        Some("正在运行中，暂时不能执行 /retry")
    );
    assert!(app.inflight_prompts.is_empty(), "被拒的 `. ` 不得占通道");

    connection.shutdown().await;
    core.shutdown().await;
}
