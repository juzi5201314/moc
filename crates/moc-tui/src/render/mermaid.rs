//! Mermaid 围栏 → 字符图。
//!
//! 终端里没有浏览器，也不要求本机有 Node。这里只画流程图、时序图和状态图；
//! 其余图种返回 `None`，调用方保持代码块。颜色只用主题里已有的槽。

use std::collections::HashMap;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::modal::ellipsize;
use super::wrap::{str_width, wrap_text};
use crate::theme::Theme;

const MAX_NODES: usize = 40;
const MAX_EDGES: usize = 80;
const ROW_GAP: usize = 2;
const NODE_GAP: usize = 2;
const COL_GAP: usize = 3;

const N: u8 = 0b0001;
const E: u8 = 0b0010;
const S: u8 = 0b0100;
const W: u8 = 0b1000;
const ARROW_N: u8 = 0b0001_0000;
const ARROW_E: u8 = 0b0010_0000;
const ARROW_S: u8 = 0b0100_0000;
const ARROW_W: u8 = 0b1000_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Td,
    Bt,
    Lr,
    Rl,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Box,
    Round,
    Diamond,
    Dot,
}

struct Graph {
    dir: Dir,
    nodes: Vec<GNode>,
    edges: Vec<GEdge>,
}

struct GNode {
    label: String,
    shape: Shape,
}

struct GEdge {
    from: usize,
    to: usize,
    label: String,
}

enum Diagram {
    Graph(Graph),
    Sequence(Sequence),
}

struct Sequence {
    actors: Vec<String>,
    events: Vec<SeqEvent>,
}

enum SeqEvent {
    Message {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
    },
    Note(String),
    Open(String),
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ink {
    None,
    Border,
    Text,
    Arrow,
}

struct Cell {
    ch: char,
    ink: Ink,
    skip: bool,
}

struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
    mask: Vec<u8>,
}

#[derive(Clone)]
struct Place {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    lines: Vec<String>,
}

/// 画出支持的图。宽度是正文列数。空图、超限或认不出的图种返回 `None`。
pub fn render(source: &str, width: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let width = width.max(8);
    let grid = match parse(source)? {
        Diagram::Graph(graph) => layout_graph(&graph, width)?,
        Diagram::Sequence(sequence) => layout_sequence(&sequence, width)?,
    };
    let lines = grid.into_lines(theme);
    if lines.is_empty() { None } else { Some(lines) }
}

fn parse(source: &str) -> Option<Diagram> {
    let mut lines = source
        .lines()
        .map(strip_comment)
        .filter(|line| !line.is_empty());
    let header = lines.next()?.trim().to_string();
    let kind = header_kind(&header)?;
    match kind {
        Header::Flow(dir) | Header::State(dir) => {
            let mut graph = Graph {
                dir,
                nodes: Vec::new(),
                edges: Vec::new(),
            };
            let mut ids: HashMap<String, usize> = HashMap::new();
            for line in lines {
                let line = line.trim();
                if line.is_empty() || skippable(line) {
                    continue;
                }
                if let Some(dir) = line.strip_prefix("direction ").and_then(parse_dir) {
                    graph.dir = dir;
                    continue;
                }
                if let Some((id, label)) = state_alias(line) {
                    upsert(&mut graph, &mut ids, &id, &label, Shape::Box, true);
                    continue;
                }
                for statement in split_semicolons(line) {
                    feed_statement(&mut graph, &mut ids, statement);
                }
            }
            if graph.nodes.is_empty()
                || graph.nodes.len() > MAX_NODES
                || graph.edges.len() > MAX_EDGES
            {
                return None;
            }
            Some(Diagram::Graph(graph))
        }
        Header::Sequence => Some(Diagram::Sequence(parse_sequence(lines)?)),
    }
}

enum Header {
    Flow(Dir),
    State(Dir),
    Sequence,
}

fn header_kind(line: &str) -> Option<Header> {
    let mut parts = line.split_whitespace();
    let word = parts.next()?.to_ascii_lowercase();
    let dir = parts.next().and_then(parse_dir).unwrap_or(Dir::Td);
    match word.as_str() {
        "flowchart" | "graph" => Some(Header::Flow(dir)),
        "statediagram" | "statediagram-v2" => Some(Header::State(dir)),
        "sequencediagram" => Some(Header::Sequence),
        _ => None,
    }
}

fn parse_dir(token: &str) -> Option<Dir> {
    match token.to_ascii_uppercase().as_str() {
        "TD" | "TB" => Some(Dir::Td),
        "BT" => Some(Dir::Bt),
        "LR" => Some(Dir::Lr),
        "RL" => Some(Dir::Rl),
        _ => None,
    }
}

