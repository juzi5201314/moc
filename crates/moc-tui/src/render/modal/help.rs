use super::HELP_COLUMN_GAP;
use super::HELP_INDENT;
use super::HELP_KEY_GAP;
use super::HELP_KEY_MAX;
use super::HelpColumn;
use super::HelpSection;
use crate::app::App;
use crate::render::labels;
use crate::render::wrap::clip_line;
use crate::render::wrap::str_width;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// 快捷键一览：通用键位、输入与编辑、旁路交互三组，组标题加粗着色；
/// 末尾一行指路斜杠命令（输入 `/` 即见全表，不占本表篇幅）。
///
/// 每个分组独立测量键位列与描述列，再各自决定双列或单列：双列按组内自然列宽
/// 左右排布；单列把描述按剩余宽度折行、悬挂缩进到描述列，超过 [`HELP_KEY_MAX`]
/// 的键位独占一行。行尾不补空格，返回内容的自然宽度，模态据此收窄到刚好容纳。
/// 只产出正文；滚动窗口与底部提示由 `modal_body` 负责。
pub(super) fn help_lines(app: &App, max_width: usize) -> (Vec<Line<'static>>, usize) {
    let theme = app.theme;
    let get_key = |act: &str| -> String {
        app.preferences
            .keybindings
            .get(act)
            .map_or_else(|| "—".to_owned(), |keys| labels::keys(keys))
    };
    let fixed_keys =
        |list: &[&str]| labels::keys(&list.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>());
    let mut sections = vec![HelpSection {
        title: "会话与通用".to_owned(),
        rows: vec![
            (get_key("model"), "选择模型".to_owned()),
            (get_key("engine"), "切换会话引擎".to_owned()),
            (get_key("config"), "打开设置".to_owned()),
            (get_key("agents"), "唤出子代理侧栏".to_owned()),
            (get_key("resume"), "恢复会话".to_owned()),
            (get_key("thinking"), "切换思考显示档位".to_owned()),
            (get_key("thinking_level"), "切换思考等级".to_owned()),
            (get_key("tools"), "切换工具显示档位".to_owned()),
            (get_key("help"), "快捷键一览".to_owned()),
        ],
    }];
    sections.push(HelpSection {
        title: "输入与编辑".to_owned(),
        rows: vec![
            (get_key("send"), "发送消息".to_owned()),
            (get_key("queue"), "排队发送".to_owned()),
            (get_key("newline"), "插入换行".to_owned()),
            (get_key("cancel"), "停止当前生成 / 退出".to_owned()),
            (get_key("focus"), "切换输入焦点".to_owned()),
            (get_key("clear"), "清空输入".to_owned()),
            (get_key("line_start"), "移动到行首".to_owned()),
            (get_key("line_end"), "移动到行尾".to_owned()),
            (get_key("delete_word"), "删除前一词".to_owned()),
            (get_key("delete_to_end"), "删除至行尾".to_owned()),
            (get_key("paste_image"), "粘贴图片或文本".to_owned()),
            (get_key("queue_edit"), "编辑排队中的消息".to_owned()),
        ],
    });
    sections.push(HelpSection {
        title: "旁路交互 (BTW)".to_owned(),
        rows: vec![
            (
                fixed_keys(&["alt+b", "tab"]),
                "旁路展开与焦点切换".to_owned(),
            ),
            (
                fixed_keys(&["esc", "ctrl+c"]),
                "收起旁路 / 取消生成".to_owned(),
            ),
            (fixed_keys(&["c", "x"]), "复制回答 / 清空旁路".to_owned()),
        ],
    });
    let sections = sections;

    let mut lines = Vec::new();
    // 内容自然宽度：各分组实际排版宽度的最大值。
    let mut natural = 0usize;
    for (sec_idx, section) in sections.iter().enumerate() {
        if sec_idx > 0 {
            lines.push(Line::from(String::new()));
        }
        lines.push(Line::from(Span::styled(
            section.title.clone(),
            Style::new()
                .fg(theme.accent_warning)
                .add_modifier(Modifier::BOLD),
        )));

        // 每组各自测量：组内的长键位不撑宽其他组的键位列。
        let (key_max, desc_max) = measure_section(&section.rows);
        let key_width = key_max.min(HELP_KEY_MAX);
        let column: HelpColumn = (key_width, key_width + HELP_KEY_GAP + desc_max);
        // 双列要求所有键位都装得进列宽上限，且两份列宽加间距不超过可用宽度。
        let two_columns = key_max <= HELP_KEY_MAX
            && HELP_INDENT + column.1 + HELP_COLUMN_GAP + column.1 <= max_width;

        if two_columns {
            natural = natural.max(HELP_INDENT + column.1 + HELP_COLUMN_GAP + column.1);
            let split = section.rows.len().div_ceil(2);
            for index in 0..split {
                let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
                spans.extend(help_cell(&section.rows[index], column, theme));
                if let Some(right) = section.rows.get(split + index) {
                    spans.push(Span::styled(" ".repeat(HELP_COLUMN_GAP), Style::new()));
                    spans.extend(help_cell(right, column, theme));
                }
                lines.push(Line::from(spans));
            }
            continue;
        }

        // 单列：描述按剩余宽度折行、悬挂缩进到描述列；行尾不补空格。
        natural = natural.max(HELP_INDENT + (key_width + HELP_KEY_GAP + desc_max).max(key_max));
        let desc_x = HELP_INDENT + key_width + HELP_KEY_GAP;
        let desc_w = max_width.saturating_sub(desc_x).max(1);
        for (key, description) in &section.rows {
            let mut parts = crate::render::wrap::wrap_text(description, desc_w).into_iter();
            let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
            spans.push(Span::styled(
                key.clone(),
                Style::new().fg(theme.accent_user),
            ));
            if str_width(key) <= key_width {
                // 键位装进列宽：首段描述与键位同行。
                spans.push(Span::styled(
                    " ".repeat(key_width - str_width(key) + HELP_KEY_GAP),
                    Style::new(),
                ));
                spans.push(Span::styled(
                    parts.next().unwrap_or_default(),
                    Style::new().fg(theme.text_secondary),
                ));
            }
            lines.push(Line::from(spans));
            // 超长键位独占一行；其余段落统一悬挂缩进到描述列。
            for part in parts {
                lines.push(Line::from(vec![
                    Span::styled(" ".repeat(desc_x), Style::new()),
                    Span::styled(part, Style::new().fg(theme.text_secondary)),
                ]));
            }
        }
    }
    // 末尾指路斜杠命令：输入 `/` 即见全表，不放进本表。
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "输入 / 查看全部斜杠命令",
        Style::new().fg(theme.gray_dim),
    )));
    // 极窄终端连单列都放不下时截断，卡片不二次折行。
    let lines = lines
        .into_iter()
        .map(|line| clip_line(line, max_width))
        .collect();
    (lines, natural.min(max_width))
}

