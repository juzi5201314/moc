//! 终端图形能力探测 + 每帧图像放置。
//!
//! 刻意不做「猜终端」：启动时用终端自己的查询串问一次（单元格像素尺寸、窗口
//! 像素尺寸、kitty 图形应答、DA1 的 sixel 位），拿不到就当作不支持——没有
//! 单元格像素尺寸就无法 1:1 落位，宁可不出图也不出缩放后发虚的图。
//!
//! 放置策略是「后处理」：每帧 `terminal.draw()` 之后把图像按格坐标贴上去。
//! kitty 的图像独立于文本层，只在位置变化时重放，消失时按图像 ID 删除；
//! sixel / iTerm2 的图像会被后续文本覆盖，因此位置或内容变化时整串重发，
//! 图像消失时则请求下一帧清屏重绘（`App::chart_repaint`），只靠文本 diff
//! 擦不掉留在空白占位格上的旧像素。浮层（模态、toast、拖选）期间一律不放置，
//! 浮层消失后整批重放。

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::app::{App, ChartSlot, Focus, Placed};
use crate::render::chart::{encode, layout, rasterize, with_fonts};

/// 终端单元格的像素尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPixels {
    pub w: u32,
    pub h: u32,
}

/// 支持的图形协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsProto {
    /// kitty 图形协议（kitty / WezTerm / foot / ghostty 等）。
    Kitty,
    /// DEC sixel（xterm -ti vt340、mlterm、WezTerm 的 sixel 模式等）。
    Sixel,
    /// iTerm2 内联图片（iTerm2 / WezTerm 的 iTerm2 模式）。
    Iterm2,
}

/// 探测结果：协议 + 单元格像素尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphicsSupport {
    pub proto: GraphicsProto,
    pub cell: CellPixels,
}

/// 进程内是否真的放置过 kitty 图像（退出时据此决定要不要发清理序列）。
static KITTY_USED: AtomicBool = AtomicBool::new(false);

/// 探测预算：终端应答应在 1~10ms 内到齐，250ms 只是为了防止哑终端拖住启动。
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// 探测终端图形能力。非 unix 平台直接返回 `None`（没有安全读 fd 的途径）。
pub fn probe() -> Option<GraphicsSupport> {
    let forced = std::env::var("MOC_CHART_GRAPHICS").ok();
    if forced.as_deref() == Some("none") {
        return None;
    }
    let forced_proto = match forced.as_deref() {
        Some("kitty") => Some(GraphicsProto::Kitty),
        Some("sixel") => Some(GraphicsProto::Sixel),
        Some("iterm2") => Some(GraphicsProto::Iterm2),
        _ => None,
    };
    let manual_cell = std::env::var("MOC_CHART_CELL")
        .ok()
        .and_then(|spec| parse_cell(&spec));
    // tmux 会吞掉终端应答且需要 passthrough 才能转发图像：没有显式覆盖就不启用。
    if forced_proto.is_none() && std::env::var_os("TMUX").is_some() {
        tracing::info!("检测到 tmux，跳过图形能力探测");
        return None;
    }
    let replies = if forced_proto.is_some() && manual_cell.is_some() {
        Replies::default()
    } else {
        query_terminal()
    };
    let proto = forced_proto.or_else(|| {
        if replies.kitty {
            Some(GraphicsProto::Kitty)
        } else if replies.sixel {
            Some(GraphicsProto::Sixel)
        } else if std::env::var("TERM_PROGRAM").as_deref() == Ok("iTerm.app")
            || std::env::var("LC_TERMINAL").as_deref() == Ok("iTerm2")
        {
            Some(GraphicsProto::Iterm2)
        } else {
            None
        }
    })?;
    let cell = manual_cell.or_else(|| cell_from_replies(&replies))?;
    tracing::info!(?proto, cell_w = cell.w, cell_h = cell.h, "图形能力探测完成");
    Some(GraphicsSupport { proto, cell })
}

