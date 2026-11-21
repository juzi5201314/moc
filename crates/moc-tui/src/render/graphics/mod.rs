//! 终端图形能力探测 + 每帧图像放置。
//!
//! 刻意不做「猜终端」：启动时用终端自己的查询串问一次（单元格像素尺寸、窗口
//! 像素尺寸、kitty 图形应答、DA1 的 sixel 位），拿不到就当作不支持——没有
//! 单元格像素尺寸就无法 1:1 落位，宁可不出图也不出缩放后发虚的图。
//!
//! 放置策略是「后处理」：每帧 `terminal.draw()` 之后把图像按格坐标贴上去。
//! 槽位来自两处：表格图表（载荷随块缓存在 `TableChartState` 里）与通用内联
//! 图片（载荷在 [`crate::render::image::ImageStore`] 里，按内容寻址）。
//! kitty 的图像独立于文本层，只在位置变化时重放，消失时按图像 ID 删除；
//! 同一张图贴多处各有放置 ID，像素数据只传一次。sixel / iTerm2 的图像会被
//! 后续文本覆盖，因此位置或内容变化时整串重发，图像消失时则请求下一帧清屏
//! 重绘（`App::image_repaint`），只靠文本 diff 擦不掉留在空白占位格上的旧像素。
//! 浮层（模态、toast、拖选）期间一律不放置，浮层消失后整批重放。

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::layout::Rect;

use crate::app::{App, Focus, ImageSlot, Placed, SlotSource};
use crate::protocol::AppEvent;
use crate::render::chart::{ChartLayout, layout, rasterize_for_proto, with_fonts};

pub mod encode;

/// 终端单元格的像素尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPixels {
    pub w: u32,
    pub h: u32,
}

/// 支持的图形协议。
///
/// `Hash` 给「终端里已存在哪份像素数据」建索引用：同一张图可能按不同协议
/// 各传输一份，二者不能互相顶替。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GraphicsProto {
    /// kitty 图形协议（kitty / WezTerm / foot / ghostty 等）。
    Kitty,
    /// DEC sixel（xterm -ti vt340、mlterm、WezTerm 的 sixel 模式等）。
    Sixel,
    /// iTerm2 内联图片（iTerm2 / WezTerm 的 iTerm2 模式）。
    Iterm2,
}

/// 出图所需的最小单元格宽度（像素）：再窄放不下半个字形。
pub const MIN_CELL_W: u32 = 4;

/// 出图所需的最小单元格高度（像素）：低于它一行字都放不下。
pub const MIN_CELL_H: u32 = 8;

/// 单元格尺寸是否足够出图。
///
/// 入口是否出现、点击后能否画出来都必须走这同一个判定，否则会出现
/// 「按钮能点、点了没反应」的死入口。
pub fn cell_supported(cell: CellPixels) -> bool {
    cell.w >= MIN_CELL_W && cell.h >= MIN_CELL_H
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

/// 终端能力探测结果：图形协议 + DEC 2026 同步输出。
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    /// 图形协议与单元格像素；`None` = 本终端不出图表入口。
    pub graphics: Option<GraphicsSupport>,
    /// 终端是否支持 DEC 2026 同步输出。
    pub sync_output: bool,
}

