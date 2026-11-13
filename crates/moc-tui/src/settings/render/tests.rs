use super::*;
use crate::settings::controls::PickAction;
use crate::settings::editors::SelectEditor;
use crate::settings::interaction::scroll;
use crate::settings::{EditState, EditTarget, SettingsPanel, View};
use moc_protocol::ConfigScope;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use serde_json::json;
use std::collections::BTreeSet;

/// 单选弹窗：卡片不透底，候选行可点击，卡片外点击关闭，底下的行不再响应。
#[test]
fn select_popup_is_opaque_and_modal() {
    let theme = Theme::global();
    let mut panel = SettingsPanel {
        id: 1,
        cwd: "/tmp".into(),
        scope: ConfigScope::Global,
        data: None,
        tui: json!({}),
        effective: json!({}),
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: 0,
        selected: 0,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
        advanced: false,
        filter: String::new(),
        edit: Some(EditState {
            target: EditTarget::Filter,
            editor: Editor::Select(SelectEditor::new(
                vec![controls::Pick {
                    label: "选项A".into(),
                    action: PickAction::Set(json!("a")),
                }],
                &Value::Null,
            )),
            popup: true,
            title: "选择选项".into(),
            note: String::new(),
        }),
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Providers::default(),
        history: Default::default(),
        outcome: None,
        restore: None,
    };

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    terminal
        .draw(|frame| {
            frame
                .buffer_mut()
                .set_style(Rect::new(0, 0, 80, 24), Style::new().bg(Color::Red));
            draw(frame, &mut panel, false, theme);
        })
        .expect("绘制成功");

    let buffer = terminal.backend().buffer();
    // 验证中心弹窗卡片位置不漏底
    let center = buffer.cell((40, 12)).expect("中心格");
    assert_ne!(center.bg, Color::Reset, "弹窗不应为 Reset");
    assert_ne!(center.bg, Color::Red, "弹窗不应漏出底层红色");
    assert!(center.bg == theme.bg_base || center.bg == theme.bg_visual);

    let item = panel
        .hits
        .iter()
        .find(|(_, hit)| matches!(hit, Hit::Item(0)))
        .map(|(area, _)| *area)
        .expect("候选行可点击");
    let row: String = (item.x..item.right())
        .map(|x| buffer.cell((x, item.y)).expect("候选格").symbol())
        .collect();
    // 宽字符后的占位格是空格，比较前去掉
    assert!(
        row.replace(' ', "").contains("选项A"),
        "点击区域落在候选行上：{row}"
    );
    // 只剩弹窗自己的候选、底部键位与卡片外关闭；底下页面的命中区全部清掉。
    assert!(
        panel
            .hits
            .iter()
            .all(|(_, hit)| matches!(hit, Hit::Item(_) | Hit::Hint(..) | Hit::DismissPopup))
    );
    assert!(
        panel.hits.iter().any(|(area, hit)| {
            matches!(hit, Hit::DismissPopup) && area.contains((0, 0).into())
        })
    );
}

/// 读入 `document` 并停在 `tab` 页的面板（字段目录由 core 补齐）。
fn loaded(tab: &str, document: Value, fields: Vec<ConfigField>) -> SettingsPanel {
    let mut panel = SettingsPanel {
        id: 1,
        cwd: "/tmp".into(),
        scope: ConfigScope::Global,
        data: Some(SettingsDocument {
            path: String::new(),
            revision: String::new(),
            document: json!({}),
            effective: document.clone(),
            inherited: document,
            defaults: Value::Null,
            fields,
            provider_template: Value::Null,
            model_template: Value::Null,
        }),
        tui: json!({}),
        effective: Value::Null,
        fields: Vec::new(),
        controls: Vec::new(),
        rows: Vec::new(),
        collapsed: BTreeSet::new(),
        expanded: BTreeSet::new(),
        tab: TABS
            .iter()
            .position(|name| *name == tab)
            .expect("选项卡存在"),
        selected: 0,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
        advanced: false,
        filter: String::new(),
        edit: None,
        error: None,
        message: None,
        busy: false,
        hits: Vec::new(),
        pending: None,
        view: View::Fields,
        providers: Providers::default(),
        history: Default::default(),
        outcome: None,
        restore: None,
    };
    panel.rebuild();
    panel
}

