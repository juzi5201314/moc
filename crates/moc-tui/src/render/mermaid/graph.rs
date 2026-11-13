use super::super::wrap::str_width;
use super::super::wrap::wrap_text;
use super::ARROW_E;
use super::ARROW_N;
use super::ARROW_S;
use super::ARROW_W;
use super::COL_GAP;
use super::Dir;
use super::Graph;
use super::Grid;
use super::Ink;
use super::NODE_GAP;
use super::Place;
use super::ROW_GAP;
use super::Shape;

pub(super) fn layout_graph(graph: &Graph, width: usize) -> Option<Grid> {
    let ranks = rank_nodes(graph);
    let ranks = barycenter(graph, ranks);
    let vertical = matches!(graph.dir, Dir::Td | Dir::Bt);
    if vertical
        && let Some(grid) = layout_axis(graph, &ranks, width, true, matches!(graph.dir, Dir::Td))
    {
        return Some(grid);
    }
    if !vertical
        && let Some(grid) = layout_axis(graph, &ranks, width, false, matches!(graph.dir, Dir::Lr))
    {
        return Some(grid);
    }
    if !vertical && let Some(grid) = layout_axis(graph, &ranks, width, true, true) {
        return Some(grid);
    }
    Some(layout_column(graph, &ranks, width))
}

pub(super) fn rank_nodes(graph: &Graph) -> Vec<Vec<usize>> {
    let n = graph.nodes.len();
    let mut preds = vec![Vec::new(); n];
    for edge in &graph.edges {
        if edge.from != edge.to {
            preds[edge.to].push(edge.from);
        }
    }
    let mut memo = vec![None; n];
    let mut stack = vec![false; n];
    fn dfs(
        node: usize,
        preds: &[Vec<usize>],
        memo: &mut [Option<usize>],
        stack: &mut [bool],
    ) -> usize {
        if let Some(rank) = memo[node] {
            return rank;
        }
        if stack[node] {
            return 0;
        }
        stack[node] = true;
        let mut rank = 0;
        for &pred in &preds[node] {
            rank = rank.max(dfs(pred, preds, memo, stack) + 1);
        }
        stack[node] = false;
        memo[node] = Some(rank);
        rank
    }
    let mut rank = vec![0; n];
    for (node, slot) in rank.iter_mut().enumerate() {
        *slot = dfs(node, &preds, &mut memo, &mut stack);
    }
    let height = rank.iter().copied().max().unwrap_or(0);
    let mut groups = vec![Vec::new(); height + 1];
    for (node, group) in rank.into_iter().enumerate() {
        groups[group].push(node);
    }
    groups
}

pub(super) fn barycenter(graph: &Graph, mut ranks: Vec<Vec<usize>>) -> Vec<Vec<usize>> {
    let n = graph.nodes.len();
    let mut preds = vec![Vec::new(); n];
    for edge in &graph.edges {
        if edge.from != edge.to {
            preds[edge.to].push(edge.from);
        }
    }
    for _ in 0..4 {
        let mut pos = vec![0; n];
        for rank in &ranks {
            for (index, &node) in rank.iter().enumerate() {
                pos[node] = index;
            }
        }
        for rank in ranks.iter_mut().skip(1) {
            rank.sort_by(|&left, &right| {
                let left_key = average(&preds[left], &pos).unwrap_or(pos[left]);
                let right_key = average(&preds[right], &pos).unwrap_or(pos[right]);
                left_key.cmp(&right_key).then(left.cmp(&right))
            });
        }
    }
    ranks
}

pub(super) fn average(nodes: &[usize], pos: &[usize]) -> Option<usize> {
    if nodes.is_empty() {
        return None;
    }
    Some(nodes.iter().map(|&node| pos[node]).sum::<usize>() / nodes.len())
}

pub(super) fn rank_of(ranks: &[Vec<usize>]) -> Vec<usize> {
    let mut out = vec![0; ranks.iter().map(Vec::len).sum()];
    for (rank, nodes) in ranks.iter().enumerate() {
        for &node in nodes {
            out[node] = rank;
        }
    }
    out
}

