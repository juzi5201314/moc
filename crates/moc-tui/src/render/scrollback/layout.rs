use super::super::PROSE_INDENT;
use super::super::TITLE_INDENT;
use super::super::labels;
use super::super::wrap::clip_line;
use super::super::wrap::str_width;
use super::super::wrap::wrap_text;
use super::AGENT_MESSAGE_PREVIEW_ROWS;
use super::BRIEF_THINKING_ROWS;
use super::BatchIndex;
use super::EntryLines;
use super::TreeIndex;
use super::TreeLayout;
use super::TreeShape;
use super::block_layout;
use super::messages::agent_message_block;
use super::messages::agent_result_block;
use super::messages::assistant_message;
use super::messages::goal_round_block;
use super::messages::note_block;
use super::messages::thinking_block;
use super::messages::user_message;
use super::text::hang;
use super::tool_content::prefer_builtin_redirect;
use super::tool_content::shows_tool_body;
use super::tool_content::tool_body;
use super::tool_content::tool_body_rows;
use super::tool_content::tool_is_error;
use super::tool_summary::batch_member_tail;
use super::tool_summary::batch_summary_budget;
use super::tool_summary::title_summary;
use super::tool_summary::tool_summary_budget;
use super::tool_summary::tool_title;
use super::tool_summary::tool_title_tail;
use crate::app::App;
use crate::theme::Theme;
use crate::transcript::Block;
use crate::transcript::DetailLevel;
use crate::transcript::Entry;
use crate::transcript::ToolCallBlock;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

pub(super) fn estimate_entry_height_indexed(
    app: &App,
    index: usize,
    width: u16,
    batches: &BatchIndex,
    tree: &TreeIndex,
) -> usize {
    let Some(entry) = app.blocks.get(index) else {
        return 0;
    };
    // 含表格 / 图片 / mermaid 的正文不能用折行近似：表格列宽收缩、入口按钮、
    // 图表占位行、图片占位行和字符图的行数都是渲染期才算出来的，估错会让滚动
    // 范围与懒渲染的可见区间一起失真。
    if let Block::Assistant { text } = &entry.block
        && (may_contain_table(text) || may_contain_image(text) || may_contain_mermaid(text))
    {
        return render_entry_with(index, entry, app, width, false, batches, tree).height;
    }
    let level = entry.level(app.block_level(&entry.block));
    let layout = block_layout(batches, tree, index);
    let Some(tools) = batches.tools(index, app) else {
        return estimate_entry_height(entry, app, level, width, layout);
    };
    if tools.is_empty() {
        return 0;
    }
    estimate_tool_batch_height(app, &tools, level, usize::from(width), layout)
}

/// 成组渲染的行数：组头 + 每个成员的（摘要行 + 正文行）+ 尾随留白（树尾才有）。
///
/// 组只在组首块渲染，其余成员高度为零；`layout` 是最后一个成员的树布局。
pub(super) fn estimate_tool_batch_height(
    app: &App,
    tools: &[&ToolCallBlock],
    level: DetailLevel,
    width: usize,
    layout: TreeLayout,
) -> usize {
    // 成员都画在组头下一层：可用宽度固定为 width − 3×成员深度 − TITLE_INDENT。
    let member_depth = usize::from(member_shape(layout.shape, false).depth);
    let member_width = width.saturating_sub(3 * member_depth);
    let mut rows = 1 + usize::from(layout.gap && level != DetailLevel::Hidden);
    for tool in tools {
        rows += 1;
        if !shows_tool_body(tool, level) {
            continue;
        }
        let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        let tail = batch_member_tail(tool, app.theme);
        let budget = batch_summary_budget(&tail, member_width);
        let (_, in_title) = title_summary(tool, &args, budget, level, app.theme, app.cwd());
        rows += tool_body_rows(
            tool,
            &args,
            app,
            member_width.saturating_sub(TITLE_INDENT),
            level,
            in_title,
        );
    }
    rows
}