/// 解析 `MOC_CHART_CELL` 的 `WxH` 形式（也接受 `W,H`）。
fn parse_cell(spec: &str) -> Option<CellPixels> {
    let (w, h) = spec
        .split_once(['x', 'X', ','])
        .map(|(w, h)| (w.trim(), h.trim()))?;
    let cell = CellPixels {
        w: w.parse().ok()?,
        h: h.parse().ok()?,
    };
    if !(1..=128).contains(&cell.w) || !(1..=128).contains(&cell.h) {
        return None;
    }
    Some(cell)
}

/// 终端查询应答的解析结果。
#[derive(Debug, Default, PartialEq, Eq)]
struct Replies {
    /// `CSI 16 t` 的单元格像素尺寸。
    cell: Option<CellPixels>,
    /// `CSI 14 t` 的窗口像素尺寸。
    window: Option<(u32, u32)>,
    /// kitty 图形协议应答 `OK`。
    kitty: bool,
    /// DA1 里的 sixel 位（参数 4）。
    sixel: bool,
    /// 是否已收到 DA1：收到就说明「协议能力已问清」，不必再等其余应答。
    da1: bool,
}

/// 由应答算单元格尺寸：优先 `CSI 16 t`，否则窗口像素 ÷ 终端格数。
fn cell_from_replies(replies: &Replies) -> Option<CellPixels> {
    if let Some(cell) = replies.cell {
        return Some(cell);
    }
    let (width, height) = replies.window?;
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let cell = CellPixels {
        w: width / u32::from(cols.max(1)),
        h: height / u32::from(rows.max(1)),
    };
    (cell.w >= 1 && cell.h >= 1).then_some(cell)
}

/// 发查询并收应答（仅 unix：raw mode 下直接读 fd 0）。
#[cfg(unix)]
fn query_terminal() -> Replies {
    use std::io::Write as _;

    let mut stdout = std::io::stdout();
    // CSI 16 t（单元格像素）、CSI 14 t（窗口像素）、kitty 图形查询、DA1
    let _ = stdout.write_all(b"\x1b[16t\x1b[14t\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c");
    let _ = stdout.flush();
    let mut bytes = Vec::new();
    let deadline = std::time::Instant::now() + PROBE_BUDGET;
    let stdin = std::io::stdin();
    let mut chunk = [0u8; 1024];
    while std::time::Instant::now() < deadline {
        // 预算用尽前最多等这么久；终端一次性把四个应答写完，通常第一次就读全
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let mut fds = [rustix::event::PollFd::new(
            &stdin,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        }
        match rustix::io::read(&stdin, &mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                let replies = parse_replies(&bytes);
                // 尺寸与协议都问清了就收工：常见终端在首轮就读全四个应答，
                // 不必为等齐 kitty 应答而白等满预算。
                let sized = replies.cell.is_some() || replies.window.is_some();
                if sized && replies.da1 {
                    return replies;
                }
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        }
    }
    parse_replies(&bytes)
}

/// 非 unix：没有安全读 fd 的途径，按不支持处理。
#[cfg(not(unix))]
fn query_terminal() -> Replies {
    Replies::default()
}

/// 解析终端应答字节流：只认 `CSI ... t`、DA1 与 kitty 的 APC 应答，其余丢弃。
///
/// 探测窗口内的用户键入同样会被读走（无法区分），250ms 的窗口把这个代价
/// 压到可接受范围；代价写在这里，免得后来者以为是 bug。
fn parse_replies(bytes: &[u8]) -> Replies {
    let mut replies = Replies::default();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        match bytes.get(index + 1) {
            Some(b'[') => {
                let start = index + 2;
                let mut end = start;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                if end >= bytes.len() {
                    break;
                }
                apply_csi(&bytes[start..end], bytes[end], &mut replies);
                index = end + 1;
            }
            Some(b'_') => {
                let start = index + 2;
                let mut end = start;
                while end + 1 < bytes.len() && !(bytes[end] == 0x1b && bytes[end + 1] == b'\\') {
                    end += 1;
                }
                if end + 1 >= bytes.len() {
                    break;
                }
                if bytes[start..end].ends_with(b"OK") {
                    replies.kitty = true;
                }
                index = end + 2;
            }
            _ => index += 2,
        }
    }
    replies
}