/// 绘制一帧，返回每行的可见文本与每个字符的起始列（宽字符尾格不计）。
fn screen(panel: &mut SettingsPanel, width: u16, height: u16) -> Vec<(String, Vec<u16>)> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| draw(frame, panel, false, Theme::global()))
        .expect("绘制成功");
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let mut text = String::new();
            let mut columns = Vec::new();
            let mut x = 0;
            while x < width {
                let symbol = buffer.cell((x, y)).expect("格").symbol();
                for _ in symbol.chars() {
                    columns.push(x);
                }
                text.push_str(symbol);
                x += u16::try_from(str_width(symbol).max(1)).expect("单格宽度");
            }
            (text, columns)
        })
        .collect()
}

/// `needle` 在第几行、从第几列开始。
fn locate(rows: &[(String, Vec<u16>)], needle: &str) -> Option<(usize, u16)> {
    rows.iter().enumerate().find_map(|(y, (text, columns))| {
        let index = text.find(needle)?;
        Some((y, columns[text[..index].chars().count()]))
    })
}

/// 「模型」页常见的配置：模型角色 + Fusion 组合。
fn models_config() -> Value {
    json!({"models": {
        "roles": {"default": "p/m", "explore": "inherit", "plan": "none"},
        "fusion": {"lead": "p/m", "sidekick": "p/m"}
    }})
}

fn select_group(panel: &mut SettingsPanel, key: &str) {
    panel.selected = panel
        .rows
        .iter()
        .position(|row| matches!(&row.kind, RowKind::Group(group) if group.key == key))
        .expect("分组行存在");
}

/// 不可删除的分组也要按自己的分组路径给说明，不能统统落到模型角色那句。
#[test]
fn group_description_follows_grouping_path() {
    let mut panel = loaded("模型角色", models_config(), Vec::new());
    select_group(&mut panel, "models/fusion");
    let rows = screen(&mut panel, 120, 40);
    assert!(
        locate(&rows, "会话使用的各类模型").is_none(),
        "Fusion 分组不得显示模型角色说明"
    );
    assert!(locate(&rows, "执行模型负责实现与测试").is_some());

    select_group(&mut panel, "models/roles");
    let rows = screen(&mut panel, 120, 40);
    assert!(locate(&rows, "会话使用的各类模型").is_some());
}

/// 工具栏放不下时换行，按钮一个都不能丢。
#[test]
fn toolbar_wraps_instead_of_dropping_buttons() {
    let mut panel = loaded("模型角色", models_config(), Vec::new());
    screen(&mut panel, 80, 30);
    let keys: Vec<(KeyCode, KeyModifiers)> = panel
        .hits
        .iter()
        .filter_map(|(_, hit)| match hit {
            Hit::Key(code, modifiers) => Some((*code, *modifiers)),
            _ => None,
        })
        .collect();
    for expected in [
        (KeyCode::Char('c'), KeyModifiers::NONE),
        (KeyCode::Char('d'), KeyModifiers::CONTROL),
    ] {
        assert!(
            keys.contains(&expected),
            "工具栏丢了 {expected:?}：{keys:?}"
        );
    }
    panel.select_tab(crate::settings::providers::TAB);
    screen(&mut panel, 80, 30);
    for expected in ['n', 'a', 'f'] {
        assert!(
            panel.hits.iter().any(|(_, hit)| matches!(hit,
                Hit::Key(KeyCode::Char(key), KeyModifiers::NONE) if *key == expected
            )),
            "统一页面丢了 {expected} 操作"
        );
    }
}