/// 懒块的估算高度：与各渲染分支同结构（标题行 + 内容行 + 尾注 + 尾随留白）。
///
/// 工具结果的截断规则、折叠态与展开态都与渲染共用同一套判定；正文按显示宽度折行
/// 近似 markdown 的落行数。估算与渲染差一个数量级会让滚动范围、滚动条与懒渲染的
/// 可见区间同时失真。
pub(super) fn estimate_entry_height(
    entry: &Entry,
    app: &App,
    level: DetailLevel,
    width: u16,
    layout: TreeLayout,
) -> usize {
    let width = usize::from(width).max(8);
    match &entry.block {
        // 纯文本折行很便宜：直接复用渲染，估高与渲染恒等。
        Block::User { text } => user_message(text, entry.at_ms, width, app.theme).len(),
        Block::Note { text, tone } => note_block(text, *tone, width, app.theme).len(),
        Block::Assistant { text } => wrapped_rows(text, prose_width(width))
            .saturating_add(usize::from(entry.elapsed_ms.is_some()))
            .saturating_add(1),
        Block::Thinking { text, .. } => {
            let width = prose_width(width);
            match level {
                DetailLevel::Hidden => 2,
                // 简略档跳过段间空行，超出预算再补一行省略提示
                DetailLevel::Brief => {
                    let rows: usize = text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| wrapped_rows(line, width))
                        .sum();
                    rows.min(BRIEF_THINKING_ROWS) + usize::from(rows > BRIEF_THINKING_ROWS) + 2
                }
                DetailLevel::Full => wrapped_rows(text, width).saturating_add(2),
            }
        }
        Block::Tool(tool) => estimate_tool_height(tool, app, level, width, layout),
        Block::AgentMessage { body, .. }
        | Block::AgentResult { body, .. }
        | Block::GoalRound { body, .. } => {
            // 与 `agent_message_block` 同源：简略档是预览行数 + 省略行，隐藏档只剩标题行。
            let rows = wrapped_text_rows(body, prose_width(width)).len();
            let (shown, omitted) = match level {
                DetailLevel::Hidden => (0, false),
                DetailLevel::Brief => (
                    rows.min(AGENT_MESSAGE_PREVIEW_ROWS),
                    rows > AGENT_MESSAGE_PREVIEW_ROWS,
                ),
                DetailLevel::Full => (rows, false),
            };
            shown + usize::from(omitted) + 2
        }
    }
}

/// 工具块高度：标题 + 正文行 + 尾随留白（树尾才有），与 [`render_tool`] 同规则。
pub(super) fn estimate_tool_height(
    tool: &ToolCallBlock,
    app: &App,
    level: DetailLevel,
    width: usize,
    layout: TreeLayout,
) -> usize {
    let body_visible = shows_tool_body(tool, level);
    let gap = usize::from(layout.gap && level != DetailLevel::Hidden);
    if !body_visible {
        return 1 + gap;
    }
    let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
    let nested = layout.shape.is_some();
    let depth = layout.shape.map_or(0, |shape| usize::from(shape.depth));
    let width = width.saturating_sub(3 * depth);
    let budget = tool_summary_budget(tool_title(tool, &args), tool, app.theme, width, nested);
    let (_, in_title) = title_summary(tool, &args, budget, level, app.theme, app.cwd());
    let indent = if nested { TITLE_INDENT } else { PROSE_INDENT };
    let body = tool_body_rows(
        tool,
        &args,
        app,
        width.saturating_sub(indent),
        level,
        in_title,
    );
    body + 1 + gap
}

/// `spawn_images = false` 用于估高：只查缓存，绝不触发视口外图片解码。
pub(super) fn render_entry_with(
    index: usize,
    entry: &Entry,
    app: &App,
    width: u16,
    spawn_images: bool,
    batches: &BatchIndex,
    tree: &TreeIndex,
) -> EntryLines {
    // 估高路径统一按「至少 8 列」计算宽度；渲染必须用同一口径，否则超窄
    // 终端下图片版式（可容纳列数 → 行数）在两侧会算出不同的块高。
    let width = width.max(8);
    let level = entry.level(app.block_level(&entry.block));
    let layout = block_layout(batches, tree, index);
    if let Some(tools) = batches.tools(index, app) {
        if tools.is_empty() {
            return EntryLines::plain(Vec::new());
        }
        return render_tool_batch(app, &tools, usize::from(width), level, spawn_images, layout);
    }
    render_entry_single(entry, app, usize::from(width), level, spawn_images, layout)
}