/// `vertical` 为真时按行排，`forward` 为真时 rank 0 在起点一侧（上或左）。
pub(super) fn layout_axis(
    graph: &Graph,
    ranks: &[Vec<usize>],
    width: usize,
    vertical: bool,
    forward: bool,
) -> Option<Grid> {
    let mut order: Vec<&[usize]> = ranks.iter().map(Vec::as_slice).collect();
    if !forward {
        order.reverse();
    }
    let mut places = vec![
        Place {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            lines: Vec::new(),
        };
        graph.nodes.len()
    ];
    if vertical {
        place_vertical(graph, &order, width, &mut places)?;
    } else {
        place_horizontal(graph, &order, width, &mut places)?;
    }
    finish_graph(graph, ranks, &places, width)
}

pub(super) fn place_vertical(
    graph: &Graph,
    order: &[&[usize]],
    width: usize,
    places: &mut [Place],
) -> Option<()> {
    let mut y = 0;
    for rank in order {
        let mut measured = Vec::new();
        let mut total = 0;
        for &id in *rank {
            let lines = box_lines(&graph.nodes[id].label, graph.nodes[id].shape, width);
            let node_w = str_width(lines.first()?);
            if !measured.is_empty() {
                total += NODE_GAP;
            }
            total += node_w;
            measured.push((id, lines, node_w));
        }
        if total > width {
            return None;
        }
        let mut x = (width - total) / 2;
        let mut rank_h = 1;
        for (id, lines, node_w) in measured {
            let h = lines.len();
            rank_h = rank_h.max(h);
            places[id] = Place {
                x,
                y,
                w: node_w,
                h,
                lines,
            };
            x += node_w + NODE_GAP;
        }
        y += rank_h + ROW_GAP;
    }
    Some(())
}

pub(super) fn place_horizontal(
    graph: &Graph,
    order: &[&[usize]],
    width: usize,
    places: &mut [Place],
) -> Option<()> {
    let mut x = 0;
    for (index, rank) in order.iter().enumerate() {
        let mut measured = Vec::new();
        let mut col_w = 0;
        for &id in *rank {
            let lines = box_lines(&graph.nodes[id].label, graph.nodes[id].shape, width);
            let node_w = str_width(lines.first()?);
            col_w = col_w.max(node_w);
            measured.push((id, lines, node_w));
        }
        if x + col_w > width {
            return None;
        }
        let mut y = 0;
        for (id, lines, node_w) in measured {
            let h = lines.len();
            places[id] = Place {
                x: x + (col_w - node_w) / 2,
                y,
                w: node_w,
                h,
                lines,
            };
            y += h + 1;
        }
        x += col_w;
        if index + 1 != order.len() {
            if x + COL_GAP > width {
                return None;
            }
            x += COL_GAP;
        }
    }
    Some(())
}

pub(super) fn finish_graph(
    graph: &Graph,
    ranks: &[Vec<usize>],
    places: &[Place],
    width: usize,
) -> Option<Grid> {
    let rank = rank_of(ranks);
    let mut footers = Vec::new();
    let mut routed = Vec::new();
    for edge in &graph.edges {
        let from = &places[edge.from];
        let to = &places[edge.to];
        let adjacent = rank[edge.from].abs_diff(rank[edge.to]) == 1 && edge.from != edge.to;
        if adjacent && can_route(from, to) {
            routed.push((edge.from, edge.to, edge.directed));
            if !edge.label.is_empty() {
                footers.push(link_text(
                    &graph.nodes[edge.from].label,
                    &graph.nodes[edge.to].label,
                    &edge.label,
                    edge.directed,
                ));
            }
        } else {
            footers.push(link_text(
                &graph.nodes[edge.from].label,
                &graph.nodes[edge.to].label,
                &edge.label,
                edge.directed,
            ));
        }
    }
    let mut height = places
        .iter()
        .map(|place| place.y + place.h)
        .max()
        .unwrap_or(0);
    let footer_lines: Vec<String> = footers
        .iter()
        .flat_map(|line| wrap_text(line, width))
        .collect();
    if !footer_lines.is_empty() {
        height += 1 + footer_lines.len();
    }
    if height == 0 || height > 400 {
        return None;
    }
    let mut grid = Grid::new(width, height);
    for &(from, to, directed) in &routed {
        route(&mut grid, &places[from], &places[to], directed);
    }
    for place in places {
        for (row, line) in place.lines.iter().enumerate() {
            grid.put_str(place.x, place.y + row, line, Ink::Border);
            paint_label(&mut grid, place.x, place.y + row, line);
        }
    }
    for (row, line) in (height - footer_lines.len()..).zip(&footer_lines) {
        grid.put_str(0, row, line, Ink::Text);
    }
    Some(grid)
}

