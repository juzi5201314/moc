use super::*;

#[test]
fn paged_message_paints_identically_after_scroll_and_resize() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let text = "中文 mixed paragraph words /path/file，标点。 ".repeat(6000);
    assert!(text.len() > 256 * 1024);
    app.push_block(Block::Assistant { text: text.clone() });
    let mut view = ScrollbackView::new();
    for width in [41, 83] {
        let mut full: Vec<_> = render_markdown_with(
            &text,
            prose_width(width).max(8),
            app.theme,
            RenderViews::default(),
        )
        .lines
        .into_iter()
        .map(|line| hang(line, PROSE_INDENT, usize::MAX))
        .collect();
        full.push(Line::default());
        view.sync(&app, width as u16);
        for from in [0, full.len() / 2, full.len().saturating_sub(20), 0] {
            let window = view.ensure_visible(&app, 20, full.len().saturating_sub(20 + from));
            assert_eq!(window.skip_rows, from);
            let rect = Rect::new(0, 0, width as u16, 20);
            let mut actual = Buffer::empty(rect);
            let mut expected = Buffer::empty(rect);
            let (mut hits, mut charts, mut links, mut slots, mut lines) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
            view.paint(
                &mut actual,
                rect,
                window.skip_rows,
                true,
                app.theme,
                None,
                None,
                PaintSinks {
                    hits: &mut hits,
                    charts: &mut charts,
                    links: &mut links,
                    slots: &mut slots,
                    lines: &mut lines,
                },
            );
            for (row, line) in full.iter().skip(from).take(20).enumerate() {
                expected.set_line(0, row as u16, line, width as u16);
            }
            assert_eq!(actual, expected);
            assert!(view.cache[0].lines.len() <= 276);
        }
        view.trim_with_budget(full.len(), 1, 0);
        assert!(view.cache[0].paged.is_none());
        assert!(!view.cache[0].rendered);
        let window = view.ensure_visible(&app, 20, 0);
        assert_eq!(window.skip_rows, full.len() - 20);
        let cached = &view.cache[0];
        assert_eq!(
            &cached.lines[window.skip_rows - cached.line_start..],
            &full[window.skip_rows..]
        );
    }
}

#[tokio::test]
async fn background_layout_coalesces_without_losing_final_or_reusing_old_blocks() {
    use crate::protocol::AppEvent;
    use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
    async fn ready(events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    events.recv().await.expect("布局事件通道"),
                    AppEvent::LayoutReady
                ) {
                    break;
                }
            }
        })
        .await
        .expect("轻量布局应及时完成");
    }
    fn allow_render(view: &mut ScrollbackView) {
        view.cache[0].rendered_at = std::time::Instant::now() - std::time::Duration::from_secs(1);
    }
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.prompt_running = true;
    app.blocks.push(Entry::new(Block::Assistant {
        text: "Alpha [late] **正文** and `code`.\n\n".repeat(2200),
    }));
    let mut view = ScrollbackView::background();
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    });
    view.sync(&app, 80);
    app.blocks[0].push_text("\n\n| 项 | 值 |\n|---|---|\n| A | 1 |\n| B | 2 |\n");
    let first = app.blocks[0].clone();
    allow_render(&mut view);
    view.sync(&app, 80);
    assert!(view.pending_layout.is_some());
    assert!(!view.has_deferred_render(), "等待后台结果不忙画空帧");
    // 后加的引用定义会改变前文的渲染，不能用盲目的文本尾部拼接替代原 parser。
    app.blocks[0].push_text("\n[late]: https://example.com\n\n新的末尾");
    ready(&mut receiver).await;
    view.sync(&app, 80);
    assert_eq!(
        view.cache[0].key.0, first.generation,
        "追加期间仍能发布完成的中间帧"
    );
    let Block::Assistant { text } = &first.block else {
        unreachable!()
    };
    assert_eq!(
        view.cache[0].lines,
        assistant_message(text, &first, &app, 80, true).lines
    );
    allow_render(&mut view);
    view.sync(&app, 80);
    ready(&mut receiver).await;
    view.sync(&app, 80);
    let expected = render_entry(0, &app.blocks[0], &app, 80);
    assert_eq!(view.cache[0].key.0, app.blocks[0].generation);
    assert_eq!(view.cache[0].lines, expected.lines);
    assert_eq!(
        format!("{:?}", view.cache[0].tables),
        format!("{:?}", expected.tables)
    );
    assert!(
        view.cache[0].tables[0].button.is_some(),
        "表格落定后的交互入口不能丢失"
    );

    app.blocks[0].push_text("最终尾部");
    allow_render(&mut view);
    view.sync(&app, 80);
    assert!(view.pending_layout.is_some());
    app.blocks[0].stamp_prompt(1_700_000_000_000, 2000);
    app.prompt_running = false;
    view.sync(&app, 80);
    let final_key = view.cache[0].key;
    assert_eq!(
        view.cache[0].lines,
        render_entry(0, &app.blocks[0], &app, 80).lines
    );
    assert!(!view.has_deferred_render());
    ready(&mut receiver).await;
    view.sync(&app, 80);
    assert_eq!(view.cache[0].key, final_key, "迟到流式结果不能覆盖终态");

    app.prompt_running = true;
    app.blocks[0].elapsed_ms = None;
    app.blocks[0].touch();
    view.sync(&app, 80);
    app.blocks[0].push_text("旧会话排队内容");
    allow_render(&mut view);
    view.sync(&app, 80);
    assert!(view.pending_layout.is_some());
    app.blocks = vec![Entry::new(Block::Assistant {
        text: "新的会话".into(),
    })];
    view.sync(&app, 80);
    ready(&mut receiver).await;
    view.sync(&app, 80);
    assert_eq!(
        view.cache[0].lines,
        render_entry(0, &app.blocks[0], &app, 80).lines
    );
    assert!(view.pending_layout.is_none());
}