/// 底部提示条的单键项可点：命中区落在画出来的那段文字上，且是编辑态也生效的 `Hint`。
#[test]
fn hint_bar_keys_are_clickable_where_drawn() {
    let mut panel = loaded("模型角色", models_config(), Vec::new());
    select_group(&mut panel, "models/roles");
    let rows = screen(&mut panel, 120, 40);
    let (y, x) = locate(&rows, "Esc 返回").expect("提示条有 Esc 返回");
    let hit = panel
        .hits
        .iter()
        .find_map(|(area, hit)| match hit {
            Hit::Hint(KeyCode::Esc, KeyModifiers::NONE) => Some(*area),
            _ => None,
        })
        .expect("Esc 返回 登记为提示条键位");
    let y = u16::try_from(y).expect("行号");
    assert_eq!((hit.y, hit.x, hit.width), (y, x, 8));
}

/// 超宽的值以 `…` 收尾，且不越过右边距。
#[test]
fn long_values_end_with_ellipsis_inside_margin() {
    let path = vec!["zz".to_owned(), "long".to_owned()];
    let field = ConfigField {
        path: path.clone(),
        tab: "模型".into(),
        label: "超长取值".into(),
        description: "测试用".into(),
        kind: FieldKind::Text,
        advanced: false,
    };
    let document = json!({"zz": {"long": "x".repeat(200)}});
    let mut panel = loaded("模型角色", document, vec![field]);
    panel.select_path(&path);
    let rows = screen(&mut panel, 80, 30);
    let (y, _) = locate(&rows, "超长取值").expect("字段行可见");
    let (text, columns) = &rows[y];
    let end = text.trim_end();
    assert!(end.ends_with('…'), "超宽值应以 … 收尾：{text:?}");
    let last = columns[end.chars().count() - 1];
    assert!(
        last < 80 - crate::render::MARGIN_X,
        "值越过右边距：止于第 {last} 列"
    );
}

/// 模型候选三种状态的前缀等宽，模型 ID 落在同一列。
#[test]
fn model_candidates_align_ids() {
    let mut panel = loaded(
        "供应商与模型",
        json!({"providers": {"p": {"models": {"a1": {}}}}}),
        Vec::new(),
    );
    panel.providers.discovery = Some(crate::settings::providers::Discovery {
        generation: 1,
        rows: ["a1", "b2", "c3"]
            .into_iter()
            .map(|id| moc_protocol::CatalogModel {
                id: id.into(),
                name: id.into(),
            })
            .collect(),
        checked: BTreeSet::from(["b2".to_owned()]),
        query: String::new(),
        error: None,
        pending: None,
    });
    panel.rebuild_rows();
    panel.select_action(ProviderAction::Candidate(0));
    let rows = screen(&mut panel, 100, 40);
    let column = |id| locate(&rows, id).expect("候选行可见").1;
    assert_eq!(column("a1"), column("b2"));
    assert_eq!(column("b2"), column("c3"));
}

/// MCP 服务分组行右侧显示启动方式。
#[test]
fn mcp_group_shows_server_address() {
    let mut panel = loaded(
        "MCP",
        json!({"mcp": {"servers": {"srv": {"command": "npx"}}}}),
        Vec::new(),
    );
    let rows = screen(&mut panel, 120, 40);
    assert!(locate(&rows, "srv").is_some(), "MCP 分组可见");
    assert!(locate(&rows, "本地程序：npx").is_some());
}