fn skippable(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "end"
        || lower.starts_with("classdef ")
        || lower.starts_with("class ")
        || lower.starts_with("style ")
        || lower.starts_with("click ")
        || lower.starts_with("linkstyle ")
        || lower.starts_with("subgraph")
        || lower.starts_with("autonumber")
        || lower.starts_with("activate ")
        || lower.starts_with("deactivate ")
        || (lower.starts_with("state ") && lower.ends_with('{'))
}

fn state_alias(line: &str) -> Option<(String, String)> {
    let rest = strip_prefix_ignore_case(line, "state ")?;
    let rest = rest.trim_start();
    let (label, after) = if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        (&stripped[..end], stripped[end + 1..].trim_start())
    } else {
        return None;
    };
    let id = after.strip_prefix("as ")?.trim();
    if id.is_empty() {
        None
    } else {
        Some((id.to_string(), label.to_string()))
    }
}

fn feed_statement(graph: &mut Graph, ids: &mut HashMap<String, usize>, statement: &str) {
    let mut rest = statement.trim();
    if rest.is_empty() {
        return;
    }
    let Some(mut sources) = node_group(graph, ids, &mut rest) else {
        return;
    };
    while let Some(mut label) = arrow(&mut rest) {
        let Some(targets) = node_group(graph, ids, &mut rest) else {
            break;
        };
        if label.is_empty() {
            label = colon_label(&mut rest);
        }
        for &from in &sources {
            for &to in &targets {
                graph.edges.push(GEdge {
                    from,
                    to,
                    label: label.clone(),
                });
            }
        }
        sources = targets;
    }
}

fn node_group(
    graph: &mut Graph,
    ids: &mut HashMap<String, usize>,
    rest: &mut &str,
) -> Option<Vec<usize>> {
    let mut ids_out = Vec::new();
    loop {
        ids_out.push(one_node(graph, ids, rest)?);
        let trimmed = rest.trim_start();
        if let Some(next) = trimmed.strip_prefix('&') {
            *rest = next;
            continue;
        }
        *rest = trimmed;
        break;
    }
    Some(ids_out)
}

fn one_node(graph: &mut Graph, ids: &mut HashMap<String, usize>, rest: &mut &str) -> Option<usize> {
    let trimmed = rest.trim_start();
    if let Some(next) = trimmed.strip_prefix("[*]") {
        *rest = next;
        return Some(upsert(graph, ids, "[*]", "[*]", Shape::Dot, false));
    }
    let (id, after_id) = read_id(trimmed)?;
    let after_id = after_id.trim_start();
    let (shape, label, after) = if let Some(parsed) = read_shape(after_id) {
        parsed
    } else {
        (Shape::Box, id.to_string(), after_id)
    };
    *rest = after;
    let explicit = label != id;
    Some(upsert(graph, ids, id, &label, shape, explicit))
}

fn read_id(text: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            end = index + ch.len_utf8();
            continue;
        }
        if ch == '-' {
            let rest = &text[index..];
            if rest.starts_with("-->")
                || rest.starts_with("---")
                || rest.starts_with("-.->")
                || rest.starts_with("->")
                || rest.starts_with("==>")
            {
                break;
            }
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if end == 0 {
        None
    } else {
        Some((&text[..end], &text[end..]))
    }
}

fn read_shape(text: &str) -> Option<(Shape, String, &str)> {
    if let Some(text) = text.strip_prefix("[[") {
        let (inner, rest) = take_close(text, '[', ']')?;
        let (inner, rest) = take_close(inner, '[', ']').unwrap_or((inner, rest));
        // `[[label]]`：第一次 take 从 `label]]` 收到第一个 `]`，inner 是 label，rest 以 `]` 开头。
        let rest = rest.strip_prefix(']').unwrap_or(rest);
        return Some((Shape::Box, clean_label(inner), rest));
    }
    if text.starts_with("([") {
        let (inner, rest) = take_close(&text[1..], '[', ']')?;
        let rest = rest.strip_prefix(')').unwrap_or(rest);
        return Some((Shape::Round, clean_label(inner), rest));
    }
    if text.starts_with("((") {
        let (inner, rest) = take_close(text, '(', ')')?;
        let label = clean_label(inner.trim().trim_start_matches('(').trim_end_matches(')'));
        return Some((Shape::Round, label, rest));
    }
    if text.starts_with('[') {
        let (inner, rest) = take_close(text, '[', ']')?;
        return Some((Shape::Box, clean_label(inner), rest));
    }
    if text.starts_with('(') {
        let (inner, rest) = take_close(text, '(', ')')?;
        return Some((Shape::Round, clean_label(inner), rest));
    }
    if text.starts_with("{{") {
        let (inner, rest) = take_close(text, '{', '}')?;
        let label = clean_label(inner.trim().trim_start_matches('{').trim_end_matches('}'));
        return Some((Shape::Diamond, label, rest));
    }
    if text.starts_with('{') {
        let (inner, rest) = take_close(text, '{', '}')?;
        return Some((Shape::Diamond, clean_label(inner), rest));
    }
    if text.starts_with('>') {
        let end = text.find(']')?;
        return Some((Shape::Box, clean_label(&text[1..end]), &text[end + 1..]));
    }
    None
}