pub(super) fn render_entry_single(
    entry: &Entry,
    app: &App,
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    layout: TreeLayout,
) -> EntryLines {
    let theme = app.theme;
    match &entry.block {
        Block::User { text } => EntryLines::plain(user_message(text, entry.at_ms, width, theme)),
        Block::Assistant { text } => assistant_message(text, entry, app, width, spawn_images),
        Block::Thinking {
            text,
            reasoning_tokens,
            duration_ms,
        } => EntryLines::plain(thinking_block(
            text,
            *reasoning_tokens,
            *duration_ms,
            app,
            width,
            level,
        )),
        Block::Tool(tool) => render_tool(tool, app, width, level, spawn_images, layout),
        Block::AgentMessage { meta, body } => {
            EntryLines::plain(agent_message_block(meta, body, width, theme, level))
        }
        Block::AgentResult { meta, body } => {
            EntryLines::plain(agent_result_block(meta, body, width, theme, level))
        }
        Block::GoalRound { meta, body } => {
            EntryLines::plain(goal_round_block(meta, body, width, theme, level))
        }
        Block::Note { text, tone } => EntryLines::plain(note_block(text, *tone, width, theme)),
    }
}

/// 同名工具的成组渲染：顶层批次与嵌套连续同名子调用共用这棵树。
///
/// 组头 = bullet（仅顶层）+ 工具名 + ` (N)`；有失败或运行中的成员时紧接着补
/// `{done}/{N} done` 统计（失败、运行中另行点明）。成员画在组头下一层：
/// `{连线前缀}` + 摘要 + 状态尾注，正文悬挂一格与摘要同列。
/// `layout` 取组内最后一个成员的树布局（它的 `more` 与 `gap` 才代表整组）。
pub(super) fn render_tool_batch(
    app: &App,
    tools: &[&ToolCallBlock],
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    layout: TreeLayout,
) -> EntryLines {
    use moc_protocol::methods::ToolStatus;

    let theme = app.theme;
    let head = layout.shape;
    let name = tools.first().map_or("tool", |tool| tool.name.as_str());
    let head_width = width.saturating_sub(3 * head.map_or(0, |shape| usize::from(shape.depth)));
    let count = |pick: fn(&ToolCallBlock) -> bool| tools.iter().filter(|tool| pick(tool)).count();
    let done = count(|tool| tool.status == Some(ToolStatus::Completed));
    let failed = count(tool_is_error);
    let running = count(|tool| tool.running);
    let (bullet, bullet_color) = if running > 0 {
        ("▸", theme.accent_running)
    } else if failed == tools.len() {
        ("✘", theme.accent_error)
    } else {
        ("◆", theme.accent_tool)
    };
    let mut title = vec![Span::raw(" ".repeat(TITLE_INDENT))];
    if head.is_none() {
        title.push(Span::styled(
            format!("{bullet} "),
            Style::new().fg(bullet_color),
        ));
    }
    title.push(Span::styled(
        name.to_owned(),
        Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
    ));
    title.push(Span::styled(
        format!(" ({})", tools.len()),
        Style::new().fg(theme.gray_dim),
    ));
    // 组统计紧跟数量：只在有失败或运行中时显示。
    if failed > 0 || running > 0 {
        let mut tail = vec![Span::styled(
            format!("{done}/{} {}", tools.len(), labels::status::DONE),
            Style::new().fg(theme.gray_dim),
        )];
        if failed > 0 {
            tail.push(Span::styled(
                format!("{}{failed} {}", labels::SEP, labels::status::FAILED),
                Style::new().fg(theme.accent_error),
            ));
        }
        if running > 0 {
            tail.push(Span::styled(
                format!("{}{running} {}", labels::SEP, labels::status::RUNNING),
                Style::new().fg(theme.accent_running),
            ));
        }
        title.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
        title.extend(tail);
    }
    let mut lines = vec![clip_line(Line::from(title), head_width)];
    let rail = tree_rail_style(theme);
    if let Some(head) = head {
        // 嵌套组的组头按最后一棒的树形状接线（├─ 表示后面还有兄弟或父调用还在跑）。
        lines[0]
            .spans
            .insert(0, Span::styled(tree_prefix(head, true), rail));
    }
    let mut images = Vec::new();
    let last = tools.len() - 1;
    for (offset, tool) in tools.iter().enumerate() {
        let shape = member_shape(head, offset == last);
        let depth = usize::from(shape.depth);
        let width = width.saturating_sub(3 * depth);
        let args = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        let tail = batch_member_tail(tool, theme);
        let (summary, in_title) = title_summary(
            tool,
            &args,
            batch_summary_budget(&tail, width),
            level,
            theme,
            app.cwd(),
        );
        let has_summary = summary.iter().any(|span| !span.content.is_empty());
        let mut spans = vec![Span::raw(" ".repeat(TITLE_INDENT))];
        if has_summary {
            spans.extend(summary);
        }
        if !tail.is_empty() {
            if has_summary {
                spans.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
            }
            spans.extend(tail);
        }
        let mut member_lines = vec![clip_line(Line::from(spans), width)];
        let mut placements = Vec::new();
        if shows_tool_body(tool, level) {
            let body_width = width.saturating_sub(TITLE_INDENT);
            let (body, body_placements) =
                tool_body(tool, &args, app, body_width, level, spawn_images, in_title);
            let base = member_lines.len();
            member_lines.extend(
                body.into_iter()
                    .map(|line| hang(line, TITLE_INDENT, body_width)),
            );
            placements = body_placements;
            for placement in &mut placements {
                placement.row += base;
                placement.x = placement
                    .x
                    .saturating_add(u16::try_from(TITLE_INDENT + 3 * depth).unwrap_or(u16::MAX));
            }
        }
        apply_tree_prefix(&mut member_lines, shape, rail);
        lines.extend(member_lines);
        images.extend(placements);
    }
    // 树尾才补一行纯空行（无连线前缀）。
    if layout.gap && level != DetailLevel::Hidden {
        lines.push(Line::default());
    }
    EntryLines {
        height: lines.len(),
        paged: None,
        lines,
        tables: Vec::new(),
        links_button: None,
        images,
    }
}

