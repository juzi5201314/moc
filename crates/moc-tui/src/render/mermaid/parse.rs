use super::Diagram;
use super::Dir;
use super::GEdge;
use super::GNode;
use super::Graph;
use super::Header;
use super::MAX_EDGES;
use super::MAX_NODES;
use super::SeqEvent;
use super::Sequence;
use super::Shape;
use std::collections::HashMap;

pub(super) fn parse(source: &str) -> Option<Diagram> {
    let mut lines = source
        .lines()
        .map(strip_comment)
        .filter(|line| !line.is_empty());
    let header = lines.next()?.trim().to_string();
    let kind = header_kind(&header)?;
    match kind {
        Header::Flow(dir) | Header::State(dir) => {
            let state = matches!(kind, Header::State(_));
            let mut graph = Graph {
                dir,
                nodes: Vec::new(),
                edges: Vec::new(),
            };
            let mut ids: HashMap<String, usize> = HashMap::new();
            // `end note` 收尾的多行 note 块（`note right of X` 不带冒号时开启）。
            let mut in_note = false;
            for line in lines {
                let line = line.trim();
                if in_note {
                    if line.to_ascii_lowercase().starts_with("end note") {
                        in_note = false;
                    }
                    continue;
                }
                if line.is_empty() || skippable(line) {
                    continue;
                }
                if let Some(dir) = line.strip_prefix("direction ").and_then(parse_dir) {
                    graph.dir = dir;
                    continue;
                }
                if state {
                    if let Some(rest) = strip_prefix_ignore_case(line, "note ") {
                        if !rest.contains(':') {
                            in_note = true;
                        }
                        continue;
                    }
                    if let Some(rest) = strip_prefix_ignore_case(line, "state ") {
                        state_line(&mut graph, &mut ids, rest.trim());
                        continue;
                    }
                    if let Some((id, desc)) = v1_description(line) {
                        upsert(&mut graph, &mut ids, &id, &desc, Shape::Box, true);
                        continue;
                    }
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

pub(super) fn header_kind(line: &str) -> Option<Header> {
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

pub(super) fn parse_dir(token: &str) -> Option<Dir> {
    match token.to_ascii_uppercase().as_str() {
        "TD" | "TB" => Some(Dir::Td),
        "BT" => Some(Dir::Bt),
        "LR" => Some(Dir::Lr),
        "RL" => Some(Dir::Rl),
        _ => None,
    }
}

pub(super) fn skippable(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower == "end"
        || lower.starts_with("end note")
        || lower.starts_with("classdef ")
        || lower.starts_with("class ")
        || lower.starts_with("style ")
        || lower.starts_with("click ")
        || lower.starts_with("linkstyle ")
        || lower.starts_with("subgraph")
        || lower.starts_with("autonumber")
        || lower.starts_with("activate ")
        || lower.starts_with("deactivate ")
        || lower.starts_with("acctitle")
        || lower.starts_with("accdescr")
        || (lower.starts_with("state ") && lower.ends_with('{'))
}

/// `state` 关键字的单行写法：`state "长名" as id`、`state id <<构造型>>`、
/// `state id : 描述`、裸 `state id`。复合块 `state X {` 已被 [`skippable`] 挡掉。
pub(super) fn state_line(graph: &mut Graph, ids: &mut HashMap<String, usize>, rest: &str) {
    if let Some((id, label)) = state_alias(rest) {
        upsert(graph, ids, &id, &label, Shape::Box, true);
        return;
    }
    if let Some((id, stereo)) = rest.split_once("<<") {
        let id = id.trim();
        if !id.is_empty() {
            let shape = if stereo.starts_with("choice") {
                Shape::Diamond
            } else {
                Shape::Dot
            };
            upsert(graph, ids, id, id, shape, true);
        }
        return;
    }
    if let Some((id, desc)) = rest.split_once(':') {
        let id = id.trim();
        if !id.is_empty() {
            upsert(graph, ids, id, desc.trim(), Shape::Box, true);
        }
        return;
    }
    // 只允许整段是单个 id 的裸写法，带箭头等残片的一律不碰。
    if let Some((id, tail)) = read_id(rest)
        && tail.trim().is_empty()
    {
        upsert(graph, ids, id, id, Shape::Box, true);
    }
}

/// `rest` 是 `state ` 之后的部分。只认带引号的 `state "标签" as id`。
pub(super) fn state_alias(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim_start();
    let stripped = rest.strip_prefix('"')?;
    let end = stripped.find('"')?;
    let label = &stripped[..end];
    let id = stripped[end + 1..].trim_start().strip_prefix("as ")?.trim();
    if id.is_empty() {
        None
    } else {
        Some((id.to_string(), label.to_string()))
    }
}

/// stateDiagram v1 的 `id : 描述` 行（无箭头）：整行必须是 `id : 文本` 的形状。
pub(super) fn v1_description(line: &str) -> Option<(String, String)> {
    let (id, desc) = line.split_once(':')?;
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }
    let (id, tail) = read_id(id.trim_start())?;
    if tail.trim().is_empty() {
        Some((id.to_string(), desc.to_string()))
    } else {
        None
    }
}

pub(super) fn feed_statement(graph: &mut Graph, ids: &mut HashMap<String, usize>, statement: &str) {
    let mut rest = statement.trim();
    if rest.is_empty() {
        return;
    }
    let Some(mut sources) = node_group(graph, ids, &mut rest) else {
        return;
    };
    while let Some((mut label, reversed, directed)) = arrow(&mut rest) {
        let Some(targets) = node_group(graph, ids, &mut rest) else {
            break;
        };
        if label.is_empty() {
            label = colon_label(&mut rest);
        }
        for &from in &sources {
            for &to in &targets {
                let (from, to) = if reversed { (to, from) } else { (from, to) };
                graph.edges.push(GEdge {
                    from,
                    to,
                    label: label.clone(),
                    directed,
                });
            }
        }
        sources = targets;
    }
}

pub(super) fn node_group(
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

pub(super) fn one_node(
    graph: &mut Graph,
    ids: &mut HashMap<String, usize>,
    rest: &mut &str,
) -> Option<usize> {
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

pub(super) fn read_id(text: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            end = index + ch.len_utf8();
            continue;
        }
        if ch == '-' {
            let rest = &text[index..];
            if rest.starts_with("--") || rest.starts_with("-.") || rest.starts_with("->") {
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

pub(super) fn read_shape(text: &str) -> Option<(Shape, String, &str)> {
    if let Some(text) = text.strip_prefix("[[") {
        // `[[label]]` 子程序形：直接找成对的 `]]`。
        let end = text.find("]]")?;
        return Some((Shape::Box, clean_label(&text[..end]), &text[end + 2..]));
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

pub(super) fn take_close(text: &str, open: char, close: char) -> Option<(&str, &str)> {
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

pub(super) fn clean_label(text: &str) -> String {
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

pub(super) fn upsert(
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

/// 返回值：`(标签, 反箭头, 有向)`。反箭头是 `<` 左箭头头（`A <-- B` 即 B→A）。
pub(super) fn arrow(rest: &mut &str) -> Option<(String, bool, bool)> {
    let trimmed = rest.trim_start();
    let (label, consumed, reversed, directed) = arrow_at(trimmed)?;
    *rest = &trimmed[consumed..];
    Some((label, reversed, directed))
}

/// 链接 = 可选左箭头头（`<`/`x`/`o`）+ 至少两个 `-`/`=`/`.` 主体 +
/// 可选右箭头头（`>`/`x`/`o`）+ 可选 `|label|`；`-- 标签 -->` 这类夹标签
/// 写法要求第二段主体也以箭头头结尾。
///
/// 返回值：`(标签, 消费字节数, 反箭头, 有向)`。
pub(super) fn arrow_at(text: &str) -> Option<(String, usize, bool, bool)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut reversed = false;
    let mut directed = false;
    match bytes.first() {
        Some(b'<') => {
            reversed = true;
            directed = true;
            i += 1;
        }
        Some(b'x' | b'o') => i += 1,
        _ => {}
    }
    let left_marker = i != 0;
    let body_start = i;
    while matches!(bytes.get(i), Some(b'-' | b'=' | b'.')) {
        i += 1;
    }
    if i - body_start < 2 {
        return None;
    }
    let mut right_head = false;
    if matches!(bytes.get(i), Some(b'>' | b'x' | b'o')) {
        directed = true;
        right_head = true;
        i += 1;
    }
    let rest = &text[i..];
    if rest.trim_start().starts_with('|') {
        let (label, consumed) = labeled_pipe(rest, i);
        return Some((label, consumed, reversed, directed));
    }
    // 夹标签只在裸主体后算：`--> B --> C` 是 A→B→C 链，`-- B --> C` 才是
    // 带标签的 A→C。
    if !left_marker
        && !right_head
        && let Some((label, consumed)) = label_then_link(rest, i)
    {
        return Some((label, consumed, reversed, true));
    }
    Some((String::new(), i, reversed, directed))
}

/// `|label|` 跟在箭头头之后。`consumed` 以 `base` 为起点累计。
pub(super) fn labeled_pipe(rest: &str, base: usize) -> (String, usize) {
    let trimmed = rest.trim_start();
    let spaces = rest.len() - trimmed.len();
    if let Some(inner) = trimmed.strip_prefix('|')
        && let Some(end) = inner.find('|')
    {
        return (inner[..end].to_string(), base + spaces + 1 + end + 1);
    }
    (String::new(), base)
}

/// `-- 标签 -->` / `-. 标签 .->`：第一段主体之后以空白开头时，找下一段
/// `[-=.]{2,}` 主体，只有它紧跟箭头头（`>`/`x`/`o`）才算夹标签链接；
/// 否则交给调用方按普通无标签链接处理。
pub(super) fn label_then_link(rest: &str, base: usize) -> Option<(String, usize)> {
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    for (off, ch) in rest.char_indices() {
        if !matches!(ch, '-' | '=' | '.') {
            continue;
        }
        let mut end = off + ch.len_utf8();
        while let Some(next) = rest[end..].chars().next() {
            if !matches!(next, '-' | '=' | '.') {
                break;
            }
            end += next.len_utf8();
        }
        if end - off < 2 {
            continue;
        }
        let Some(head) = rest[end..].chars().next() else {
            continue;
        };
        if !matches!(head, '>' | 'x' | 'o') {
            continue;
        }
        let label = rest[..off].trim();
        if label.is_empty() {
            return None;
        }
        return Some((label.to_string(), base + end + head.len_utf8()));
    }
    None
}

pub(super) fn colon_label(rest: &mut &str) -> String {
    let trimmed = rest.trim_start();
    let Some(label) = trimmed.strip_prefix(':') else {
        return String::new();
    };
    *rest = "";
    label.trim().to_string()
}

pub(super) fn split_semicolons(line: &str) -> Vec<&str> {
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

pub(super) fn parse_sequence<'a>(lines: impl Iterator<Item = &'a str>) -> Option<Sequence> {
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

pub(super) fn split_as(text: &str) -> (&str, &str) {
    if let Some((id, label)) = text.split_once(" as ") {
        (id.trim(), label.trim().trim_matches('"'))
    } else {
        let id = text.trim();
        (id, id)
    }
}

pub(super) fn is_group_open(line: &str) -> bool {
    const WORDS: &[&str] = &[
        "loop", "alt", "opt", "par", "critical", "break", "rect", "else",
    ];
    let word = line.split_whitespace().next().unwrap_or("");
    WORDS
        .iter()
        .any(|candidate| equals_ignore_case(word, candidate))
}

/// `(from, to, text, dashed, reverse)`。
pub(super) fn parse_message(line: &str) -> Option<(&str, &str, String, bool, bool)> {
    const TOKENS: &[(&str, bool, bool)] = &[
        ("<<-->>", true, false),
        ("<<->>", false, false),
        ("-->>", true, false),
        ("->>", false, false),
        ("--x", true, false),
        ("--)", true, false),
        ("-->", true, false),
        ("-x", false, false),
        ("-)", true, false),
        ("<<--", true, true),
        ("<<-", false, true),
        ("->", false, false),
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

pub(super) fn strip_comment(line: &str) -> &str {
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

pub(super) fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

pub(super) fn equals_ignore_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}