/// 单条 CSI 应答的语义：`t` 家庭带尺寸，`c` 是 DA1。
fn apply_csi(params: &[u8], final_byte: u8, replies: &mut Replies) {
    let text = std::str::from_utf8(params).unwrap_or_default();
    let text = text.strip_prefix('?').unwrap_or(text);
    let values: Vec<u32> = text
        .split(';')
        .filter(|field| !field.is_empty())
        .filter_map(|field| field.parse().ok())
        .collect();
    match final_byte {
        b't' => match values.as_slice() {
            // CSI 6 ; 高 ; 宽 t：单元格像素尺寸
            [6, height, width, ..] => {
                replies.cell = Some(CellPixels {
                    w: *width,
                    h: *height,
                });
            }
            // CSI 4 ; 高 ; 宽 t：窗口像素尺寸
            [4, height, width, ..] => replies.window = Some((*width, *height)),
            _ => {}
        },
        // DA1：4 = sixel，28 = ReGIS，这些位不必全有，只认 sixel
        b'c' => {
            replies.da1 = true;
            replies.sixel = values.contains(&4);
        }
        _ => {}
    }
}

/// 每帧放置图像：在 `terminal.draw()` 之后调用。
///
/// 位置与内容都没变就跳过；kitty 换位时先删旧放置，永久消失的图像按图像 ID
/// 连数据一起删除。sixel / iTerm2 消失或改尺寸时置 `chart_repaint`，由事件
/// 循环清屏重绘，避免残影留在空白占位行上。
pub fn emit(app: &mut App, out: &mut impl Write) -> std::io::Result<()> {
    let Some(support) = app.graphics else {
        return Ok(());
    };
    let covered = app.modal.is_some()
        || app.notice_visible()
        || app.warning_visible()
        || app.selection.is_some();
    if covered != app.chart_covered {
        app.chart_covered = covered;
        // 浮层期间不贴图（会盖住浮层，或被浮层重绘擦掉）：位置记录留着，
        // 恢复后整批重放，浮层期间消失的图才能按 ID 删干净。
        if covered {
            app.chart_resume = true;
        }
    }
    if covered {
        return Ok(());
    }
    let resume = std::mem::take(&mut app.chart_resume);
    let previous = std::mem::take(&mut app.chart_placed);
    let slots: Vec<ChartSlot> = app.chart_slots.clone();
    let theme = app.theme;
    let mut placed: Vec<Placed> = Vec::with_capacity(slots.len());
    let mut wrote = false;
    for slot in slots {
        let Some(entry) = app.blocks.get_mut(slot.entry) else {
            continue;
        };
        let Some(state) = entry.table_chart_mut(slot.table) else {
            continue;
        };
        let Some(chart_layout) = layout(&state.spec, slot.cols, support.cell) else {
            continue;
        };
        // 载荷按像素尺寸缓存：终端格尺寸或可用列数变了就重排，绝不缩放旧图
        let fresh = state.payload.is_none() || state.px != chart_layout.px;
        if fresh {
            let canvas = with_fonts(|fonts| rasterize(&state.spec, &chart_layout, theme, fonts));
            state.payload = Some(std::sync::Arc::new(encode::encode(
                &canvas,
                support.proto,
                state.image_id,
                slot.cols,
                slot.rows,
            )));
            state.px = chart_layout.px;
        }
        let Some(payload) = state.payload.as_ref() else {
            continue;
        };
        let reused = !fresh
            && !resume
            && previous.iter().any(|old| {
                old.image_id == state.image_id
                    && old.proto == support.proto
                    && old.x == slot.x
                    && old.y == slot.y
                    && old.cols == slot.cols
                    && old.rows == slot.rows
                    && old.hash == slot.hash
            });
        if !reused {
            write!(out, "\x1b[{};{}H", slot.y + 1, slot.x + 1)?;
            match payload.as_ref() {
                encode::Payload::Kitty(_) => {
                    KITTY_USED.store(true, Ordering::Relaxed);
                    out.write_all(encode::kitty_hide(state.image_id).as_bytes())?;
                    // 像素数据只传一次：换位只需要重新放置
                    if fresh {
                        out.write_all(payload.as_bytes())?;
                    }
                    out.write_all(
                        encode::kitty_place(state.image_id, slot.cols, slot.rows).as_bytes(),
                    )?;
                }
                _ => out.write_all(payload.as_bytes())?,
            }
            wrote = true;
        }
        placed.push(Placed {
            image_id: state.image_id,
            proto: support.proto,
            x: slot.x,
            y: slot.y,
            cols: slot.cols,
            rows: slot.rows,
            hash: slot.hash,
        });
    }
    // kitty 的放置独立于文本层：图像离开视口或收起图表时必须显式删除。
    // sixel / iTerm2 没有删除动作，只能请求下一帧清屏重绘。
    let mut repaint = false;
    for old in &previous {
        match placed.iter().find(|now| now.image_id == old.image_id) {
            None => {
                if old.proto == GraphicsProto::Kitty {
                    out.write_all(encode::kitty_delete(old.image_id).as_bytes())?;
                    wrote = true;
                } else {
                    repaint = true;
                }
            }
            Some(now) => {
                if old.proto != GraphicsProto::Kitty
                    && (old.cols != now.cols || old.rows != now.rows)
                {
                    repaint = true;
                }
            }
        }
    }
    if repaint {
        app.chart_repaint = true;
    }
    app.chart_placed = placed;
    if wrote {
        // 贴图把光标挪走了：把它送回输入框（ratatui 只在焦点位于输入框时定位光标）
        if app.focus == Focus::Prompt && !app.prompt_area.is_empty() {
            let (x, y) = crate::render::prompt_cursor_position(app, app.prompt_area);
            write!(out, "\x1b[{};{}H", y + 1, x + 1)?;
        }
        out.flush()?;
    }
    Ok(())
}