/// 一组快捷键的自然宽度：最长键位与最长描述（格）。
pub(super) fn measure_section(rows: &[(String, String)]) -> (usize, usize) {
    let key = rows
        .iter()
        .map(|(key, _)| str_width(key))
        .max()
        .unwrap_or(0);
    let description = rows
        .iter()
        .map(|(_, description)| str_width(description))
        .max()
        .unwrap_or(0);
    (key, description)
}

/// 帮助表的一格：键位右补齐到列内 `key_width`，整格再补齐到列宽。
pub(super) fn help_cell(
    row: &(String, String),
    column: HelpColumn,
    theme: &crate::theme::Theme,
) -> [Span<'static>; 4] {
    let (key_width, column_width) = column;
    let (key, description) = row;
    let key_cells = str_width(key);
    let description_cells = str_width(description);
    // 键位先补齐到 key_width，整格再补齐到列宽，同一列的描述才能上下对齐。
    let pad = column_width.saturating_sub(key_width + HELP_KEY_GAP + description_cells);
    [
        Span::styled(key.clone(), Style::new().fg(theme.accent_user)),
        Span::styled(
            " ".repeat(key_width.saturating_sub(key_cells) + HELP_KEY_GAP),
            Style::new(),
        ),
        Span::styled(
            (*description).to_owned(),
            Style::new().fg(theme.text_secondary),
        ),
        Span::styled(" ".repeat(pad), Style::new()),
    ]
}