/// C6：懒渲染与绘制共用同一份渲染后高度累计。
///
/// 估算高度只影响滚动范围，随时会被真实渲染修正（工具结果按全文估高、渲染却
/// 截断到 [`TOOL_RESULT_TAIL_ROWS`] 行）。若绘制仍按渲染前的累计取起点，向上滚进
/// 懒区时就会切进只存估算高度的空块。用例同时钉住契约：视口覆盖的块必须已渲染。
#[test]
fn lazy_paint_never_slices_unrendered_blocks() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let result = (0..200)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    // 70 个工具块：只有末尾 64 块进 eager 渲染，前面几块只留估算高度。
    for index in 0..70 {
        let id = format!("call_{index}");
        let arguments = r#"{"command":"run"}"#.to_owned();
        app.start_tool_block(ToolCallStart {
            id: id.clone(),
            name: "bash".into(),
            parent_id: None,
            arguments: arguments.clone(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: id.clone(),
            name: "bash".into(),
            parent_id: None,
            arguments,
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: id,
                content: result.clone(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: None,
        });
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    let mut view = ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("首帧绘制成功");
    let viewport = usize::from(app.scrollback_area.height);
    assert!(viewport > 0, "测试终端应有转写区");

    // 向上滚进懒区：绘制起点落在首个懒块的估算高度范围内。
    let estimate = view.cache[0].height;
    app.scroll_offset = app.scroll_max_offset.saturating_sub(estimate / 2);
    app.selection = Some(((0, 0), (0, 0)));
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("懒区绘制不得越界切片");

    let skip_rows = app.scroll_max_offset - app.scroll_offset;
    let end = skip_rows.saturating_add(viewport);
    let mut offset = 0usize;
    let mut covered = 0usize;
    for cached in &view.cache {
        let next = offset.saturating_add(cached.height);
        if next > skip_rows && offset < end {
            assert!(cached.rendered, "视口覆盖的块必须已渲染");
            assert_eq!(cached.lines.len(), cached.height, "渲染高度必须等于行数");
            covered += 1;
        } else {
            assert_eq!(
                cached.lines.capacity(),
                0,
                "离屏缓存不得保留整条消息的行数组"
            );
            assert_eq!(cached.tables.capacity(), 0);
            assert_eq!(cached.images.capacity(), 0);
        }
        offset = next;
    }
    assert!(covered > 0, "视口应落在至少一个块上");

    app.blocks = vec![Entry::new(Block::Assistant {
        text: "x".repeat(65537),
    })];
    app.prompt_running = true;
    let mut streaming = ScrollbackView::new();
    streaming.sync(&app, 80);
    let old_key = streaming.cache[0].key;
    streaming.cache[0].render_cost = std::time::Duration::from_millis(100);
    streaming.cache[0].rendered_at = std::time::Instant::now() + std::time::Duration::from_secs(60);
    app.blocks[0].push_text("最后一段");
    streaming.sync(&app, 80);
    assert!(streaming.has_deferred_render());
    assert_eq!(streaming.cache[0].key, old_key);
    app.prompt_running = false;
    streaming.sync(&app, 80);
    assert!(!streaming.has_deferred_render(), "终态不得延迟");
    assert_eq!(streaming.cache[0].key.0, app.blocks[0].generation);
    assert!(
        streaming.cache[0]
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<String>()
            .contains("最后一段")
    );
}

/// 含表格的正文：估算高度必须等于真实渲染高度，切换成图表后依然成立。
///
/// 表格列宽收缩、入口按钮、图表占位行都只在渲染期算得出来，任何一处
/// 走「折行近似」都会让滚动范围与懒渲染窗口一起失真。
#[test]
fn table_blocks_estimate_exactly_like_render() {
    use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    app.scrollback_area = ratatui::layout::Rect::new(3, 0, 80, 24);
    let text = "结论如下：\n\n| 项 | 耗时 | 内存 |\n|---|---|---|\n| a | 10 ms | 2 MiB |\n| b | 20 ms | 4 MiB |\n\n仅供参考。";
    app.push_block(Block::Assistant { text: text.into() });

    let rendered = render_entry(0, &app.blocks[0], &app, width);
    assert!(
        rendered
            .lines
            .iter()
            .all(|line| line.width() <= usize::from(width)),
        "表格行不得超出转写区宽度"
    );
    let heights = |app: &App| {
        (
            estimate_entry_height_at(app, 0, width),
            render_entry(0, &app.blocks[0], app, width).lines.len(),
        )
    };
    let (estimate, rendered) = heights(&app);
    assert_eq!(estimate, rendered, "表格块：估算与渲染必须同源");
    assert!(
        rendered > wrapped_rows(text, usize::from(width)),
        "表格实际比折行近似更高（边框行）"
    );

    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    });
    let (estimate, rendered) = heights(&app);
    assert_eq!(estimate, rendered, "支持图形时（多出入侵按钮行）也必须同源");

    app.toggle_table_chart(0, 0);
    assert!(app.blocks[0].table_chart(0).is_some());
    let (estimate, rendered) = heights(&app);
    assert_eq!(estimate, rendered, "切换成图表后（占位行）也必须同源");
}

