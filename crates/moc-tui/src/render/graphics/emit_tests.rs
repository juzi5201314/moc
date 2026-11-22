use super::*;

use ratatui::layout::Rect;

use crate::app::ImageSlot;
use crate::transcript::Block;

/// 等候后台图表编码完成并回填到 `App`。
fn settle_chart(
    app: &mut App,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<crate::protocol::AppEvent>,
) {
    for _ in 0..1000 {
        while let Ok(event) = receiver.try_recv() {
            if let crate::protocol::AppEvent::ChartEncoded {
                table,
                image_id,
                px,
                payload,
            } = event
            {
                app.complete_chart_encode(table, image_id, px, payload);
            }
        }
        if !app.has_rendering_charts()
            && app
                .blocks
                .first()
                .and_then(|block| block.table_chart(0))
                .is_some_and(|state| state.payload.is_some())
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("图表未在预算内完成后台编码");
}

/// 造一个「已展开并完成后台编码」的 app，并给出对应的槽位。
fn app_with_chart(proto: GraphicsProto) -> (App, ImageSlot) {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.graphics = Some(GraphicsSupport {
        proto,
        cell: CellPixels { w: 8, h: 18 },
    });
    app.scrollback_area = Rect::new(3, 0, 74, 40);
    app.push_block(Block::Assistant {
        text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
    });
    app.toggle_table_chart(0, 0);
    let state = app.blocks[0].table_chart(0).expect("图表态已建立");
    let rows = crate::render::chart::rows_needed(
        state.spec.kind,
        state.spec.categories.len(),
        state.spec.series.len(),
        71 * 8,
    );
    let slot = ImageSlot {
        entry: 0,
        source: SlotSource::Chart { table: 0 },
        x: 6,
        y: 5,
        cols: 71,
        rows,
        px: (71 * 8, u32::from(rows) * 18),
        hash: 0xabcd,
        placement_id: 0x1234,
    };
    app.image_slots = vec![slot.clone()];
    settle_chart(&mut app, &mut receiver);
    (app, slot)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 首次贴图：先定位光标，再写协议载荷；位置与指纹不变时一个字都不写。
#[test]
fn emit_places_once_and_skips_unchanged_frames() {
    let (mut app, slot) = app_with_chart(GraphicsProto::Sixel);
    let mut out = Vec::new();
    emit(&mut app, &mut out).expect("写内存不会失败");
    assert!(
        out.starts_with(format!("\x1b[{};{}H", slot.y + 1, slot.x + 1).as_bytes()),
        "贴图前先定位到格子左上角"
    );
    assert!(contains(&out, b"\x1bP0;1q"), "sixel 载荷应已写出");
    assert!(contains(&out, b"\x1b\\"), "sixel 结尾 ST");
    assert_eq!(app.image_placed.len(), 1);

    let mut again = Vec::new();
    emit(&mut app, &mut again).expect("写内存不会失败");
    assert!(again.is_empty(), "位置与指纹都没变，不该重发：{again:?}");

    // 滚动一格：位置变化必须重发；纯移动不触发整屏清屏，避免滚动卡顿
    app.image_slots[0].y = slot.y + 1;
    let mut moved = Vec::new();
    emit(&mut app, &mut moved).expect("写内存不会失败");
    assert!(contains(&moved, b"\x1bP0;1q"), "换位后应重发载荷");
    assert!(!app.image_repaint, "纯换位不需要清屏重绘");
}

/// kitty：载荷只传一次，换位只重发放置；图像消失时删除放置。
#[test]
fn kitty_transmits_once_and_deletes_stale_placements() {
    let (mut app, _slot) = app_with_chart(GraphicsProto::Kitty);
    let mut out = Vec::new();
    emit(&mut app, &mut out).expect("写内存不会失败");
    assert!(contains(&out, b"a=t"), "先传输数据");
    assert!(contains(&out, b"a=p"), "再放置图像");
    let transmitted = out.clone();

    app.image_slots[0].y += 2;
    let mut moved = Vec::new();
    emit(&mut app, &mut moved).expect("写内存不会失败");
    assert!(
        // 按 image id + placement id 精确删除；`d=p` 是按单元格删，命中不了
        contains(&moved, b"a=d,d=i,i=1,p=4660"),
        "换位先删旧放置：{:?}",
        String::from_utf8_lossy(&moved)
    );
    assert!(contains(&moved, b"a=p"), "再贴新位置");
    assert!(
        !contains(&moved, b"a=t"),
        "像素数据不重传（载荷已驻留在终端里）"
    );
    assert!(!transmitted.is_empty());

    // 图像离开视口：连图像数据与放置一起删
    let saved_slot = app.image_slots[0].clone();
    app.image_slots.clear();
    let mut gone = Vec::new();
    emit(&mut app, &mut gone).expect("写内存不会失败");
    assert!(
        contains(&gone, b"a=d,d=i"),
        "消失的图像要按 ID 删除数据与放置"
    );
    assert!(app.image_placed.is_empty());
    assert!(!app.image_repaint, "kitty 有显式删除，不需要清屏");

    // 滚回视口：已删除的图表必须重新传输像素数据并放置
    app.image_slots.push(saved_slot);
    let mut returned = Vec::new();
    emit(&mut app, &mut returned).expect("写内存不会失败");
    assert!(contains(&returned, b"a=t"), "滚回视口后应重传像素数据");
    assert!(contains(&returned, b"a=p"), "滚回视口后应重新放置图像");
}

/// 浮层期间不贴图，浮层消失后整批重放（避免图像盖住模态或残影）。
#[test]
fn overlays_suspend_and_resume_placement() {
    let (mut app, slot) = app_with_chart(GraphicsProto::Sixel);
    let mut first = Vec::new();
    emit(&mut app, &mut first).expect("写内存不会失败");
    assert!(contains(&first, b"\x1bP0;1q"), "先正常贴一次");
    app.notice = Some("已附加剪贴板图片".into());
    app.notice_at = Some(std::time::Instant::now());
    let mut covered = Vec::new();
    emit(&mut app, &mut covered).expect("写内存不会失败");
    assert!(
        !contains(&covered, b"\x1bP0;1q"),
        "浮层期间不得再贴图：{:?}",
        String::from_utf8_lossy(&covered)
    );

    app.notice = None;
    app.image_slots = vec![slot];
    let mut resumed = Vec::new();
    emit(&mut app, &mut resumed).expect("写内存不会失败");
    assert!(contains(&resumed, b"\x1bP0;1q"), "浮层消失后重放");
}

/// 浮层出现时 kitty 的图必须被撤下来：图像是独立图层，默认画在文字之上，
/// 「不放置」不等于「撤下来」，不删就会直接盖住模态框。
#[test]
fn kitty_placements_are_hidden_when_an_overlay_appears() {
    let (mut app, _slot) = app_with_chart(GraphicsProto::Kitty);
    let mut first = Vec::new();
    emit(&mut app, &mut first).expect("写内存不会失败");
    assert_eq!(app.image_placed.len(), 1);

    app.notice = Some("操作反馈".into());
    app.notice_at = Some(std::time::Instant::now());
    let mut covered = Vec::new();
    emit(&mut app, &mut covered).expect("写内存不会失败");
    assert!(
        contains(&covered, b"a=d,d=i"),
        "浮层出现时要撤下图：{:?}",
        String::from_utf8_lossy(&covered)
    );
    assert!(app.image_placed.is_empty(), "放置记录已随删除作废");
}

/// 浮层期间图表被还原：恢复时要按 ID 删掉它的 kitty 放置（不留残影）。
#[test]
fn placements_removed_under_an_overlay_are_deleted() {
    let (mut app, _slot) = app_with_chart(GraphicsProto::Kitty);
    let mut first = Vec::new();
    emit(&mut app, &mut first).expect("写内存不会失败");

    app.notice = Some("操作反馈".into());
    app.notice_at = Some(std::time::Instant::now());
    let mut covered = Vec::new();
    emit(&mut app, &mut covered).expect("写内存不会失败");
    // 浮层出现的第一帧就把放置撤下来，避免图像盖住模态
    assert!(
        contains(&covered, b"a=d,d=i"),
        "{:?}",
        String::from_utf8_lossy(&covered)
    );

    // 浮层期间用户把图表还原成表格，浮层随后消失
    app.blocks[0].set_table_chart(0, None);
    app.image_slots.clear();
    app.notice = None;
    let mut resumed = Vec::new();
    emit(&mut app, &mut resumed).expect("写内存不会失败");
    assert!(
        !contains(&resumed, b"a=p"),
        "图已经撤过，恢复时不该再贴：{:?}",
        String::from_utf8_lossy(&resumed)
    );
    assert!(app.image_placed.is_empty());
}

/// sixel / iTerm2 没有删除动作：图消失时只能请求下一帧清屏重绘。
#[test]
fn non_kitty_removal_requests_full_repaint() {
    let (mut app, _slot) = app_with_chart(GraphicsProto::Sixel);
    let mut first = Vec::new();
    emit(&mut app, &mut first).expect("写内存不会失败");
    assert!(!app.image_repaint);

    app.image_slots.clear();
    let mut gone = Vec::new();
    emit(&mut app, &mut gone).expect("写内存不会失败");
    assert!(app.image_repaint, "sixel 图消失后必须请求清屏重绘");
    assert!(app.image_placed.is_empty());
}

/// 没有图形能力时 emit 完全静默（表格形态的终端不该收到任何控制序列）。
#[test]
fn emit_is_silent_without_graphics_support() {
    let (mut app, _slot) = app_with_chart(GraphicsProto::Sixel);
    app.graphics = None;
    let mut out = Vec::new();
    emit(&mut app, &mut out).expect("写内存不会失败");
    assert!(out.is_empty());
}

/// 异步渲染：渲染中 emit 立即返回且保持加载态，取消后过期回执被安全丢弃。
#[test]
fn async_chart_renders_without_blocking_and_drops_stale_results() {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Sixel,
        cell: CellPixels { w: 8, h: 18 },
    });
    app.scrollback_area = Rect::new(3, 0, 74, 40);
    app.push_block(Block::Assistant {
        text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
    });
    app.toggle_table_chart(0, 0);
    assert!(app.has_rendering_charts(), "点击后应立刻进入后台渲染态");

    let state = app.blocks[0].table_chart(0).expect("图表态已建立");
    let rows = crate::render::chart::rows_needed(
        state.spec.kind,
        state.spec.categories.len(),
        state.spec.series.len(),
        71 * 8,
    );
    app.image_slots = vec![ImageSlot {
        entry: 0,
        source: SlotSource::Chart { table: 0 },
        x: 6,
        y: 5,
        cols: 71,
        rows,
        px: (71 * 8, u32::from(rows) * 18),
        hash: 0xabcd,
        placement_id: 0x1234,
    }];

    // 渲染尚未回填时 emit 不阻塞、不输出半成品
    let mut pending_out = Vec::new();
    emit(&mut app, &mut pending_out).expect("写内存不会失败");
    assert!(pending_out.is_empty());

    // 用户中途取消后，收到在途回执必须安全丢弃
    app.toggle_table_chart(0, 0);
    assert!(!app.has_rendering_charts());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(crate::protocol::AppEvent::ChartEncoded {
            table,
            image_id,
            px,
            payload,
        }) = receiver.try_recv()
        {
            assert!(!app.complete_chart_encode(table, image_id, px, payload));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "未在预算内收到回执");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    // 再次展开并等回填完成后可正常贴出
    app.toggle_table_chart(0, 0);
    settle_chart(&mut app, &mut receiver);
    assert!(!app.has_rendering_charts());
    let mut ready_out = Vec::new();
    emit(&mut app, &mut ready_out).expect("写内存不会失败");
    assert!(contains(&ready_out, b"\x1bP0;1q"));
}

