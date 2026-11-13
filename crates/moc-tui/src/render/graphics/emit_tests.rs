use super::*;

use ratatui::layout::Rect;

use crate::app::ImageSlot;
use crate::transcript::Block;

/// 造一个「已展开一张图表」的 app，并给出对应的槽位。
fn app_with_chart(proto: GraphicsProto) -> (App, ImageSlot) {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
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
    assert!(contains(&out, b"\x1bPq"), "sixel 载荷应已写出");
    assert!(contains(&out, b"\x1b\\"), "sixel 结尾 ST");
    assert_eq!(app.image_placed.len(), 1);

    let mut again = Vec::new();
    emit(&mut app, &mut again).expect("写内存不会失败");
    assert!(again.is_empty(), "位置与指纹都没变，不该重发：{again:?}");

    // 滚动一格：位置变化必须重发；纯移动不触发整屏清屏，避免滚动卡顿
    app.image_slots[0].y = slot.y + 1;
    let mut moved = Vec::new();
    emit(&mut app, &mut moved).expect("写内存不会失败");
    assert!(contains(&moved, b"\x1bPq"), "换位后应重发载荷");
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
    app.image_slots.clear();
    let mut gone = Vec::new();
    emit(&mut app, &mut gone).expect("写内存不会失败");
    assert!(
        contains(&gone, b"a=d,d=i"),
        "消失的图像要按 ID 删除数据与放置"
    );
    assert!(app.image_placed.is_empty());
    assert!(!app.image_repaint, "kitty 有显式删除，不需要清屏");
}

/// 浮层期间不贴图，浮层消失后整批重放（避免图像盖住模态或残影）。
#[test]
fn overlays_suspend_and_resume_placement() {
    let (mut app, slot) = app_with_chart(GraphicsProto::Sixel);
    let mut first = Vec::new();
    emit(&mut app, &mut first).expect("写内存不会失败");
    assert!(contains(&first, b"\x1bPq"), "先正常贴一次");
    app.notice = Some("已附加剪贴板图片".into());
    app.notice_at = Some(std::time::Instant::now());
    let mut covered = Vec::new();
    emit(&mut app, &mut covered).expect("写内存不会失败");
    assert!(
        !contains(&covered, b"\x1bPq"),
        "浮层期间不得再贴图：{:?}",
        String::from_utf8_lossy(&covered)
    );

    app.notice = None;
    app.image_slots = vec![slot];
    let mut resumed = Vec::new();
    emit(&mut app, &mut resumed).expect("写内存不会失败");
    assert!(contains(&resumed, b"\x1bPq"), "浮层消失后重放");
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