/// M25：懒块的估算高度与渲染同规则。
///
/// 估算按全文计高（工具结果 2000 行即 2000 行、折叠态也按展开算）时，滚动范围、
/// 滚动条与懒渲染的可见区间都失真一个数量级。
#[test]
fn estimates_follow_render_rules() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let long_result = (0..200)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");

    app.register_tool_batch("batch".into(), ["call_a".to_owned(), "call_b".to_owned()]);
    app.start_tool_block(ToolCallStart {
        id: "call_bash".into(),
        name: "bash".into(),
        parent_id: None,
        arguments: r#"{"command":"print lines"}"#.into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "call_a".into(),
        name: "read".into(),
        parent_id: None,
        arguments: r#"{"path":"a.rs"}"#.into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "call_b".into(),
        name: "read".into(),
        parent_id: None,
        arguments: r#"{"path":"b.rs"}"#.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_bash".into(),
        name: "bash".into(),
        parent_id: None,
        arguments: r#"{"command":"print lines"}"#.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_bash".into(),
            content: long_result,
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(42),
        diff: None,
    });
    run_tool(
        &mut app,
        "call_todo",
        "todo",
        r#"{"op":"init","list":[{"phase":"阶段","items":["很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述"]}]}"#,
        ToolStatus::Completed,
        "阶段\n  [ ] 很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述\n",
    );
    // execute：三档 + 失败的估高都要与渲染同源
    run_tool(
        &mut app,
        "call_rc",
        "execute",
        &serde_json::json!({"description":"核对","code":"const a = 1;\nawait tools.bash({command:'ls'})"}).to_string(),
        ToolStatus::Completed,
        r#"[{"status":"fulfilled","value":{"content":" M a.rs\n M b.rs","isError":false}}]"#,
    );
    run_tool(
        &mut app,
        "call_rc_bad",
        "execute",
        r#"{"code":"throw 1"}"#,
        ToolStatus::Error,
        "脚本执行失败",
    );
    // 嵌套调用树：execute 带子调用，兄弟连线进估高
    app.start_tool_block(ToolCallStart {
        id: "call_parent".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: r#"{"description":"父"}"#.into(),
    });
    for (id, summary) in [("call_child1", "one"), ("call_child2", "two")] {
        app.start_tool_block(ToolCallStart {
            id: id.into(),
            name: "bash".into(),
            parent_id: Some("call_parent".into()),
            arguments: format!(r#"{{"description":"{summary}","command":"echo {summary}"}}"#),
        });
        app.finish_tool_block(ToolCallFinish {
            id: id.into(),
            name: "bash".into(),
            parent_id: Some("call_parent".into()),
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: id.into(),
                content: "done".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(3),
            diff: None,
        });
    }
    app.finish_tool_block(ToolCallFinish {
        id: "call_parent".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: String::new(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_parent".into(),
            content: "[]".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(9),
        diff: None,
    });
    // todo 多阶段：简略档折叠已关闭阶段与历史已完成任务
    run_tool(
        &mut app,
        "call_todo2",
        "todo",
        r#"{"op":"start","task":"乙"}"#,
        ToolStatus::Completed,
        "一\n  [x] 甲\n  [-] 弃\n二\n  [x] 丙\n  [/] 乙\n  [ ] 丁\n",
    );

    let rendered = |app: &App, index: usize| {
        render_entry(index, &app.blocks[index], app, width)
            .lines
            .len()
    };
    for index in 0..app.blocks.len() {
        assert_eq!(
            estimate_entry_height_at(&app, index, width),
            rendered(&app, index),
            "第 {index} 块的估算应与渲染一致"
        );
    }

    // 三档下估算与渲染都必须同源（隐藏只剩标题行、简略是预览、完整是全文）。
    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, width),
                rendered(&app, index),
                "{level:?} 档第 {index} 块的估算应与渲染一致"
            );
        }
    }
    app.tool_detail = DetailLevel::Hidden;
    assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        1,
        "隐藏档只占标题一行"
    );
    app.tool_detail = DetailLevel::Brief;
    assert!(
        estimate_entry_height_at(&app, 0, width) < 30,
        "简略档不得按全文估算"
    );
    app.tool_detail = DetailLevel::Full;
    assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
    app.blocks[0].toggle_level(app.tool_detail);
    assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
}