/// 探测终端能力：一次 fd-0 往返问清图形协议、单元格像素与同步输出。
///
/// DEC 2026 用 DECRQM（`CSI ? 2026 $ p`）探测；tmux<3.7 与 tmux-CC 不透传、
/// ConPTY 不应答，这些场景默认降级为关闭，`MOC_SYNC_OUTPUT=0|1` 可强制。
/// 非 unix 平台没有安全读 fd 的途径，应答读不到则按不支持处理。
pub fn probe() -> Probe {
    let forced = std::env::var("MOC_CHART_GRAPHICS").ok();
    let forced_proto = match forced.as_deref() {
        Some("kitty") => Some(GraphicsProto::Kitty),
        Some("sixel") => Some(GraphicsProto::Sixel),
        Some("iterm2") => Some(GraphicsProto::Iterm2),
        _ => None,
    };
    let manual_cell = std::env::var("MOC_CHART_CELL")
        .ok()
        .and_then(|spec| parse_cell(&spec));
    let under_tmux = std::env::var_os("TMUX").is_some();
    let sync_forced = match std::env::var("MOC_SYNC_OUTPUT").ok().as_deref() {
        Some("1") | Some("on") | Some("true") => Some(true),
        Some("0") | Some("off") | Some("false") => Some(false),
        _ => None,
    };
    // tmux 会吞掉终端应答且需要 passthrough 才能转发图像：没有显式覆盖就不启用。
    let graphics_blocked =
        forced.as_deref() == Some("none") || (under_tmux && forced_proto.is_none());
    if under_tmux && forced_proto.is_none() && forced.as_deref() != Some("none") {
        tracing::info!("检测到 tmux，跳过图形能力探测");
    }
    let graphics_needs_query =
        !graphics_blocked && !(forced_proto.is_some() && manual_cell.is_some());
    let sync_needs_query = sync_forced.is_none() && !under_tmux;
    let replies = if graphics_needs_query || sync_needs_query {
        query_terminal()
    } else {
        Replies::default()
    };
    let graphics = if graphics_blocked {
        None
    } else {
        forced_proto
            .or_else(|| proto_from(&replies))
            .and_then(|proto| {
                manual_cell
                    .or_else(|| cell_from_replies(&replies))
                    .map(|cell| {
                        tracing::info!(
                            ?proto,
                            cell_w = cell.w,
                            cell_h = cell.h,
                            "图形能力探测完成"
                        );
                        GraphicsSupport { proto, cell }
                    })
            })
    };
    Probe {
        graphics,
        // tmux 下无论应答如何都按降级处理：应答来自 tmux 转发与否不可控。
        sync_output: sync_forced.unwrap_or(!under_tmux && replies.sync_output),
    }
}

/// 由应答与厂商环境变量选图形协议：kitty 应答优先，其次 DA1 sixel 位，
/// 最后退回 iTerm2 环境变量启发（它没有应答式探测）。
fn proto_from(replies: &Replies) -> Option<GraphicsProto> {
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
}

/// 单元格尺寸的合法范围（像素）。
///
/// 下界是「画不出一张可读的图」，上界是防御：终端应答是**不可信输入**
/// （探测窗口内 stdin 上的任何字节都会被当成应答），一个
/// `CSI 6;4294967295;8t` 就能让 `cols × cell.h` 撑出 TB 级画布，而 Rust 的
/// 分配失败是 abort，不是可恢复错误。
const CELL_RANGE: std::ops::RangeInclusive<u32> = 1..=128;

/// 单元格尺寸是否在合法范围内。
fn cell_in_range(cell: CellPixels) -> bool {
    CELL_RANGE.contains(&cell.w) && CELL_RANGE.contains(&cell.h)
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
    cell_in_range(cell).then_some(cell)
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
    /// DECRQM 应答里 2026 的状态：Ps ∈ 1..=4 表示终端认识该模式（0 = 不认识）。
    sync_output: bool,
}

/// 由应答算单元格尺寸：优先 `CSI 16 t`，否则窗口像素 ÷ 终端格数。
///
/// 两条路都要过 [`cell_in_range`]：应答来自终端，是不可信输入。
fn cell_from_replies(replies: &Replies) -> Option<CellPixels> {
    if let Some(cell) = replies.cell {
        return cell_in_range(cell).then_some(cell);
    }
    let (width, height) = replies.window?;
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let cell = CellPixels {
        w: width / u32::from(cols.max(1)),
        h: height / u32::from(rows.max(1)),
    };
    cell_in_range(cell).then_some(cell)
}