fn take_close(text: &str, open: char, close: char) -> Option<(&str, &str)> {
    if !text.starts_with(open) {
        return None;
    }
    let mut depth = 0;
    let mut quote = false;
    for (index, ch) in text.char_indices() {
        if quote {
            if ch == '"' {
                quote = false;
            }
            continue;
        }
        if ch == '"' {
            quote = true;
            continue;
        }
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some((
                    &text[open.len_utf8()..index],
                    &text[index + close.len_utf8()..],
                ));
            }
        }
    }
    None
}

fn clean_label(text: &str) -> String {
    let text = text.trim().trim_matches('"').trim();
    let mut out = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            continue;
        }
        if !in_tag {
            out.push(ch);
        }
    }
    out
}

fn upsert(
    graph: &mut Graph,
    ids: &mut HashMap<String, usize>,
    id: &str,
    label: &str,
    shape: Shape,
    explicit: bool,
) -> usize {
    if let Some(&index) = ids.get(id) {
        if explicit {
            graph.nodes[index].label = label.to_string();
            graph.nodes[index].shape = shape;
        }
        return index;
    }
    let index = graph.nodes.len();
    ids.insert(id.to_string(), index);
    let shown = if id == "[*]" { "●" } else { label };
    let shape = if id == "[*]" { Shape::Dot } else { shape };
    graph.nodes.push(GNode {
        label: shown.to_string(),
        shape,
    });
    index
}

fn arrow(rest: &mut &str) -> Option<String> {
    let trimmed = rest.trim_start();
    let (label, consumed) = arrow_at(trimmed)?;
    *rest = &trimmed[consumed..];
    Some(label)
}

fn arrow_at(text: &str) -> Option<(String, usize)> {
    if let Some(rest) = text.strip_prefix("-->") {
        return Some(labeled_pipe(rest, 3));
    }
    if text.starts_with("-.->") {
        return Some((String::new(), 4));
    }
    if let Some(rest) = text.strip_prefix("==>") {
        return Some(labeled_pipe(rest, 3));
    }
    if text.starts_with("---") {
        return Some((String::new(), 3));
    }
    if let Some(rest) = text.strip_prefix("--")
        && let Some(index) = rest.find("-->")
    {
        let label = rest[..index].trim().to_string();
        if !label.is_empty() {
            return Some((label, 2 + index + 3));
        }
    }
    if let Some(rest) = text.strip_prefix("==")
        && let Some(index) = rest.find("==>")
    {
        let label = rest[..index].trim().to_string();
        if !label.is_empty() {
            return Some((label, 2 + index + 3));
        }
    }
    if let Some(rest) = text.strip_prefix("-.")
        && let Some(index) = rest.find(".->")
    {
        let label = rest[..index].trim().to_string();
        return Some((label, 2 + index + 3));
    }
    None
}

fn labeled_pipe(rest: &str, base: usize) -> (String, usize) {
    let trimmed = rest.trim_start();
    let spaces = rest.len() - trimmed.len();
    if let Some(inner) = trimmed.strip_prefix('|')
        && let Some(end) = inner.find('|')
    {
        return (inner[..end].to_string(), base + spaces + 1 + end + 1);
    }
    (String::new(), base)
}

fn colon_label(rest: &mut &str) -> String {
    let trimmed = rest.trim_start();
    let Some(label) = trimmed.strip_prefix(':') else {
        return String::new();
    };
    *rest = "";
    label.trim().to_string()
}

