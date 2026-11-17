use super::*;
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn model_picker_anchor_stays_inside_terminal() {
    for (w, h) in [(120, 40), (80, 24), (40, 12), (20, 6)] {
        let screen = Rect::new(0, 0, w, h);
        for anchor in [
            Rect::new(1, h - 1, 4, 1),
            Rect::new(w - 5, h - 1, 4, 1),
            Rect::new(w - 5, 0, 4, 1),
        ] {
            let rect = popup_rect(screen, anchor, 20, false);
            assert_eq!(rect.intersection(screen), rect);
            assert!(rect.bottom() <= anchor.y || rect.y >= anchor.bottom());
            assert!(rect.height >= 4);
        }
    }
}

/// 背景不被压暗；列表卡底边贴模型标签行；详情卡与列表卡不相交；
/// 等级下拉打开时 6 档全部落在屏内；元信息变化不移动列表卡。
#[test]
fn model_picker_preserves_background_and_separates_detail_hits() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let entry = ModelEntry {
        selector: "p/model".into(),
        label: "p/model".into(),
        display_name: "模型示例".into(),
        levels: crate::modal::DEFAULT_LEVELS
            .iter()
            .map(|level| (*level).into())
            .collect(),
        context_window: Some(128_000),
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    };
    app.modal = Some(Modal::Model {
        entries: vec![entry],
        current: "p/model:max".into(),
        picker: Default::default(),
        level: Default::default(),
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    });
    app.model_tag_area = Rect::new(90, 28, 8, 1);
    let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
    terminal
        .draw(|frame| {
            frame
                .buffer_mut()
                .set_string(0, 0, "正文", Style::new().fg(Color::Red));
            super::super::draw_modal(frame, &mut app);
        })
        .unwrap();
    assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "正");
    assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Red);
    assert_eq!(app.modal_card_area.bottom(), app.model_tag_area.y);
    assert!(
        app.modal_detail_area
            .intersection(app.modal_card_area)
            .is_empty(),
        "详情卡与列表卡不相交"
    );

    // Tab 展开等级下拉的状态：6 档全部有命中区且都在屏内。
    if let Some(Modal::Model { detail, .. }) = app.modal.as_mut() {
        detail.menu = Some(crate::modal::MenuState {
            row: DetailRow::Level,
            selected: 5,
            offset: 0,
            scroll: Default::default(),
            query: String::new(),
        });
    }
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .unwrap();
    let screen = Rect::new(0, 0, 110, 30);
    assert_eq!(
        app.modal_menu_hits.len(),
        crate::modal::DEFAULT_LEVELS.len(),
        "等级下拉给出全部档位"
    );
    for (_, area) in &app.modal_menu_hits {
        assert_eq!(area.intersection(screen), *area, "下拉候选都在屏内");
    }

    let before = app.modal_card_area;
    if let Some(Modal::Model { entries, .. }) = &mut app.modal {
        entries[0].context_window = None;
    }
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .unwrap();
    assert_eq!(app.modal_card_area, before, "元信息变化不能移动浮层");
}

/// 详情卡 Level 行以 `›` 开头、完整显示最长的等级名，不被裁成省略号。
#[test]
fn detail_level_chip_shows_full_level_name() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let app = App::new(Theme::global(), events);
    let modal = Modal::Model {
        entries: vec![ModelEntry {
            selector: "p/model".into(),
            label: "p/model".into(),
            display_name: "Model".into(),
            levels: crate::modal::DEFAULT_LEVELS
                .iter()
                .map(|level| (*level).into())
                .collect(),
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
            fast_mode: false,
        }],
        current: "p/model:max".into(),
        picker: Default::default(),
        level: Default::default(),
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    };
    let (lines, hits) = detail_lines(&app, &modal, 34);
    let row = hits
        .iter()
        .find(|(row, _)| *row == DetailRow::Level)
        .map(|(_, line)| *line)
        .expect("思考等级控制行");
    let text: String = lines[row]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.starts_with("›"), "{text}");
    assert!(text.contains("max"), "{text}");
    assert!(!text.contains('…'), "{text}");
}