pub(super) fn paint_label(grid: &mut Grid, x: usize, y: usize, line: &str) {
    let mut col = x;
    let chars: Vec<char> = line.chars().collect();
    if chars.len() < 2 {
        return;
    }
    for (index, ch) in chars.iter().enumerate() {
        let width = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
        let edge = index == 0 || index + 1 == chars.len();
        if !edge && *ch != '─' && *ch != '◇' {
            grid.set_ink(col, y, Ink::Text);
        }
        col += width;
    }
}

pub(super) fn can_route(from: &Place, to: &Place) -> bool {
    let below = from.y + from.h < to.y;
    let above = to.y + to.h < from.y;
    let right = from.x + from.w < to.x;
    let left = to.x + to.w < from.x && from.x > 0;
    below || above || right || left
}

pub(super) fn route(grid: &mut Grid, from: &Place, to: &Place, directed: bool) {
    let x0 = from.x + from.w / 2;
    let x1 = to.x + to.w / 2;
    if from.y + from.h < to.y && to.y > 0 {
        route_south(grid, x0, from.y + from.h, x1, to.y - 1, directed);
        return;
    }
    if to.y + to.h < from.y {
        route_north(grid, x0, from.y, x1, to.y + to.h, directed);
        return;
    }
    let y0 = from.y + from.h / 2;
    let y1 = to.y + to.h / 2;
    if from.x + from.w < to.x && to.x > 0 {
        route_east(grid, from.x + from.w, y0, to.x - 1, y1, directed);
        return;
    }
    if to.x + to.w < from.x && from.x > 0 {
        route_west(grid, from.x - 1, y0, to.x + to.w, y1, directed);
    }
}

pub(super) fn route_south(
    grid: &mut Grid,
    x0: usize,
    y_start: usize,
    x1: usize,
    y_arrow: usize,
    directed: bool,
) {
    if y_arrow <= y_start {
        return;
    }
    let horiz = y_arrow - 1;
    grid.vline(x0, y_start, horiz);
    if x0 != x1 {
        grid.hline(x0, x1, horiz);
    }
    if directed {
        grid.arrow(x1, y_arrow, ARROW_S);
    } else {
        grid.vline(x1, horiz, y_arrow);
    }
}

pub(super) fn route_north(
    grid: &mut Grid,
    x0: usize,
    y_source: usize,
    x1: usize,
    y_arrow: usize,
    directed: bool,
) {
    if y_arrow >= y_source || y_source == 0 {
        return;
    }
    let horiz = y_arrow + 1;
    if horiz < y_source {
        grid.vline(x0, horiz, y_source - 1);
    }
    let cross = horiz.min(y_source - 1);
    if x0 != x1 {
        grid.hline(x0, x1, cross);
    }
    if directed {
        grid.arrow(x1, y_arrow, ARROW_N);
    } else {
        grid.vline(x1, y_arrow, cross);
    }
}

pub(super) fn route_east(
    grid: &mut Grid,
    x_from: usize,
    y_from: usize,
    x_arrow: usize,
    y_to: usize,
    directed: bool,
) {
    if x_from >= x_arrow {
        return;
    }
    let stop = if directed { x_arrow - 1 } else { x_arrow };
    if y_from == y_to {
        grid.hline(x_from, stop, y_from);
    } else {
        grid.vline(x_from, y_from, y_to);
        grid.hline(x_from, stop, y_to);
    }
    if directed {
        grid.arrow(x_arrow, y_to, ARROW_E);
    }
}

pub(super) fn route_west(
    grid: &mut Grid,
    x_from: usize,
    y_from: usize,
    x_arrow: usize,
    y_to: usize,
    directed: bool,
) {
    if x_arrow >= x_from {
        return;
    }
    let start = if directed { x_arrow + 1 } else { x_arrow };
    if y_from == y_to {
        grid.hline(start, x_from, y_from);
    } else {
        grid.vline(x_from, y_from, y_to);
        grid.hline(start, x_from, y_to);
    }
    if directed {
        grid.arrow(x_arrow, y_to, ARROW_W);
    }
}