/// 批次代次一遍算清：所有成员共用布局代次，非批次块保留自身代次。
/// 精确成员序列比较避免漏刷新，整帧工作量仍为 O(块数)。
#[test]
fn batch_members_share_one_generation_scan() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.register_tool_batch(
        "batch".into(),
        [
            "call_a".to_owned(),
            "call_b".to_owned(),
            "call_c".to_owned(),
        ],
    );
    for id in ["call_a", "call_b", "call_c"] {
        app.start_tool_block(ToolCallStart {
            id: id.into(),
            name: "read".into(),
            parent_id: None,
            arguments: r#"{"path":"a.rs"}"#.into(),
        });
    }
    app.push_note("独立块".into(), crate::transcript::NoteTone::Info);
    // 只有第二个成员继续变化：全批仍须使用同一个失效标记。
    app.tool_entry_mut("call_b").expect("批次成员").touch();
    let latest = app.blocks[1].generation;
    assert!(latest > app.blocks[0].generation, "成员各自推进");

    let mut generations = Vec::new();
    fill_block_generations(&app.blocks, &mut generations);
    assert_eq!(generations.len(), app.blocks.len());
    assert_eq!(
        &generations[..3],
        &[generations[0], generations[0], generations[0]],
        "同批成员共用布局代次"
    );
    assert_eq!(
        generations[3], app.blocks[3].generation,
        "非批次块保留自身代次"
    );

    // 缓存 key 用的就是这份代次
    let mut view = ScrollbackView::new();
    view.sync(&app, 80);
    let key = view.cache[0].key;
    assert_eq!(key, (view.generations[0], app.view_gen));
    assert_eq!(view.cache[1].key.0, key.0);
    view.sync(&app, 80);
    assert_eq!(view.cache[0].key, key, "无成员变化时布局代次稳定");
}

