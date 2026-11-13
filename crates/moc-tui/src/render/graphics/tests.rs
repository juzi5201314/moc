use super::*;

#[test]
fn parses_cell_window_kitty_and_sixel_replies() {
    // 单元格 8x18、窗口 800x540、kitty OK、DA1 含 sixel 位 4
    let replies = parse_replies(b"\x1b[6;18;8t\x1b[4;540;800t\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c");
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

/// DECRPM 应答 `? 2026 ; Ps $ y`：Ps ∈ 1..=4 即支持同步输出（0 = 不认识）。
#[test]
fn parses_sync_output_report() {
    for ps in [1u8, 2, 3, 4] {
        let reply = format!("\x1b[?2026;{ps}$y");
        assert!(
            parse_replies(reply.as_bytes()).sync_output,
            "Ps={ps} 应判为支持"
        );
    }
    assert!(
        !parse_replies(b"\x1b[?2026;0$y").sync_output,
        "Ps=0 是「不认识该模式」"
    );
    assert!(!parse_replies(b"").sync_output);
}

#[test]
fn cell_spec_needs_both_dimensions_in_range() {
    assert_eq!(parse_cell("8x18"), Some(CellPixels { w: 8, h: 18 }));
    assert_eq!(parse_cell("10,20"), Some(CellPixels { w: 10, h: 20 }));
    assert_eq!(parse_cell("8x"), None);
    assert_eq!(parse_cell("0x18"), None);
    assert_eq!(parse_cell("8x999"), None);
}