/// 工具块：标题行 + 正文 + 尾随留白（树尾才有）。
///
/// 标题落在块标记列，耗时紧跟摘要（todo 是 `done/total` 进度），正文落在正文列；
/// 收起态（无正文、read 非完整档）只留标题行。嵌套调用（PTC）按 [`TreeShape`] 加
/// 连线前缀：子调用不画 bullet（连接符本身就是标记），正文与工具名首字对齐。
pub(super) fn render_tool(
    tool: &ToolCallBlock,
    app: &App,
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    layout: TreeLayout,
) -> EntryLines {
    let theme = app.theme;
    let nested = layout.shape.is_some();
    let depth = layout.shape.map_or(0, |shape| usize::from(shape.depth));
    let width = width.saturating_sub(3 * depth);
    let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);

    let (bullet, bullet_color) = match (tool.running, tool.status) {
        (true, _) => ("▸", theme.accent_running),
        (_, Some(moc_protocol::methods::ToolStatus::Completed)) => ("◆", theme.accent_tool),
        _ if prefer_builtin_redirect(tool) => ("→", theme.accent_warning),
        (_, Some(_)) => ("✘", theme.accent_error),
        (_, None) => ("◆", theme.accent_tool),
    };
    let name = tool_title(tool, &args);
    // 子调用没有 bullet，失败标记挪到尾列的 ✘（与批次成员同一套尾列）。
    let tail = if nested {
        batch_member_tail(tool, theme)
    } else {
        tool_title_tail(tool, theme)
    };
    let (summary, in_title) = title_summary(
        tool,
        &args,
        tool_summary_budget(name, tool, theme, width, nested),
        level,
        theme,
        app.cwd(),
    );
    let mut title = vec![Span::raw(" ".repeat(TITLE_INDENT))];
    if !nested {
        title.push(Span::styled(
            format!("{bullet} "),
            Style::new().fg(bullet_color),
        ));
    }
    title.push(Span::styled(
        name.to_owned(),
        Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
    ));
    let has_summary = summary.iter().any(|span| !span.content.is_empty());
    if has_summary {
        title.push(Span::raw(" "));
        title.extend(summary);
    }
    // 后台任务标记（job_id 只在任务于后台存活期间挂着）
    if let Some(job_id) = &tool.job_id {
        title.push(Span::styled(
            format!(" · 后台 {job_id}"),
            Style::new().fg(theme.gray_dim),
        ));
    }
    if !tail.is_empty() {
        title.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
        title.extend(tail);
    }
    let mut lines = vec![clip_line(Line::from(title), width)];
    let mut placements = Vec::new();
    if shows_tool_body(tool, level) {
        // 子调用正文与工具名首字对齐；顶层留在正文列。
        let indent = if nested { TITLE_INDENT } else { PROSE_INDENT };
        let body_width = width.saturating_sub(indent);
        let (body, body_placements) =
            tool_body(tool, &args, app, body_width, level, spawn_images, in_title);
        let base = lines.len();
        lines.extend(body.into_iter().map(|line| hang(line, indent, body_width)));
        placements = body_placements;
        for placement in &mut placements {
            placement.row += base;
            placement.x = placement
                .x
                .saturating_add(u16::try_from(indent).unwrap_or(u16::MAX));
        }
    }
    if let Some(shape) = layout.shape {
        // 嵌套调用画连线：一个空格 + 祖先层各 3 格 + 本层 2 格；
        // 子块自己行首的缩进空格就是连接符后面那一格。
        apply_tree_prefix(&mut lines, shape, tree_rail_style(theme));
        for placement in &mut placements {
            placement.x = placement
                .x
                .saturating_add(u16::try_from(3 * depth).unwrap_or(u16::MAX));
        }
    }
    // 隐藏档不加额外留白，警告仍显示；简略与手动展开沿用树尾留白。
    if layout.gap && level != DetailLevel::Hidden {
        lines.push(Line::default());
    }
    EntryLines {
        height: lines.len(),
        paged: None,
        lines,
        tables: Vec::new(),
        links_button: None,
        images: placements,
    }
}