#[test]
fn provider_workspace_and_draft_fit_narrow_terminals() {
    let mut panel = loaded(
        "供应商与模型",
        json!({"providers": {"empty": {"models": {}}}}),
        Vec::new(),
    );
    for (width, height) in [(120, 40), (80, 24), (60, 24), (40, 20)] {
        let rows = screen(&mut panel, width, height);
        assert!(
            locate(&rows, "empty").is_some(),
            "当前供应商必须可见：{width}x{height}"
        );
        assert!(
            panel
                .hits
                .iter()
                .any(|(area, hit)| area.y < height - 3 && matches!(hit, Hit::Row(_))),
            "详情必须留有操作空间"
        );
        panel.new_provider();
        panel.providers.draft.as_mut().unwrap().key.input =
            PromptState::from_text("secret-must-stay-hidden".into());
        let rows = screen(&mut panel, width, height);
        assert!(
            !rows
                .iter()
                .any(|(text, _)| text.contains("secret-must-stay-hidden"))
        );
        for index in 0..6 {
            panel.providers.draft.as_mut().unwrap().focus = index;
            screen(&mut panel, width, height);
            assert!(
                panel.hits.iter().any(|(area, hit)| area.y < height - 3
                    && matches!(hit, Hit::ProviderInput(i) if *i == index)),
                "表单控件 {index} 在 {width}x{height} 必须可见"
            );
        }
        panel.providers.draft = None;
    }
}

/// 字段树的滚动条与主界面同一字形，贴终端右缘。
#[test]
fn field_scrollbar_matches_main_screen() {
    let mut panel = loaded("模型角色", models_config(), Vec::new());
    let rows = screen(&mut panel, 80, 16);
    assert!(
        rows.iter().any(|(text, _)| text.ends_with('▐')),
        "滚动条应在最右列：{rows:?}"
    );
}

/// 一帧的真实缓冲区（`screen` 只给文本，这里要按格看底色）。
fn buffer_of(panel: &mut SettingsPanel, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| draw(frame, panel, false, Theme::global()))
        .expect("绘制成功");
    terminal.backend().buffer().clone()
}

/// 「界面」页的一组平铺字段，行数足以溢出正文区。
fn flat_fields(count: usize) -> Vec<ConfigField> {
    (0..count)
        .map(|index| ConfigField {
            path: vec!["ui".into(), format!("f{index}")],
            tab: "界面".into(),
            label: format!("字段{index}"),
            description: "测试用".into(),
            kind: FieldKind::Text,
            advanced: false,
        })
        .collect()
}

/// 滚轮只滚视口：选中项不动，普通重绘也不把手动滚到的位置拉回。
#[test]
fn wheel_scrolls_fields_viewport_not_selection() {
    let mut panel = loaded("界面", json!({}), flat_fields(20));
    screen(&mut panel, 80, 16);
    assert!(panel.scroll.max_offset > 0, "行数应溢出正文区");

    scroll(&mut panel, (10, 10).into(), 6);
    assert_eq!(panel.selected, 0, "滚轮不得移动选中项");
    assert_eq!(panel.offset, 6.min(panel.scroll.max_offset));
    assert!(!panel.scroll.reveal_selection);

    let offset = panel.offset;
    screen(&mut panel, 80, 16);
    assert_eq!(panel.offset, offset, "普通重绘不得拉回手动滚到的位置");
}

/// 悬停只铺底色：不改选中行，选中行保留自己的色带。
#[test]
fn hover_paints_background_without_moving_selection() {
    let theme = Theme::global();
    let mut panel = loaded("界面", json!({}), flat_fields(20));
    screen(&mut panel, 80, 24);
    let (row_y, row_x) = {
        let (area, _) = panel
            .hits
            .iter()
            .find(|(area, hit)| {
                matches!(hit, Hit::Row(index) if *index != panel.selected) && area.height == 1
            })
            .expect("有未选中的行");
        (area.y, area.x)
    };
    let selected_y = panel
        .hits
        .iter()
        .find_map(|(area, hit)| match hit {
            Hit::Row(index) if *index == panel.selected => Some(area.y),
            _ => None,
        })
        .expect("选中行可见");
    panel.pointer = Some((row_x, row_y).into());

    let buffer = buffer_of(&mut panel, 80, 24);
    assert_eq!(panel.selected, 0, "悬停不得移动选中项");
    assert_eq!(buffer.cell((row_x + 2, row_y)).unwrap().bg, theme.bg_hover);
    let selected = buffer.cell((row_x + 2, selected_y)).unwrap();
    assert_eq!(selected.bg, theme.bg_visual, "选中行保留选中色带");
    assert_ne!(selected.bg, theme.bg_hover);
}