/// 退出前清理：删掉本进程放置过的全部 kitty 图像（其余协议随备用屏一起消失）。
pub fn cleanup(out: &mut impl Write) {
    if !KITTY_USED.load(Ordering::Relaxed) {
        return;
    }
    KITTY_USED.store(false, Ordering::Relaxed);
    let _ = out.write_all(b"\x1b_Ga=d,d=a,q=2\x1b\\");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cell_window_kitty_and_sixel_replies() {
        // 单元格 8x18、窗口 800x540、kitty OK、DA1 含 sixel 位 4
        let replies =
            parse_replies(b"\x1b[6;18;8t\x1b[4;540;800t\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c");
        assert_eq!(replies.cell, Some(CellPixels { w: 8, h: 18 }));
        assert_eq!(replies.window, Some((800, 540)));
        assert!(replies.kitty, "kitty 应答");
        assert!(replies.sixel, "DA1 的 sixel 位");
    }

    #[test]
    fn ignores_unknown_bytes_and_partial_replies() {
        let replies = parse_replies(b"\x1b[?1;2chello\x1b[6;18;8");
        assert!(!replies.sixel, "无 sixel 位");
        assert!(!replies.kitty);
        assert_eq!(replies.cell, None, "截断的应答不参与判定");
    }

    #[test]
    fn cell_spec_needs_both_dimensions_in_range() {
        assert_eq!(parse_cell("8x18"), Some(CellPixels { w: 8, h: 18 }));
        assert_eq!(parse_cell("10,20"), Some(CellPixels { w: 10, h: 20 }));
        assert_eq!(parse_cell("8x"), None);
        assert_eq!(parse_cell("0x18"), None);
        assert_eq!(parse_cell("8x999"), None);
    }
}

#[cfg(test)]
mod emit_tests {
    use super::*;

    use ratatui::layout::Rect;

    use crate::app::ChartSlot;
    use crate::transcript::Block;