#[test]
fn result_card_folding_keeps_report_and_scroll_height_in_sync() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::AgentResult {
        meta: moc_protocol::methods::AgentResultMeta {
            agent_id: "ses_child".into(),
            title: "探查者".into(),
            status: moc_protocol::methods::AgentStatus::Completed,
        },
        body: "第一行\n第二行\n第三行\n报告末尾".into(),
    });
    let width = 24;
    let text =
        |lines: &[Line<'static>]| lines.iter().map(plain_text).collect::<Vec<_>>().join("\n");
    let preview = render_entry(0, &app.blocks[0], &app, width).lines;
    assert!(text(&preview).contains("第三行"));
    assert!(!text(&preview).contains("报告末尾"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), preview.len());
    app.blocks[0].toggle_level(DetailLevel::Brief);
    let full = render_entry(0, &app.blocks[0], &app, width).lines;
    assert!(text(&full).contains("报告末尾"));
    assert!(full.iter().all(|line| line.width() <= usize::from(width)));
    assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());
    app.cycle_tool_detail();
    let hidden = render_entry(0, &app.blocks[0], &app, width).lines;
    assert!(!text(&hidden).contains("第一行"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), hidden.len());
    app.cycle_tool_detail();
    assert!(!text(&render_entry(0, &app.blocks[0], &app, width).lines).contains("报告末尾"));
}

/// 工具结果里的图片：正文多出的行数估算与渲染必须一致。
#[test]
fn tool_result_images_keep_estimate_and_render_in_sync() {
    use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
    use crate::render::image::{DecodedImage, ImageKey};
    use crate::transcript::ToolImage;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    });
    let key = ImageKey::from_bytes("image/png", "AAAA");
    app.images.complete(
        key.clone(),
        Ok(DecodedImage::Raster(image::RgbaImage::from_pixel(
            40,
            20,
            image::Rgba([9, 9, 9, 255]),
        ))),
    );
    app.push_block(Block::Tool(Box::new(ToolCallBlock {
        id: "shot".into(),
        batch_id: None,
        parent_id: None,
        name: "screenshot".into(),
        arguments: "{}".into(),
        status: Some(moc_protocol::methods::ToolStatus::Completed),
        result: None,
        diff: None,
        duration_ms: None,
        running: false,
        progress: None,
        images: vec![ToolImage::for_test(key, "image/png", Some("AAAA"))],
        job_id: None,
        live_output: None,
        preview: None,
    })));

    let width = 80u16;
    let rendered = render_entry(0, &app.blocks[0], &app, width);
    assert_eq!(rendered.images.len(), 1, "一张图一张落位");
    let placement = &rendered.images[0];
    let prose_col = u16::try_from(PROSE_INDENT).expect("正文列应放得进 u16");
    assert_eq!(placement.x, prose_col, "图片落在正文列");
    assert!(
        placement.row + usize::from(placement.rows) <= rendered.lines.len(),
        "占位行必须真的存在"
    );
    assert!(rendered.lines[placement.row].width() >= usize::from(placement.cols));
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        rendered.lines.len(),
        "图片块：估算与渲染必须同源"
    );
}