/// 在事件队列边界控制回调顺序，不依赖线程完成速度。
#[test]
fn chart_completion_follows_identity_and_rejects_obsolete_sizes() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::Assistant {
        text: "| 项 | 值 |\n|---|---|\n| a | 10 |\n| b | 20 |".into(),
    });
    app.toggle_table_chart(0, 0);
    let state = app.blocks[0].table_chart_mut(0).expect("图表态");
    let id = state.image_id;
    let first_px = (100, 40);
    let second_px = (200, 40);
    state.rendering = Some((second_px, std::time::Instant::now()));
    let canvas = crate::render::canvas::Canvas::new(200, 40, [0; 3]);
    let payload = Arc::new(encode::encode(&canvas, GraphicsProto::Kitty, id, 25, 2));

    // 历史页前插后，原图表从下标 0 移到 1。
    app.blocks.insert(
        0,
        crate::transcript::Entry::new(Block::Assistant {
            text: "历史".into(),
        }),
    );
    assert!(!app.complete_chart_encode(0, id, first_px, Arc::clone(&payload)));
    assert!(app.has_rendering_charts());
    assert!(app.complete_chart_encode(0, id, second_px, Arc::clone(&payload)));
    assert!(!app.has_rendering_charts());
    // 新尺寸已经完成时，旧回执不能因为 rendering=None 而被接收。
    assert!(!app.complete_chart_encode(0, id, first_px, Arc::clone(&payload)));
    assert!(!app.complete_chart_encode(0, id, second_px, Arc::clone(&payload)));
    let state = app.blocks[1].table_chart(0).expect("前插后图表仍存在");
    assert_eq!(state.px, second_px);
    assert!(Arc::ptr_eq(
        state.payload.as_ref().expect("新载荷"),
        &payload
    ));
    app.toggle_table_chart(1, 0);
    assert!(!app.complete_chart_encode(0, id, second_px, payload));
}

#[test]
fn abort_chart_encode_clears_state_sets_notice_and_requests_redraw() {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::Assistant {
        text: "| 项 | 值 |\n|---|---|\n| a | 10 |\n| b | 20 |".into(),
    });
    app.toggle_table_chart(0, 0);
    assert!(app.blocks[0].table_chart(0).is_some());

    abort_chart_encode(&mut app, 0, 0, &std::io::Error::other("resource temporarily unavailable"));

    assert!(app.blocks[0].table_chart(0).is_none());
    assert!(app.notice_visible());
    assert!(matches!(
        receiver.try_recv(),
        Ok(crate::protocol::AppEvent::LayoutReady)
    ));
}