/// 供应商页滚轮按指针所在子区分流：侧栏滚动不动详情列表与侧栏光标。
#[test]
fn wheel_routes_to_provider_sidebar_not_detail() {
    let providers: serde_json::Map<String, Value> = (0..30)
        .map(|index| (format!("p{index:02}"), json!({"models": {}})))
        .collect();
    let mut panel = loaded(
        "供应商与模型",
        json!({ "providers": Value::Object(providers) }),
        Vec::new(),
    );
    screen(&mut panel, 100, 16);
    let point = panel
        .hits
        .iter()
        .find_map(|(area, hit)| match hit {
            Hit::Provider(_) => Some(*area),
            _ => None,
        })
        .map(|area| ratatui::layout::Position::new(area.x, area.y))
        .expect("侧栏行存在");
    assert!(panel.providers.scroll.max_offset > 0, "侧栏应溢出");

    scroll(&mut panel, point, 6);
    assert_eq!(
        panel.providers.offset,
        6.min(panel.providers.scroll.max_offset)
    );
    assert_eq!(panel.providers.cursor, 0, "滚轮不得移动侧栏光标");
    assert_eq!(panel.offset, 0, "详情列表的视口不得跟着滚动");
    assert!(!panel.providers.scroll.reveal_selection);

    let offset = panel.providers.offset;
    screen(&mut panel, 100, 16);
    assert_eq!(panel.providers.offset, offset, "普通重绘不得拉回侧栏");
}

/// 弹窗列表滚轮只滚自己的视口，光标行不动。
#[test]
fn popup_wheel_scrolls_viewport_not_cursor() {
    let mut panel = loaded("界面", json!({}), flat_fields(1));
    panel.edit = Some(EditState {
        target: EditTarget::Filter,
        editor: Editor::Select(SelectEditor::new(
            (0..30)
                .map(|index| controls::Pick {
                    label: format!("候选{index}"),
                    action: PickAction::Set(json!(index)),
                })
                .collect(),
            &Value::Null,
        )),
        popup: true,
        title: "选择".into(),
        note: String::new(),
    });
    screen(&mut panel, 80, 24);
    let Editor::Select(editor) = &panel.edit.as_ref().unwrap().editor else {
        panic!("是单选编辑器");
    };
    assert!(editor.scroll.max_offset > 0, "候选应溢出弹窗");

    scroll(&mut panel, (40, 12).into(), 3);
    let Editor::Select(editor) = &panel.edit.as_ref().unwrap().editor else {
        panic!("是单选编辑器");
    };
    assert_eq!(editor.offset, 3.min(editor.scroll.max_offset));
    assert_eq!(editor.selected, 0, "滚轮不得移动弹窗光标");
    assert!(!editor.scroll.reveal_selection);

    let offset = editor.offset;
    screen(&mut panel, 80, 24);
    let Editor::Select(editor) = &panel.edit.as_ref().unwrap().editor else {
        panic!("是单选编辑器");
    };
    assert_eq!(editor.offset, offset, "普通重绘不得拉回弹窗视口");
}

#[test]
fn test_format_number_commas() {
    assert_eq!(super::values::format_number(0), "0");
    assert_eq!(super::values::format_number(999), "999");
    assert_eq!(super::values::format_number(1000), "1,000");
    assert_eq!(super::values::format_number(32000), "32,000");
    assert_eq!(super::values::format_number(200000), "200,000");
    assert_eq!(super::values::format_number(10000000), "10,000,000");
}