/// 嵌套连线的行首前缀：一个空格 + 祖先层各 3 格（`│  `／`   `）+ 本层连接符
/// （首行 `├─`／`└─`，续行 `│ `／`  `）；行尾不留空格，空行由调用方 `trim_end`。
pub(super) fn tree_prefix(shape: TreeShape, first: bool) -> String {
    let mut prefix = String::from(" ");
    for level in 0..usize::from(shape.depth).saturating_sub(1) {
        prefix.push_str(if shape.rails & (1 << level) != 0 {
            "│  "
        } else {
            "   "
        });
    }
    prefix.push_str(match (first, shape.more) {
        (true, true) => "├─",
        (true, false) => "└─",
        (false, true) => "│ ",
        (false, false) => "  ",
    });
    prefix
}

/// 给一段行加树形连线前缀：首行连接符、续行竖线；空行只补 trim_end 后的竖线段。
pub(super) fn apply_tree_prefix(lines: &mut [Line<'static>], shape: TreeShape, rail: Style) {
    for (index, line) in lines.iter_mut().enumerate() {
        let prefix = tree_prefix(shape, index == 0);
        if line.spans.is_empty() {
            let prefix = prefix.trim_end();
            if !prefix.is_empty() {
                line.spans.push(Span::styled(prefix.to_owned(), rail));
            }
        } else {
            line.spans.insert(0, Span::styled(prefix, rail));
        }
    }
}

/// 批次成员的树形状：组头的下一层，`rails` 继承组头自己的 `more`，
/// 成员的 `more` 只看是否末位成员。
pub(super) fn member_shape(head: Option<TreeShape>, last: bool) -> TreeShape {
    match head {
        Some(head) => TreeShape {
            depth: head.depth.saturating_add(1).min(TreeIndex::MAX_DEPTH),
            rails: head.rails | u32::from(head.more) << (head.depth - 1),
            more: !last,
        },
        None => TreeShape {
            depth: 1,
            rails: 0,
            more: !last,
        },
    }
}

/// 树形连线的统一颜色：批次树、嵌套连线、todo 树、JSON 树共用。
///
/// `theme.border` 在 #141414 底上几乎看不见，统一用高一档的 `gray_dim`。
pub(super) fn tree_rail_style(theme: &Theme) -> Style {
    Style::new().fg(theme.gray_dim)
}

/// 块正文的可用宽度：去掉 [`PROSE_INDENT`]，右侧不再另留边距（滚动区已留 `MARGIN_X`）。
///
/// 所有块的正文（用户、助手、工具输出、IRC、Note、Todo 条目）都按这个宽度折行/截断，
/// 左缘与右缘因此只有一套网格。
pub(super) fn prose_width(width: usize) -> usize {
    width.saturating_sub(PROSE_INDENT)
}

/// 正文里是否可能出现 GFM 表格：分隔行（只有 `|`、`-`、`:`、空白且含 `-`）。
///
/// 只做廉价预筛，宁可多算（走一次真实渲染），不能漏判——漏判会破坏
/// 「估算高度 == 渲染高度」这条被测试钉住的不变量。
pub(super) fn may_contain_table(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains('-')
            && trimmed
                .chars()
                .all(|ch| matches!(ch, '|' | '-' | ':' | ' ' | '\t'))
    })
}

