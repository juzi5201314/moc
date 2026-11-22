use super::*;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::transcript::Block;

/// 解析 sixel 头里的栅格属性 `"Pan;Pad;Ph;Pv`，返回 (宽, 高)。
fn sixel_size(bytes: &[u8]) -> (u32, u32) {
    let text = String::from_utf8_lossy(bytes);
    // `\x1bP0;1q"` 7 个字节：跳过它才是栅格属性
    let start = text.find("\x1bP0;1q\"").expect("sixel 头") + 7;
    let rest = &text[start..];
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit() && ch != ';')
        .unwrap_or(rest.len());
    let fields: Vec<u32> = rest[..end]
        .split(';')
        .map(|field| field.parse().expect("十进制"))
        .collect();
    (fields[2], fields[3])
}

/// 端到端：绘制 → 槽位登记 → 贴图，图片像素尺寸恰好等于预留的格子区域。
///
/// 这是「不缩放」的唯一证据：终端拿到的像素宽高必须正好是
/// 列数 × 单元格宽、行数 × 单元格高，多一像素少一像素都会触发重采样。
#[test]
fn emitted_image_matches_the_reserved_cell_area() {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let cell = CellPixels { w: 9, h: 20 };
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Sixel,
        cell,
    });
    app.push_block(Block::Assistant {
        text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
    });
    app.toggle_table_chart(0, 0);

    let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("测试终端");
    let mut view = crate::render::ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("绘制成功");
    let slot = app.image_slots.first().expect("图表已登记槽位").clone();

    // 首帧 emit 触发后台编码，收到回执后下一帧正式贴出
    emit(&mut app, &mut Vec::new()).expect("写内存不会失败");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(crate::protocol::AppEvent::ChartEncoded {
            table,
            image_id,
            px,
            payload,
        }) = receiver.try_recv()
        {
            assert!(app.complete_chart_encode(table, image_id, px, payload));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "图表未在预算内完成编码"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let mut out = Vec::new();
    emit(&mut app, &mut out).expect("写内存不会失败");
    assert_eq!(
        sixel_size(&out),
        (u32::from(slot.cols) * cell.w, u32::from(slot.rows) * cell.h),
        "图片像素尺寸必须等于槽位的格子区域"
    );
    // 宽终端按 720px 上限收窄：720 / 9 = 80 列，而不是占满正文宽度。
    assert_eq!(
        slot.cols,
        (crate::render::chart::MAX_CHART_PX / cell.w) as u16
    );
    assert_eq!(slot.rows, 6);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 画一张真 PNG 到临时目录，返回它的绝对路径。
fn write_png(directory: &std::path::Path, name: &str, size: (u32, u32)) -> std::path::PathBuf {
    let mut canvas = crate::render::canvas::Canvas::new(size.0, size.1, [0x20, 0x40, 0x80]);
    let dot = canvas.color([0xff, 0xff, 0xff]);
    canvas.fill_rect(0, 0, size.0 / 2, size.1 / 2, dot);
    let path = directory.join(name);
    std::fs::write(&path, crate::render::graphics::encode::png(&canvas)).expect("落盘");
    path
}

/// 事件循环的替身：把解码结果灌回 `ImageStore`，直到本帧登记出图片槽位。
fn settle_images(
    app: &mut App,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<crate::protocol::AppEvent>,
    terminal: &mut Terminal<TestBackend>,
    view: &mut crate::render::ScrollbackView,
) {
    for _ in 0..400 {
        while let Ok(event) = receiver.try_recv() {
            if let crate::protocol::AppEvent::ImagePrepared { key, result } = event {
                app.images.complete(key, result);
                app.view_gen = app.view_gen.wrapping_add(1);
            }
        }
        terminal
            .draw(|frame| crate::render::draw(frame, app, view))
            .expect("绘制成功");
        // 登记出槽位后主动热身编码：生产里这一步由 `emit` → `payload()` 触发，
        // 测试不能调 emit（它会消费放置记录），所以单独走 request_payload。
        if let Some(support) = app.graphics {
            for slot in &app.image_slots {
                if let crate::app::SlotSource::Image(key) = &slot.source {
                    app.images.request_payload(
                        key,
                        support.proto,
                        slot.cols,
                        slot.rows,
                        slot.px,
                        app.theme,
                    );
                }
            }
        }
        // 解码完只说明能排版；还要等后台把缩放 / 量化 / 编码也做完，
        // 否则 `emit` 拿不到载荷（这正是主线程不再被卡住的原因）。
        if !app.image_slots.is_empty()
            && app.image_slots.iter().all(|slot| payload_ready(app, slot))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("图片没有在预算内解码并编码完成");
}

/// 槽位对应的编码载荷是否已经就绪。
fn payload_ready(app: &App, slot: &crate::app::ImageSlot) -> bool {
    let crate::app::SlotSource::Image(key) = &slot.source else {
        return true;
    };
    let Some(support) = app.graphics else {
        return true;
    };
    app.images
        .has_payload(key, support.proto, slot.cols, slot.rows, slot.px)
}

/// 从输出里取出全部 kitty 放置 ID（`a=p` 序列）。
fn kitty_placements(out: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    text.split("\x1b_G")
        .filter(|sequence| sequence.starts_with("a=p,"))
        .filter_map(|sequence| {
            sequence
                .split(',')
                .find(|field| field.starts_with("p="))
                .map(str::to_owned)
        })
        .collect()
}

/// 端到端：markdown 本地图片 → 解码 → 占位行 → 按格贴出。
#[test]
fn markdown_image_flows_from_file_to_pixels() {
    let directory = tempfile::tempdir().expect("临时目录");
    let path = write_png(directory.path(), "shot.png", (160, 90));
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let cell = CellPixels { w: 9, h: 20 };
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Sixel,
        cell,
    });
    app.push_block(Block::Assistant {
        text: format!("图：\n\n![]({})\n", path.display()),
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("测试终端");
    let mut view = crate::render::ScrollbackView::new();
    settle_images(&mut app, &mut receiver, &mut terminal, &mut view);

    let slot = app.image_slots.first().expect("图片已登记槽位").clone();
    assert!(matches!(slot.source, SlotSource::Image(_)));
    assert_eq!(
        slot.x,
        app.scrollback_area.x + super::super::PROSE_INDENT as u16
    );
    assert_eq!(slot.cols, 18, "160px ÷ 9px 单元格，向上取整");
    assert_eq!(slot.rows, 5, "90px ÷ 20px 单元格，向上取整");

    let mut out = Vec::new();
    emit(&mut app, &mut out).expect("写内存不会失败");
    assert_eq!(
        sixel_size(&out),
        (u32::from(slot.cols) * cell.w, u32::from(slot.rows) * cell.h),
        "sixel 栅格尺寸必须等于槽位的格子区域"
    );
}

/// 同一张图在一条消息里出现两次：两个放置 ID，像素数据只传一次。
#[test]
fn repeated_images_get_distinct_placements() {
    let directory = tempfile::tempdir().expect("临时目录");
    let path = write_png(directory.path(), "twice.png", (64, 36));
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    });
    app.push_block(Block::Assistant {
        text: format!("![]({})\n\n![]({})\n", path.display(), path.display()),
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("测试终端");
    let mut view = crate::render::ScrollbackView::new();
    settle_images(&mut app, &mut receiver, &mut terminal, &mut view);
    assert_eq!(app.image_slots.len(), 2, "两处引用各占一块");

    let mut out = Vec::new();
    emit(&mut app, &mut out).expect("写内存不会失败");
    let text = String::from_utf8_lossy(&out).to_string();
    assert_eq!(text.matches("a=t,").count(), 1, "像素数据只传一次");
    let placements = kitty_placements(&out);
    assert_eq!(placements.len(), 2, "两次放置：{placements:?}");
    assert_ne!(placements[0], placements[1], "放置 ID 必须区分");
}

/// 图片消失：kitty 按图像 ID 删除，sixel 只能请求清屏重绘。
#[test]
fn vanished_images_are_cleaned_up_per_protocol() {
    let directory = tempfile::tempdir().expect("临时目录");
    let path = write_png(directory.path(), "gone.png", (32, 18));
    for (proto, expect_delete) in [(GraphicsProto::Kitty, true), (GraphicsProto::Sixel, false)] {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.graphics = Some(GraphicsSupport {
            proto,
            cell: CellPixels { w: 8, h: 18 },
        });
        app.push_block(Block::Assistant {
            text: format!("![]({})\n", path.display()),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).expect("测试终端");
        let mut view = crate::render::ScrollbackView::new();
        settle_images(&mut app, &mut receiver, &mut terminal, &mut view);

        let mut first = Vec::new();
        emit(&mut app, &mut first).expect("写内存不会失败");
        assert!(!app.image_placed.is_empty());

        // 正文换成纯文本：图片离开视口（改块内容必须让缓存失效）
        app.blocks[0].block = Block::Assistant {
            text: "纯文本".into(),
        };
        app.blocks[0].touch();
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(app.image_slots.is_empty(), "换文案后不再有图片槽位");

        let mut gone = Vec::new();
        emit(&mut app, &mut gone).expect("写内存不会失败");
        if expect_delete {
            assert!(
                contains(&gone, b"a=d,d=i"),
                "kitty 必须按图像 ID 删干净：{:?}",
                String::from_utf8_lossy(&gone)
            );
            assert!(!app.image_repaint, "kitty 不需要清屏");
        } else {
            assert!(app.image_repaint, "sixel 靠清屏擦掉旧像素");
        }
        assert!(app.image_placed.is_empty());
    }
}

/// todo 卡片只在与图像槽位相交时才触发撤图，避免每次 hover 整屏闪图。
#[test]
fn todo_card_only_holds_back_intersecting_images() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.image_slots.push(ImageSlot {
        entry: 0,
        source: SlotSource::Chart { table: 0 },
        x: 10,
        y: 5,
        cols: 20,
        rows: 10,
        px: (0, 0),
        hash: 0,
        placement_id: 0,
    });
    app.todo_hud.card_visible = true;
    app.todo_hud.card_area = Rect::new(25, 8, 30, 10);
    assert!(image_covered(&app), "卡片盖住图像时应撤图");
    app.todo_hud.card_area = Rect::new(50, 8, 20, 10);
    assert!(!image_covered(&app), "不相交时不能整屏闪图");
    app.todo_hud.card_visible = false;
    assert!(!image_covered(&app));
}