fn split_semicolons(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth: i32 = 0;
    let mut quote = false;
    for (index, ch) in line.char_indices() {
        if quote {
            if ch == '"' {
                quote = false;
            }
            continue;
        }
        match ch {
            '"' => quote = true,
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                parts.push(&line[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&line[start..]);
    parts
}

fn parse_sequence<'a>(lines: impl Iterator<Item = &'a str>) -> Option<Sequence> {
    let mut actors: Vec<String> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut events = Vec::new();
    let ensure =
        |actors: &mut Vec<String>, index: &mut HashMap<String, usize>, name: &str| -> usize {
            let name = name.trim().trim_matches('"');
            if let Some(&found) = index.get(name) {
                return found;
            }
            let id = actors.len();
            index.insert(name.to_string(), id);
            actors.push(name.to_string());
            id
        };
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // `end` 也在图的跳过表里，时序图要先把它收成一组的结尾。
        if equals_ignore_case(line, "end") {
            events.push(SeqEvent::Close);
            continue;
        }
        if skippable(line) {
            continue;
        }
        if let Some(rest) = strip_prefix_ignore_case(line, "participant ")
            .or_else(|| strip_prefix_ignore_case(line, "actor "))
        {
            let (id, label) = split_as(rest);
            let slot = ensure(&mut actors, &mut index, id);
            if label != id {
                actors[slot] = label.to_string();
            }
            continue;
        }
        if strip_prefix_ignore_case(line, "note ").is_some() {
            let text = line
                .split_once(':')
                .map(|(_, text)| text.trim())
                .unwrap_or(line);
            events.push(SeqEvent::Note(text.to_string()));
            continue;
        }
        if is_group_open(line) {
            events.push(SeqEvent::Open(line.to_string()));
            continue;
        }
        if let Some(message) = parse_message(line) {
            let from = ensure(&mut actors, &mut index, message.0);
            let to = ensure(&mut actors, &mut index, message.1);
            let (from, to) = if message.4 { (to, from) } else { (from, to) };
            events.push(SeqEvent::Message {
                from,
                to,
                text: message.2,
                dashed: message.3,
            });
        }
    }
    if actors.is_empty() || actors.len() > 12 || events.len() > MAX_EDGES {
        None
    } else {
        Some(Sequence { actors, events })
    }
}

fn split_as(text: &str) -> (&str, &str) {
    if let Some((id, label)) = text.split_once(" as ") {
        (id.trim(), label.trim().trim_matches('"'))
    } else {
        let id = text.trim();
        (id, id)
    }
}

fn is_group_open(line: &str) -> bool {
    const WORDS: &[&str] = &[
        "loop", "alt", "opt", "par", "critical", "break", "rect", "else",
    ];
    let word = line.split_whitespace().next().unwrap_or("");
    WORDS
        .iter()
        .any(|candidate| equals_ignore_case(word, candidate))
}

/// `(from, to, text, dashed, reverse)`。
fn parse_message(line: &str) -> Option<(&str, &str, String, bool, bool)> {
    const TOKENS: &[(&str, bool, bool)] = &[
        ("-->>", true, false),
        ("->>", false, false),
        ("--)", true, false),
        ("-->", true, false),
        ("<<--", true, true),
        ("<<-", false, true),
        ("->", false, false),
        ("-)", true, false),
    ];
    let mut best: Option<(usize, &str, bool, bool)> = None;
    for &(token, dashed, reverse) in TOKENS {
        if let Some(index) = line.find(token) {
            let replace = match best {
                None => true,
                Some((at, previous, _, _)) => {
                    index < at || (index == at && token.len() > previous.len())
                }
            };
            if replace {
                best = Some((index, token, dashed, reverse));
            }
        }
    }
    let (index, token, dashed, reverse) = best?;
    let from = line[..index].trim();
    let rest = line[index + token.len()..].trim();
    let (to, text) = rest.split_once(':').unwrap_or((rest, ""));
    let to = to.trim();
    if from.is_empty() || to.is_empty() {
        return None;
    }
    Some((from, to, text.trim().to_string(), dashed, reverse))
}

fn layout_graph(graph: &Graph, width: usize) -> Option<Grid> {
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

fn rank_nodes(graph: &Graph) -> Vec<Vec<usize>> {
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
    for node in 0..n {
        rank[node] = dfs(node, &preds, &mut memo, &mut stack);
    }
    let height = rank.iter().copied().max().unwrap_or(0);
    let mut groups = vec![Vec::new(); height + 1];
    for (node, group) in rank.into_iter().enumerate() {
        groups[group].push(node);
    }
    groups
}

fn barycenter(graph: &Graph, mut ranks: Vec<Vec<usize>>) -> Vec<Vec<usize>> {
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

fn average(nodes: &[usize], pos: &[usize]) -> Option<usize> {
    if nodes.is_empty() {
        return None;
    }
    Some(nodes.iter().map(|&node| pos[node]).sum::<usize>() / nodes.len())
}

fn rank_of(ranks: &[Vec<usize>]) -> Vec<usize> {
    let mut out = vec![0; ranks.iter().map(Vec::len).sum()];
    for (rank, nodes) in ranks.iter().enumerate() {
        for &node in nodes {
            out[node] = rank;
        }
    }
    out
}

/// `vertical` 为真时按行排，`forward` 为真时 rank 0 在起点一侧（上或左）。
fn layout_axis(
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

fn place_vertical(
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

fn place_horizontal(
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

fn finish_graph(
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
            routed.push((edge.from, edge.to));
            if !edge.label.is_empty() {
                footers.push(link_text(
                    &graph.nodes[edge.from].label,
                    &graph.nodes[edge.to].label,
                    &edge.label,
                ));
            }
        } else {
            footers.push(link_text(
                &graph.nodes[edge.from].label,
                &graph.nodes[edge.to].label,
                &edge.label,
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
    for &(from, to) in &routed {
        route(&mut grid, &places[from], &places[to]);
    }
    for place in places {
        for (row, line) in place.lines.iter().enumerate() {
            grid.put_str(place.x, place.y + row, line, Ink::Border);
            paint_label(&mut grid, place.x, place.y + row, line);
        }
    }
    if !footer_lines.is_empty() {
        let mut y = height - footer_lines.len();
        for line in &footer_lines {
            grid.put_str(0, y, line, Ink::Text);
            y += 1;
        }
    }
    Some(grid)
}

fn paint_label(grid: &mut Grid, x: usize, y: usize, line: &str) {
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

fn can_route(from: &Place, to: &Place) -> bool {
    let below = from.y + from.h < to.y;
    let above = to.y + to.h < from.y;
    let right = from.x + from.w < to.x;
    let left = to.x + to.w < from.x && from.x > 0;
    below || above || right || left
}

fn route(grid: &mut Grid, from: &Place, to: &Place) {
    let x0 = from.x + from.w / 2;
    let x1 = to.x + to.w / 2;
    if from.y + from.h < to.y && to.y > 0 {
        route_south(grid, x0, from.y + from.h, x1, to.y - 1);
        return;
    }
    if to.y + to.h < from.y {
        route_north(grid, x0, from.y, x1, to.y + to.h);
        return;
    }
    let y0 = from.y + from.h / 2;
    let y1 = to.y + to.h / 2;
    if from.x + from.w < to.x && to.x > 0 {
        route_east(grid, from.x + from.w, y0, to.x - 1, y1);
        return;
    }
    if to.x + to.w < from.x && from.x > 0 {
        route_west(grid, from.x - 1, y0, to.x + to.w, y1);
    }
}

fn route_south(grid: &mut Grid, x0: usize, y_start: usize, x1: usize, y_arrow: usize) {
    if y_arrow <= y_start {
        return;
    }
    let horiz = y_arrow - 1;
    grid.vline(x0, y_start, horiz);
    if x0 != x1 {
        grid.hline(x0, x1, horiz);
    }
    grid.arrow(x1, y_arrow, ARROW_S);
}

fn route_north(grid: &mut Grid, x0: usize, y_source: usize, x1: usize, y_arrow: usize) {
    if y_arrow >= y_source || y_source == 0 {
        return;
    }
    let horiz = y_arrow + 1;
    if horiz < y_source {
        grid.vline(x0, horiz, y_source - 1);
    }
    if x0 != x1 {
        grid.hline(x0, x1, horiz.min(y_source - 1));
    }
    grid.arrow(x1, y_arrow, ARROW_N);
}

fn route_east(grid: &mut Grid, x_from: usize, y_from: usize, x_arrow: usize, y_to: usize) {
    if x_from >= x_arrow {
        return;
    }
    if y_from == y_to {
        grid.hline(x_from, x_arrow - 1, y_from);
    } else {
        grid.vline(x_from, y_from, y_to);
        grid.hline(x_from, x_arrow - 1, y_to);
    }
    grid.arrow(x_arrow, y_to, ARROW_E);
}

fn route_west(grid: &mut Grid, x_from: usize, y_from: usize, x_arrow: usize, y_to: usize) {
    if x_arrow >= x_from {
        return;
    }
    if y_from == y_to {
        grid.hline(x_arrow + 1, x_from, y_from);
    } else {
        grid.vline(x_from, y_from, y_to);
        grid.hline(x_arrow + 1, x_from, y_to);
    }
    grid.arrow(x_arrow, y_to, ARROW_W);
}

fn layout_column(graph: &Graph, ranks: &[Vec<usize>], width: usize) -> Grid {
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
    let mut linked = vec![false; order.len().saturating_sub(1)];
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
                linked[from_at] = true;
            }
            if !edge.label.is_empty() {
                footers.push(link_text(
                    &graph.nodes[edge.from].label,
                    &graph.nodes[edge.to].label,
                    &edge.label,
                ));
            }
        } else {
            footers.push(link_text(
                &graph.nodes[edge.from].label,
                &graph.nodes[edge.to].label,
                &edge.label,
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
        if linked.get(index).copied().unwrap_or(false) {
            let x = place.x + place.w / 2;
            let y = place.y + place.h;
            if y + 1 < grid.h {
                grid.vline(x, y, y);
                grid.arrow(x, y + 1, ARROW_S);
            }
        }
    }
    if !footer_lines.is_empty() {
        let mut y = height - footer_lines.len();
        for line in &footer_lines {
            grid.put_str(0, y, line, Ink::Text);
            y += 1;
        }
    }
    grid
}

fn link_text(from: &str, to: &str, label: &str) -> String {
    if label.is_empty() {
        format!("  {from} → {to}")
    } else {
        format!("  {from} → {to}  {label}")
    }
}

fn box_lines(label: &str, shape: Shape, max_width: usize) -> Vec<String> {
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

fn layout_sequence(sequence: &Sequence, width: usize) -> Option<Grid> {
    let count = sequence.actors.len();
    if count == 0 || count > 12 {
        return None;
    }
    let gap = 2;
    let mut col_w = sequence
        .actors
        .iter()
        .map(|label| str_width(label).clamp(3, 24))
        .max()
        .unwrap_or(4);
    while count * col_w + (count - 1) * gap > width && col_w > 3 {
        col_w -= 1;
    }
    let total = count * col_w + (count - 1) * gap;
    if total > width {
        return None;
    }
    let origin = (width - total) / 2;
    let center = |index: usize| origin + index * (col_w + gap) + col_w / 2;
    let labels: Vec<String> = sequence
        .actors
        .iter()
        .map(|label| ellipsize(label, col_w))
        .collect();
    let mut rows: Vec<SeqRow> = vec![SeqRow::Header, SeqRow::Life];
    for event in &sequence.events {
        match event {
            SeqEvent::Message {
                from,
                to,
                text,
                dashed,
            } if from == to => {
                rows.push(SeqRow::SelfTop { actor: *from });
                rows.push(SeqRow::SelfMid {
                    actor: *from,
                    text: text.clone(),
                });
                rows.push(SeqRow::SelfBot { actor: *from });
            }
            SeqEvent::Message {
                from,
                to,
                text,
                dashed,
            } => rows.push(SeqRow::Message {
                from: *from,
                to: *to,
                text: text.clone(),
                dashed: *dashed,
            }),
            SeqEvent::Note(text) => {
                for line in wrap_text(&format!("note: {text}"), width) {
                    rows.push(SeqRow::Text(line));
                }
            }
            SeqEvent::Open(title) => rows.push(SeqRow::Text(format!("┌ {title}"))),
            SeqEvent::Close => rows.push(SeqRow::Text("└".to_string())),
        }
    }
    if rows.len() > 400 {
        return None;
    }
    let mut grid = Grid::new(width, rows.len().max(1));
    for (y, row) in rows.iter().enumerate() {
        match row {
            SeqRow::Header => {
                for (index, label) in labels.iter().enumerate() {
                    let x =
                        origin + index * (col_w + gap) + col_w.saturating_sub(str_width(label)) / 2;
                    grid.put_str(x, y, label, Ink::Text);
                }
            }
            SeqRow::Life => {
                for index in 0..count {
                    grid.put_str(center(index), y, "│", Ink::Border);
                }
            }
            SeqRow::Message {
                from,
                to,
                text,
                dashed,
            } => {
                draw_message(
                    &mut grid,
                    y,
                    center(*from),
                    center(*to),
                    text,
                    *dashed,
                    width,
                );
            }
            SeqRow::SelfTop { actor } => {
                let x = center(*actor);
                grid.put_str(x, y, "│─┐", Ink::Border);
            }
            SeqRow::SelfMid { actor, text } => {
                let x = center(*actor);
                grid.put_str(x, y, "│ │", Ink::Border);
                let label_x = (x + 4).min(width.saturating_sub(1));
                grid.put_str(label_x, y, text, Ink::Text);
            }
            SeqRow::SelfBot { actor } => {
                let x = center(*actor);
                grid.put_str(x, y, "│◀┘", Ink::Arrow);
            }
            SeqRow::Text(text) => grid.put_str(0, y, text, Ink::Text),
        }
    }
    Some(grid)
}

enum SeqRow {
    Header,
    Life,
    Message {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
    },
    SelfTop {
        actor: usize,
    },
    SelfMid {
        actor: usize,
        text: String,
    },
    SelfBot {
        actor: usize,
    },
    Text(String),
}

fn draw_message(
    grid: &mut Grid,
    y: usize,
    from: usize,
    to: usize,
    text: &str,
    dashed: bool,
    width: usize,
) {
    let (left, right, forward) = if from <= to {
        (from, to, true)
    } else {
        (to, from, false)
    };
    let line = if dashed { "┄" } else { "─" };
    if left == right {
        grid.put_str(left, y, "│", Ink::Border);
        return;
    }
    grid.put_str(left, y, "│", Ink::Border);
    let mut x = left + 1;
    while x < right {
        grid.put_str(x, y, line, Ink::Border);
        x += str_width(line);
    }
    let arrow = if forward { "▶" } else { "◀" };
    if forward {
        grid.put_str(right, y, arrow, Ink::Arrow);
    } else {
        grid.put_str(left, y, arrow, Ink::Arrow);
        grid.put_str(right, y, "│", Ink::Border);
    }
    if !text.is_empty() {
        let label_x = (right + 2).min(width.saturating_sub(1));
        grid.put_str(label_x, y, text, Ink::Text);
    }
}

fn strip_comment(line: &str) -> &str {
    let mut depth: i32 = 0;
    let mut quote = false;
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    for (offset, (index, ch)) in chars.iter().enumerate() {
        if quote {
            if *ch == '"' {
                quote = false;
            }
            continue;
        }
        match ch {
            '"' => quote = true,
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            '%' if depth == 0 && chars.get(offset + 1).is_some_and(|(_, next)| *next == '%') => {
                return line[..*index].trim_end();
            }
            _ => {}
        }
    }
    line.trim()
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

fn equals_ignore_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        let len = w.saturating_mul(h);
        Self {
            w,
            h,
            cells: (0..len)
                .map(|_| Cell {
                    ch: ' ',
                    ink: Ink::None,
                    skip: false,
                })
                .collect(),
            mask: vec![0; len],
        }
    }

    fn put_str(&mut self, x: usize, y: usize, text: &str, ink: Ink) {
        if y >= self.h {
            return;
        }
        let mut col = x;
        for ch in text.chars() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width == 0 || col + width > self.w {
                break;
            }
            self.cells[y * self.w + col] = Cell {
                ch,
                ink,
                skip: false,
            };
            for step in 1..width {
                self.cells[y * self.w + col + step] = Cell {
                    ch: ' ',
                    ink,
                    skip: true,
                };
            }
            col += width;
        }
    }

    fn set_ink(&mut self, x: usize, y: usize, ink: Ink) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x].ink = ink;
        }
    }

    fn vline(&mut self, x: usize, y0: usize, y1: usize) {
        let (start, end) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        for y in start..=end {
            let mut bit = 0;
            if y > start {
                bit |= N;
            }
            if y < end {
                bit |= S;
            }
            if start == end {
                bit |= N | S;
            }
            self.mask_at(x, y, bit);
        }
    }

    fn hline(&mut self, x0: usize, x1: usize, y: usize) {
        let (start, end) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        for x in start..=end {
            let mut bit = 0;
            if x > start {
                bit |= W;
            }
            if x < end {
                bit |= E;
            }
            if start == end {
                bit |= E | W;
            }
            self.mask_at(x, y, bit);
        }
    }

    fn arrow(&mut self, x: usize, y: usize, bit: u8) {
        self.mask_at(x, y, bit);
    }

    fn mask_at(&mut self, x: usize, y: usize, bit: u8) {
        if x < self.w && y < self.h {
            self.mask[y * self.w + x] |= bit;
        }
    }

    fn into_lines(self, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for y in 0..self.h {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut ink = Ink::None;
            let flush = |spans: &mut Vec<Span<'static>>, buf: &mut String, ink: Ink| {
                if buf.is_empty() {
                    return;
                }
                spans.push(Span::styled(std::mem::take(buf), ink_style(ink, theme)));
            };
            for x in 0..self.w {
                if self.cells[y * self.w + x].skip {
                    continue;
                }
                let (ch, cell_ink) = self.glyph(x, y);
                if cell_ink != ink && !buf.is_empty() {
                    flush(&mut spans, &mut buf, ink);
                }
                ink = cell_ink;
                buf.push(ch);
            }
            while buf.ends_with(' ') {
                buf.pop();
            }
            flush(&mut spans, &mut buf, ink);
            if spans.is_empty() {
                lines.push(Line::from(""));
            } else {
                lines.push(Line::from(spans));
            }
        }
        while lines.last().is_some_and(|line| {
            line.spans
                .iter()
                .all(|span| span.content.chars().all(|ch| ch == ' '))
        }) {
            lines.pop();
        }
        lines
    }

    fn glyph(&self, x: usize, y: usize) -> (char, Ink) {
        let cell = &self.cells[y * self.w + x];
        if cell.ch != ' ' {
            return (cell.ch, cell.ink);
        }
        let mask = self.mask[y * self.w + x];
        if mask & ARROW_S != 0 {
            return ('▼', Ink::Arrow);
        }
        if mask & ARROW_N != 0 {
            return ('▲', Ink::Arrow);
        }
        if mask & ARROW_E != 0 {
            return ('▶', Ink::Arrow);
        }
        if mask & ARROW_W != 0 {
            return ('◀', Ink::Arrow);
        }
        let ch = match mask & 0b1111 {
            0 => ' ',
            bits if bits == N || bits == S || bits == (N | S) => '│',
            bits if bits == E || bits == W || bits == (E | W) => '─',
            bits if bits == (S | E) => '┌',
            bits if bits == (S | W) => '┐',
            bits if bits == (N | E) => '└',
            bits if bits == (N | W) => '┘',
            bits if bits == (N | S | E) => '├',
            bits if bits == (N | S | W) => '┤',
            bits if bits == (E | W | S) => '┬',
            bits if bits == (E | W | N) => '┴',
            bits if bits == (N | E | W | S) => '┼',
            _ => '·',
        };
        (ch, if ch == ' ' { Ink::None } else { Ink::Border })
    }
}

fn ink_style(ink: Ink, theme: &Theme) -> Style {
    match ink {
        Ink::Border => Style::new().fg(theme.border),
        Ink::Text => Style::new().fg(theme.text_primary),
        Ink::Arrow => Style::new().fg(theme.gray),
        Ink::None => Style::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorLevel;

    fn draw(source: &str, width: usize) -> Option<String> {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        render(source, width, &theme).map(|lines| {
            lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    #[test]
    fn flowchart_draws_nodes_and_arrow() {
        let picture = draw("flowchart TD\n  A[Start] --> B[End]\n", 40).expect("流程图");
        assert!(picture.contains("Start"), "{picture}");
        assert!(picture.contains("End"), "{picture}");
        assert!(picture.contains('▼'), "{picture}");
        assert!(!picture.contains("flowchart"), "{picture}");
    }

    #[test]
    fn flowchart_keeps_edge_label() {
        let picture = draw("flowchart TD\n  A -->|yes| B\n", 40).expect("带标签的边");
        assert!(picture.contains("yes"), "{picture}");
    }

    #[test]
    fn sequence_draws_message() {
        let source = "sequenceDiagram\n  participant Alice\n  participant Bob\n  Alice->>Bob: Hello\n  loop Wait\n  end\n";
        let picture = draw(source, 60).expect("时序图");
        assert!(picture.contains("Alice"), "{picture}");
        assert!(picture.contains("Bob"), "{picture}");
        assert!(picture.contains("Hello"), "{picture}");
        assert!(picture.contains('▶'), "{picture}");
        assert!(
            picture.contains("Wait") && picture.contains('└'),
            "{picture}"
        );
    }

    #[test]
    fn state_draws_transition() {
        let source = "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Run : go\n";
        let picture = draw(source, 40).expect("状态图");
        assert!(picture.contains("Idle"), "{picture}");
        assert!(picture.contains("Run"), "{picture}");
        assert!(picture.contains("go"), "{picture}");
        assert!(picture.contains('●'), "{picture}");
    }

    #[test]
    fn class_diagram_is_left_as_code() {
        assert!(draw("classDiagram\n  Animal <|-- Duck\n", 40).is_none());
    }

    #[test]
    fn narrow_width_still_shows_every_label() {
        let source = "flowchart LR\n  A[Alpha] --> B[Beta] --> C[Gamma] --> D[Delta]\n";
        let picture = draw(source, 16).expect("窄宽度要改成纵向");
        for label in ["Alpha", "Beta", "Gamma", "Delta"] {
            assert!(picture.contains(label), "{label} missing in {picture}");
        }
    }
}