/// 绘制登记：图片槽位落在正文前缀上；比视口宽时不登记（会盖住边框）。
#[test]
fn paint_registers_tool_image_slots_within_the_viewport() {
    use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
    use crate::render::image::{DecodedImage, ImageKey};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    });
    let key = ImageKey::from_bytes("image/png", "AAAA");
    app.images.complete(
        key.clone(),
        Ok(DecodedImage::Raster(image::RgbaImage::from_pixel(
            40,
            20,
            image::Rgba([9, 9, 9, 255]),
        ))),
    );
    app.push_block(Block::Tool(Box::new(ToolCallBlock {
        id: "shot".into(),
        batch_id: None,
        parent_id: None,
        name: "screenshot".into(),
        arguments: "{}".into(),
        status: Some(moc_protocol::methods::ToolStatus::Completed),
        result: None,
        diff: None,
        duration_ms: None,
        running: false,
        progress: None,
        images: vec![crate::transcript::ToolImage::for_test(
            key,
            "image/png",
            Some("AAAA"),
        )],
        job_id: None,
        live_output: None,
        preview: None,
    })));

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("测试终端");
    let mut view = ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("绘制成功");
    let slot = app.image_slots.first().expect("图片应登记槽位").clone();
    assert!(matches!(slot.source, SlotSource::Image(_)));
    let prose_col = u16::try_from(PROSE_INDENT).expect("正文列应放得进 u16");
    assert_eq!(slot.x, app.scrollback_area.x + prose_col, "落位对齐正文列");
    assert_eq!(slot.cols, 5, "40px 宽 ÷ 8px 单元格");
    assert_eq!(slot.rows, 2, "20px 高 ÷ 18px 单元格，向上取整");
    assert!(slot.placement_id != 0, "kitty 放置 ID 不得为零");

    // 视口比占位区窄：整块放不下就不登记，免得盖住边框
    let mut narrow = ScrollbackView::new();
    narrow.width = 100;
    narrow.cache = vec![CachedEntry {
        key: (0, 0),
        layout_epoch: 0,
        layout: TreeIndex::EMPTY,
        height: 3,
        paged: None,
        footer: Vec::new(),
        line_start: 0,
        lines: vec![
            Line::from("标题"),
            Line::from(" ".repeat(40)),
            Line::from(" ".repeat(40)),
        ],
        rendered: true,
        rendered_at: std::time::Instant::now(),
        render_cost: std::time::Duration::ZERO,
        tables: Vec::new(),
        links_button: None,
        images: vec![ImagePlacement {
            row: 1,
            x: 2,
            cols: 40,
            rows: 2,
            px: (320, 36),
            key: ImageKey::from_bytes("image/png", "zzz"),
        }],
    }];
    let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));
    let mut slots = Vec::new();
    let mut hits = Vec::new();
    let mut charts = Vec::new();
    let mut links = Vec::new();
    let mut lines = Vec::new();
    narrow.paint(
        &mut buffer,
        Rect::new(0, 0, 20, 10),
        0,
        false,
        Theme::global(),
        None,
        None,
        PaintSinks {
            hits: &mut hits,
            charts: &mut charts,
            links: &mut links,
            slots: &mut slots,
            lines: &mut lines,
        },
    );
    assert!(slots.is_empty(), "占位区超出视口宽度时不登记");
}

#[test]
fn distant_graphics_in_large_messages_stay_outside_the_viewport() {
    let mut view = ScrollbackView::new();
    view.cache.push(CachedEntry {
        key: (0, 0),
        layout_epoch: 0,
        layout: TreeIndex::EMPTY,
        height: 100_010,
        lines: vec![Line::from("可见正文")],
        paged: None,
        footer: Vec::new(),
        line_start: 0,
        rendered: true,
        rendered_at: std::time::Instant::now(),
        render_cost: std::time::Duration::ZERO,
        tables: vec![LocalTable {
            table: 0,
            button: Some(ButtonSpan {
                row: 100_000,
                x: 0,
                w: 10,
            }),
            chart: Some(ChartPlacement {
                row: 100_001,
                cols: 5,
                rows: 2,
                px: (40, 36),
            }),
        }],
        links_button: None,
        images: vec![ImagePlacement {
            row: 100_004,
            x: 0,
            cols: 5,
            rows: 2,
            px: (40, 36),
            key: crate::render::image::ImageKey::from_bytes("image/png", "distant"),
        }],
    });
    let area = Rect::new(0, 5, 80, 10);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
    let (mut hits, mut charts, mut links, mut slots, mut lines) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    view.paint(
        &mut buffer,
        area,
        0,
        true,
        Theme::global(),
        None,
        None,
        PaintSinks {
            hits: &mut hits,
            charts: &mut charts,
            links: &mut links,
            slots: &mut slots,
            lines: &mut lines,
        },
    );
    assert!(charts.is_empty());
    assert!(slots.is_empty());
    assert_eq!(lines, [(5, "可见正文".to_owned())]);
}