pub(super) fn layout_column(graph: &Graph, ranks: &[Vec<usize>], width: usize) -> Grid {
    let order: Vec<usize> = ranks.iter().flatten().copied().collect();
    let mut places = Vec::new();
    let mut y = 0;
    for id in &order {
        let lines = box_lines(&graph.nodes[*id].label, graph.nodes[*id].shape, width);
        let node_w = str_width(lines.first().map(String::as_str).unwrap_or(""));
        let h = lines.len();
        let x = (width - node_w) / 2;
        places.push((
            *id,
            Place {
                x,
                y,
                w: node_w,
                h,
                lines,
            },
        ));
        y += h + ROW_GAP;
    }
    let rank = rank_of(ranks);
    // `Some(directed)` 标记该间隙要画连线：有向补箭头，无向补竖线。
    let mut linked = vec![None; order.len().saturating_sub(1)];
    let mut footers = Vec::new();
    for edge in &graph.edges {
        let from_at = order.iter().position(|&id| id == edge.from);
        let to_at = order.iter().position(|&id| id == edge.to);
        let consecutive = match (from_at, to_at) {
            (Some(from), Some(to)) => to == from + 1,
            _ => false,
        };
        if consecutive && rank[edge.from] <= rank[edge.to] {
            if let Some(from_at) = from_at {
                linked[from_at] = Some(edge.directed);
            }
            if !edge.label.is_empty() {
                footers.push(link_text(
                    &graph.nodes[edge.from].label,
                    &graph.nodes[edge.to].label,
                    &edge.label,
                    edge.directed,
                ));
            }
        } else {
            footers.push(link_text(
                &graph.nodes[edge.from].label,
                &graph.nodes[edge.to].label,
                &edge.label,
                edge.directed,
            ));
        }
    }
    let footer_lines: Vec<String> = footers
        .iter()
        .flat_map(|line| wrap_text(line, width))
        .collect();
    let mut height = places
        .last()
        .map(|(_, place)| place.y + place.h)
        .unwrap_or(0);
    if !footer_lines.is_empty() {
        height += 1 + footer_lines.len();
    }
    let mut grid = Grid::new(width, height.max(1));
    for (index, (_, place)) in places.iter().enumerate() {
        for (row, line) in place.lines.iter().enumerate() {
            grid.put_str(place.x, place.y + row, line, Ink::Border);
            paint_label(&mut grid, place.x, place.y + row, line);
        }
        if let Some(directed) = linked.get(index).copied().flatten() {
            let x = place.x + place.w / 2;
            let y = place.y + place.h;
            if y + 1 < grid.h {
                if directed {
                    grid.vline(x, y, y);
                    grid.arrow(x, y + 1, ARROW_S);
                } else {
                    grid.vline(x, y, y + 1);
                }
            }
        }
    }
    for (row, line) in (height - footer_lines.len()..).zip(&footer_lines) {
        grid.put_str(0, row, line, Ink::Text);
    }
    grid
}

pub(super) fn link_text(from: &str, to: &str, label: &str, directed: bool) -> String {
    let mark = if directed { "→" } else { "—" };
    if label.is_empty() {
        format!("  {from} {mark} {to}")
    } else {
        format!("  {from} {mark} {to}  {label}")
    }
}

pub(super) fn box_lines(label: &str, shape: Shape, max_width: usize) -> Vec<String> {
    if shape == Shape::Dot {
        let token = "(●)";
        let width = str_width(token);
        if width <= max_width {
            let blank = " ".repeat(width);
            return vec![blank.clone(), token.to_string(), blank];
        }
    }
    let max_inner = max_width.saturating_sub(2).max(1);
    let mut inner = wrap_text(label, max_inner);
    if inner.is_empty() {
        inner.push(String::new());
    }
    let inner_w = inner
        .iter()
        .map(|line| str_width(line))
        .max()
        .unwrap_or(1)
        .max(1);
    let (top_left, top_right, bottom_left, bottom_right, horizontal, vertical) = match shape {
        Shape::Round => ('╭', '╮', '╰', '╯', '─', '│'),
        Shape::Diamond => ('◇', '◇', '◇', '◇', '─', '│'),
        Shape::Box | Shape::Dot => ('┌', '┐', '└', '┘', '─', '│'),
    };
    let mut lines = Vec::with_capacity(inner.len() + 2);
    lines.push(format!(
        "{top_left}{}{top_right}",
        horizontal.to_string().repeat(inner_w)
    ));
    for line in &inner {
        let pad = inner_w - str_width(line);
        let left = pad / 2;
        let right = pad - left;
        lines.push(format!(
            "{vertical}{}{line}{}{vertical}",
            " ".repeat(left),
            " ".repeat(right)
        ));
    }
    lines.push(format!(
        "{bottom_left}{}{bottom_right}",
        horizontal.to_string().repeat(inner_w)
    ));
    lines
}