/// 发查询并收应答（仅 unix：raw mode 下直接读 fd 0）。
#[cfg(unix)]
fn query_terminal() -> Replies {
    use std::io::Write as _;

    let mut stdout = std::io::stdout();
    // CSI 16 t（单元格像素）、CSI 14 t（窗口像素）、kitty 图形查询、
    // DECRQM `? 2026 $ p`（同步输出）、DA1。
    // DECRQM 排在 DA1 之前：终端按序应答，`da1` 早退条件命中时 2026 的
    // 应答已经到齐，不需要为它再等一轮。
    let _ = stdout
        .write_all(b"\x1b[16t\x1b[14t\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[?2026$p\x1b[c");
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
    // DECRPM 应答带 `$` 中间字节（`? 模式 ; 状态 $ y`），剥掉再按参数拆。
    let text = text.strip_suffix('$').unwrap_or(text);
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
        // DECRPM：只认 2026；Ps ∈ 1..=4 均表示终端认识同步输出模式。
        b'y' => {
            if let [2026, ps] = values.as_slice() {
                replies.sync_output = (1..=4).contains(ps);
            }
        }
        _ => {}
    }
}

/// 浮层是否遮住了图像：模态、toast、告警、拖选，或与图像槽位相交的 todo 卡片。
///
/// 卡片只在与图像相交时触发；整屏撤图会让每次 hover 都闪一下。
fn image_covered(app: &App) -> bool {
    let card_covers_image = app.todo_hud.card_visible
        && app.image_slots.iter().any(|slot| {
            Rect::new(slot.x, slot.y, slot.cols, slot.rows).intersects(app.todo_hud.card_area)
        });
    app.modal.is_some()
        || app.notice_visible()
        || app.warning_visible()
        || app.selection.is_some()
        || card_covers_image
}

/// 在后台线程执行图表光栅化与协议编码，完成后通过 [`AppEvent::ChartEncoded`] 回传主线程。
#[expect(clippy::too_many_arguments, reason = "后台线程入口，直接传递上下文快照")]
pub fn spawn_chart_encode(
    events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    entry: usize,
    table: usize,
    image_id: u32,
    spec: moc_core::chart::ChartSpec,
    chart_layout: ChartLayout,
    proto: GraphicsProto,
    theme: &'static crate::theme::Theme,
) {
    std::thread::Builder::new()
        .name("moc-chart-encode".into())
        .spawn(move || {
            let canvas = with_fonts(|fonts| {
                rasterize_for_proto(&spec, &chart_layout, theme, fonts, proto)
            });
            let payload = Arc::new(encode::encode(
                &canvas,
                proto,
                image_id,
                chart_layout.cols,
                chart_layout.rows,
            ));
            let _ = events.send(AppEvent::ChartEncoded {
                entry,
                table,
                image_id,
                px: chart_layout.px,
                payload,
            });
        })
        .ok();
}