/// 缓存 key 含树形状：父块从 running 变结束后，末个子块必须重渲染成 └─。
#[test]
fn parent_finish_recolors_last_child_via_cached_shape() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.start_tool_block(ToolCallStart {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: r#"{"description":"父"}"#.into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "c".into(),
        name: "bash".into(),
        parent_id: Some("p".into()),
        arguments: r#"{"command":"echo c"}"#.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "c".into(),
        name: "bash".into(),
        parent_id: Some("p".into()),
        arguments: String::new(),
        status: ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: "c".into(),
            content: "out".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(1),
        diff: None,
    });
    let mut view = ScrollbackView::new();
    view.sync(&app, 80);
    assert!(
        plain(&view.cache[1].lines[0]).starts_with(" ├─"),
        "父运行中：{:?}",
        plain(&view.cache[1].lines[0])
    );
    app.finish_tool_block(ToolCallFinish {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: String::new(),
        status: ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: "p".into(),
            content: "[]".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(9),
        diff: None,
    });
    // 子块自身内容代次没变，形状变化也要让缓存失效。
    view.sync(&app, 80);
    assert!(
        plain(&view.cache[1].lines[0]).starts_with(" └─"),
        "父结束重渲染：{:?}",
        plain(&view.cache[1].lines[0])
    );
}

/// 回归：用户消息与系统提示的估高必须与渲染完全一致（含首块与时间戳换行）。
#[test]
fn user_and_note_estimates_match_render() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::User { text: "hi".into() });
    app.push_block(Block::User {
        text: "一二三四五六七八九十".repeat(6),
    });
    app.push_block(Block::Note {
        text: "压缩完成".repeat(20),
        tone: crate::transcript::NoteTone::Info,
    });
    for width in [24u16, 60, 120] {
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, width),
                render_entry(index, &app.blocks[index], &app, width)
                    .lines
                    .len(),
                "宽 {width} 第 {index} 块"
            );
        }
    }
}

#[test]
fn long_line_notices_group_expand_and_deduplicate_after_parent_finishes() {
    use moc_protocol::methods::ToolStatus;
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let notices = (100..116)
        .map(|n| {
            format!(
                "[Line {n} is 9 KB; content omitted; read with offset={n}, limit=1, byte_offset=0]"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    app.start_tool_block(ToolCallStart {
        id: "parent".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: "{}".into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "child".into(),
        name: "read".into(),
        parent_id: Some("parent".into()),
        arguments: "{}".into(),
    });
    let finish = |id: &str, name: &str, parent: Option<&str>| ToolCallFinish {
        id: id.into(),
        name: name.into(),
        parent_id: parent.map(str::to_owned),
        arguments: "{}".into(),
        status: ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: id.into(),
            content: notices.clone(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(1),
        diff: None,
    };
    app.finish_tool_block(finish("child", "read", Some("parent")));
    let child = app.tool_block("child").unwrap();
    let compact = super::super::notices::rows(child, &app, 120, DetailLevel::Brief);
    assert_eq!(compact.len(), 1, "16 条同类通知必须合并成一行");
    let full = super::super::notices::rows(child, &app, 120, DetailLevel::Full)
        .iter()
        .map(plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        full.contains("Line 100")
            && full.contains("Line 115")
            && full.contains("单行过长时仅展示部分预览")
    );
    let mut view = ScrollbackView::new();
    view.sync(&app, 120);
    let before = view.cache[1].lines.iter().map(plain).collect::<Vec<_>>();
    app.finish_tool_block(finish("parent", "execute", None));
    view.sync(&app, 120);
    assert!(
        super::super::notices::rows(
            app.tool_block("child").unwrap(),
            &app,
            120,
            DetailLevel::Brief
        )
        .is_empty()
    );
    assert_eq!(
        super::super::notices::rows(
            app.tool_block("parent").unwrap(),
            &app,
            120,
            DetailLevel::Brief
        )
        .len(),
        1
    );
    assert!(before.iter().any(|line| line.contains("16")));
    assert!(
        !view.cache[1]
            .lines
            .iter()
            .map(plain)
            .any(|line| line.contains("16")),
        "父结果落地后不能沿用子块的旧通知缓存"
    );
}
