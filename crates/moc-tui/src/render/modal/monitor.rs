use moc_i18n::t;

use super::MONITOR_CUR_W;
use super::MONITOR_LABEL_W;
use super::MONITOR_STATS_W;
use super::SPARK_CHARS;
use crate::render::wrap::str_width;
use crate::theme::Theme;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::time::Duration;
use std::time::Instant;

/// `/monitor` 正文：内存/CPU 画瞬时值，磁盘与网络各画读/写、收/发两条速率线。
pub(super) fn monitor_lines(width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme.gray_dim);
    let spark_cols = width.saturating_sub(MONITOR_LABEL_W + MONITOR_CUR_W + MONITOR_STATS_W + 3);
    if spark_cols < 4 {
        return vec![Line::from(Span::styled(
            t!("tui.monitor.too_narrow").into_owned(),
            dim,
        ))];
    }
    let samples = moc_core::procstats::samples();
    if samples.len() < 2 {
        return vec![Line::from(Span::styled(
            t!("tui.monitor.sampling").into_owned(),
            dim,
        ))];
    }
    let now = Instant::now();
    let window = Duration::from_secs(moc_core::procstats::WINDOW_SECS);
    let latest = *samples.last().expect("samples 已判非空");

    let row = |label: &str,
               color: Color,
               current: String,
               points: &[(Instant, f64)],
               stats: String|
     -> Line<'static> {
        let label_pad = " ".repeat(MONITOR_LABEL_W.saturating_sub(str_width(label)));
        let cur_pad = " ".repeat(MONITOR_CUR_W.saturating_sub(str_width(&current)));
        Line::from(vec![
            Span::styled(format!("{label}{label_pad}"), dim),
            Span::styled(
                format!("{cur_pad}{current}"),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled(
                format!(" {}", sparkline(points, spark_cols, window, now)),
                Style::new().fg(color),
            ),
            Span::styled(format!(" {stats}"), dim),
        ])
    };
    let summary = |points: &[(Instant, f64)]| {
        let (sum, peak) = points
            .iter()
            .fold((0.0, 0.0f64), |(sum, peak), (_, v)| (sum + v, peak.max(*v)));
        (sum / points.len().max(1) as f64, peak)
    };
    let instant = |field: fn(&moc_core::procstats::Sample) -> f64| -> Vec<(Instant, f64)> {
        samples.iter().map(|s| (s.at, field(s))).collect()
    };
    let rate_line = |label: &str,
                     color: Color,
                     field: fn(&moc_core::procstats::Sample) -> u64,
                     total: u64|
     -> Line<'static> {
        let points = moc_core::procstats::rates(&samples, field);
        let current = points.last().map(|(_, rate)| *rate).unwrap_or(0.0);
        row(
            label,
            color,
            format!("{}/s", format_bytes(current as u64)),
            &points,
            t!("tui.monitor.total_fmt", total = format_bytes(total)).into_owned(),
        )
    };

    let rss = instant(|s| s.rss_bytes as f64);
    let (avg, peak) = summary(&rss);
    let mut lines = vec![row(
        t!("tui.monitor.memory").as_ref(),
        theme.chart_1,
        format_bytes(latest.rss_bytes),
        &rss,
        t!(
            "tui.monitor.avg_peak_fmt",
            avg = format_bytes(avg as u64),
            peak = format_bytes(peak as u64)
        )
        .into_owned(),
    )];
    let cpu = instant(|s| f64::from(s.cpu_pct));
    let (avg, peak) = summary(&cpu);
    lines.push(row(
        "CPU",
        theme.chart_2,
        format!("{:.0}%", latest.cpu_pct),
        &cpu,
        t!(
            "tui.monitor.avg_peak_pct_fmt",
            avg = format!("{avg:.0}"),
            peak = format!("{peak:.0}")
        )
        .into_owned(),
    ));
    lines.push(Line::default());
    lines.push(rate_line(
        t!("tui.monitor.disk_down").as_ref(),
        theme.chart_3,
        |s| s.disk_read_bytes,
        latest.disk_read_bytes,
    ));
    lines.push(rate_line(
        t!("tui.monitor.disk_up").as_ref(),
        theme.chart_3,
        |s| s.disk_write_bytes,
        latest.disk_write_bytes,
    ));
    lines.push(Line::default());
    lines.push(rate_line(
        t!("tui.monitor.net_down").as_ref(),
        theme.chart_4,
        |s| s.net_rx_bytes,
        latest.net_rx_bytes,
    ));
    lines.push(rate_line(
        t!("tui.monitor.net_up").as_ref(),
        theme.chart_4,
        |s| s.net_tx_bytes,
        latest.net_tx_bytes,
    ));
    lines
}

/// 字节量人类可读：`512B`、`45KB`、`1.2GB`（1024 进制；不足 10 单位带一位小数）。
pub(super) fn format_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = value as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 || v >= 10.0 {
        format!("{v:.0}{}", UNITS[unit])
    } else {
        format!("{v:.1}{}", UNITS[unit])
    }
}

/// 折线：按时刻把值投进 `columns` 列的固定窗口桶（每桶取峰值，空桶空格），
/// 全序列按自身峰值归一到 8 档。points 按时刻升序。
pub(super) fn sparkline(
    points: &[(Instant, f64)],
    columns: usize,
    window: Duration,
    now: Instant,
) -> String {
    debug_assert!(columns > 0);
    let bucket = window.as_secs_f64() / columns as f64;
    let start = now.checked_sub(window).unwrap_or(now);
    let mut peaks = vec![0.0f64; columns];
    let mut filled = vec![false; columns];
    for &(at, value) in points {
        let col = (at.saturating_duration_since(start).as_secs_f64() / bucket) as usize;
        if col < columns {
            filled[col] = true;
            peaks[col] = peaks[col].max(value);
        }
    }
    let peak = peaks.iter().copied().fold(0.0, f64::max);
    let mut out = String::with_capacity(columns * 3);
    for col in 0..columns {
        if !filled[col] {
            out.push(SPARK_CHARS[0]);
        } else if peak <= 0.0 || peaks[col] <= 0.0 {
            out.push(SPARK_CHARS[1]);
        } else {
            let level = 1 + ((peaks[col] / peak) * 7.0).round().min(7.0) as usize;
            out.push(SPARK_CHARS[level]);
        }
    }
    out
}