/// 每帧放置图像：在 `terminal.draw()` 之后调用。
///
/// 位置与内容都没变就跳过；kitty 换位时先删旧放置，永久消失的图像按图像 ID
/// 删放置，并让 [`crate::render::image::ImageStore`] 忘掉「已传输」记录。
/// sixel / iTerm2 消失或改尺寸时置 `image_repaint`，由事件循环清屏重绘，
/// 避免残影留在空白占位行上。浮层期间先把已贴的图撤下来，恢复后整批重放。
pub fn emit(app: &mut App, out: &mut impl Write) -> std::io::Result<()> {
    let Some(support) = app.graphics else {
        return Ok(());
    };
    let covered = image_covered(app);
    let mut wrote = false;
    if covered != app.image_covered {
        app.image_covered = covered;
        // 浮层期间不贴图：kitty 的图像独立于文本层且默认 z-index 在文字之上，
        // 「不放置」并不等于「撤下来」——已经贴上去的会直接盖住模态框。
        // 这里先删掉全部放置（数据保留），浮层消失后靠 `image_resume` 整批重放。
        if covered {
            app.image_resume = true;
            let previous = std::mem::take(&mut app.image_placed);
            for old in &previous {
                if old.proto == GraphicsProto::Kitty {
                    out.write_all(encode::kitty_hide(old.image_id, old.placement_id).as_bytes())?;
                    wrote = true;
                }
            }
            if wrote {
                out.flush()?;
            }
        }
    }
    if covered {
        return Ok(());
    }
    let resume = std::mem::take(&mut app.image_resume);
    let previous = std::mem::take(&mut app.image_placed);
    // 槽位表归 `App` 所有，下一帧 `render::draw` 会先 clear 再重填；这里只读不取走，
    // 免得 emit 之后还有人想读本帧登记的落位。
    let slots: Vec<ImageSlot> = app.image_slots.clone();
    let events = app.events.clone();
    let theme = app.theme;
    let mut placed: Vec<Placed> = Vec::with_capacity(slots.len());
    for slot in slots {
        // 载荷来源：表格图表后台异步光栅化与编码，通用图片走 `ImageStore` 的内容缓存
        let (payload, image_id, transmit) = match &slot.source {
            SlotSource::Chart { table } => {
                let Some(entry) = app.blocks.get_mut(slot.entry) else {
                    continue;
                };
                let Some(state) = entry.table_chart_mut(*table) else {
                    continue;
                };
                let Some(chart_layout) = layout(&state.spec, slot.cols, support.cell) else {
                    continue;
                };
                if let Some(payload) = state.payload.as_ref().filter(|_| state.px == chart_layout.px)
                {
                    let transmit = !state.transmitted;
                    state.transmitted = true;
                    (Arc::clone(payload), state.image_id, transmit)
                } else {
                    let already_rendering = state
                        .rendering
                        .is_some_and(|(px, _)| px == chart_layout.px);
                    if !already_rendering {
                        state.rendering = Some((chart_layout.px, std::time::Instant::now()));
                        spawn_chart_encode(
                            events.clone(),
                            slot.entry,
                            *table,
                            state.image_id,
                            state.spec.clone(),
                            chart_layout,
                            support.proto,
                            theme,
                        );
                    }
                    continue;
                }
            }
            SlotSource::Image(key) => {
                let Some(encoded) =
                    app.images
                        .payload(key, support.proto, slot.cols, slot.rows, slot.px, theme)
                else {
                    continue;
                };
                (encoded.payload, encoded.image_id, encoded.transmit)
            }
        };
        // 放置 ID 与图像 ID 分开：同一张图可以同时贴多处
        let reused = !transmit
            && !resume
            && previous.iter().any(|old| {
                old.placement_id == slot.placement_id
                    && old.image_id == image_id
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
                    out.write_all(encode::kitty_hide(image_id, slot.placement_id).as_bytes())?;
                    // 像素数据每份只传一次：换位、重复放置只需要重新放置
                    if transmit {
                        out.write_all(payload.as_bytes())?;
                    }
                    out.write_all(
                        encode::kitty_place(image_id, slot.placement_id, slot.cols, slot.rows)
                            .as_bytes(),
                    )?;
                }
                _ => out.write_all(payload.as_bytes())?,
            }
            wrote = true;
        }
        placed.push(Placed {
            image_id,
            placement_id: slot.placement_id,
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
        match placed
            .iter()
            .find(|now| now.placement_id == old.placement_id && now.image_id == old.image_id)
        {
            None => {
                if old.proto == GraphicsProto::Kitty {
                    // 同一张图还有别的放置就只删这一个，否则连图像数据一起删
                    let shared = placed.iter().any(|now| now.image_id == old.image_id);
                    let seq = if shared {
                        encode::kitty_hide(old.image_id, old.placement_id)
                    } else {
                        // 终端里这份像素数据没了：不记录的话滚回来只会重新放置
                        // 一个终端并不存在的图像，屏幕上留下一片空白。
                        app.images.forget_transmission(old.image_id);
                        for entry in &mut app.blocks {
                            for state in entry.charts.iter_mut().flatten() {
                                if state.image_id == old.image_id {
                                    state.transmitted = false;
                                }
                            }
                        }
                        encode::kitty_delete(old.image_id)
                    };
                    out.write_all(seq.as_bytes())?;
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
        app.image_repaint = true;
    }
    app.image_placed = placed;
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
mod tests;

#[cfg(test)]
mod emit_tests;

#[cfg(test)]
mod pipeline_tests;