/// 正文里是否可能出现内联图片：`![](…)` 或 SVG 代码块。
///
/// 与 [`may_contain_table`] 同理：只做廉价预筛，宁可多渲染一次。
/// SVG 的判定必须与 [`crate::render::markdown`] 里实际出图的条件一致——
/// 围栏支持 ` ``` ` 与 `~~~`、info 前可有空格、大小写不敏感；只要这里漏判，
/// 懒块的估高就会用折行近似顶替真实占位行，滚动范围随之失真。
pub(super) fn may_contain_image(text: &str) -> bool {
    text.lines().any(|line| {
        if line.contains("![") {
            return true;
        }
        let trimmed = line.trim_start();
        let fence_len = trimmed
            .chars()
            .take_while(|ch| *ch == '`' || *ch == '~')
            .count();
        fence_len >= 3 && crate::render::markdown::is_svg_fence_info(&trimmed[fence_len..])
    })
}

/// 正文里是否可能出现 mermaid 围栏。口径与 [`may_contain_image`] 相同：漏判会让
/// 字符图的行数被折行近似顶替。
pub(super) fn may_contain_mermaid(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let fence_len = trimmed
            .chars()
            .take_while(|ch| *ch == '`' || *ch == '~')
            .count();
        fence_len >= 3 && crate::render::markdown::is_mermaid_fence_info(&trimmed[fence_len..])
    })
}

/// 多行文本按显示宽度折行：先按换行分行，再逐行折行（[`wrap_text`] 只折单行）。
pub(super) fn wrapped_text_rows(text: &str, width: usize) -> Vec<String> {
    text.lines()
        .flat_map(|line| wrap_text(line, width))
        .collect()
}

/// 文本按显示宽度的折行行数（空文本也算一行）。
pub(super) fn wrapped_rows(text: &str, width: usize) -> usize {
    let width = width.max(8);
    text.lines()
        .map(|line| (str_width(line).saturating_add(width - 1) / width).max(1))
        .sum::<usize>()
        .max(1)
}