    /// 造一个「已展开一张图表」的 app，并给出对应的槽位。
    fn app_with_chart(proto: GraphicsProto) -> (App, ChartSlot) {
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
        let slot = ChartSlot {
            entry: 0,
            table: 0,
            x: 6,
            y: 5,
            cols: 71,
            rows,
            px: (71 * 8, u32::from(rows) * 18),
            hash: 0xabcd,
        };
        app.chart_slots = vec![slot];
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
        assert_eq!(app.chart_placed.len(), 1);

        let mut again = Vec::new();
        emit(&mut app, &mut again).expect("写内存不会失败");
        assert!(again.is_empty(), "位置与指纹都没变，不该重发：{again:?}");

        // 滚动一格：位置变化必须重发；纯移动不触发整屏清屏，避免滚动卡顿
        app.chart_slots[0].y = slot.y + 1;
        let mut moved = Vec::new();
        emit(&mut app, &mut moved).expect("写内存不会失败");
        assert!(contains(&moved, b"\x1bPq"), "换位后应重发载荷");
        assert!(!app.chart_repaint, "纯换位不需要清屏重绘");
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

        app.chart_slots[0].y += 2;
        let mut moved = Vec::new();
        emit(&mut app, &mut moved).expect("写内存不会失败");
        assert!(
            contains(&moved, b"a=d,d=p"),
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
        app.chart_slots.clear();
        let mut gone = Vec::new();
        emit(&mut app, &mut gone).expect("写内存不会失败");
        assert!(
            contains(&gone, b"a=d,d=i"),
            "消失的图像要按 ID 删除数据与放置"
        );
        assert!(app.chart_placed.is_empty());
        assert!(!app.chart_repaint, "kitty 有显式删除，不需要清屏");
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
        assert!(covered.is_empty(), "浮层期间不写任何图形序列");
        assert_eq!(
            app.chart_placed.len(),
            1,
            "位置记录必须保留：恢复后要靠它重放"
        );

        app.notice = None;
        app.chart_slots = vec![slot];
        let mut resumed = Vec::new();
        emit(&mut app, &mut resumed).expect("写内存不会失败");
        assert!(contains(&resumed, b"\x1bPq"), "浮层消失后重放");
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
        assert!(covered.is_empty());

        // 浮层期间用户把图表还原成表格，浮层随后消失
        app.blocks[0].set_table_chart(0, None);
        app.chart_slots.clear();
        app.notice = None;
        let mut resumed = Vec::new();
        emit(&mut app, &mut resumed).expect("写内存不会失败");
        assert!(
            contains(&resumed, b"a=d,d=i"),
            "消失的图必须按 ID 删掉数据与放置：{:?}",
            String::from_utf8_lossy(&resumed)
        );
        assert!(app.chart_placed.is_empty());
    }

    /// sixel / iTerm2 没有删除动作：图消失时只能请求下一帧清屏重绘。
    #[test]
    fn non_kitty_removal_requests_full_repaint() {
        let (mut app, _slot) = app_with_chart(GraphicsProto::Sixel);
        let mut first = Vec::new();
        emit(&mut app, &mut first).expect("写内存不会失败");
        assert!(!app.chart_repaint);

        app.chart_slots.clear();
        let mut gone = Vec::new();
        emit(&mut app, &mut gone).expect("写内存不会失败");
        assert!(app.chart_repaint, "sixel 图消失后必须请求清屏重绘");
        assert!(app.chart_placed.is_empty());
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
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::transcript::Block;

    /// 解析 sixel 头里的栅格属性 `"Pan;Pad;Ph;Pv`，返回 (宽, 高)。
    fn sixel_size(bytes: &[u8]) -> (u32, u32) {
        let text = String::from_utf8_lossy(bytes);
        // `\x1bPq"` 四个字节：跳过它才是栅格属性
        let start = text.find("\x1bPq\"").expect("sixel 头") + 4;
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
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
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
        let slot = *app.chart_slots.first().expect("图表已登记槽位");

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
}
