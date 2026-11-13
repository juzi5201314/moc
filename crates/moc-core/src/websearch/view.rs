//! 有界网页视图：保留章节与代码块边界，所有行号指向不可变正文快照。

use std::fmt::Write;

use super::{ExtractResponse, ExtractResult};

const DISPLAY_BYTES: usize = 16 * 1024;

pub(crate) async fn mark_seen(
    root: &std::path::Path,
    response: &mut ExtractResponse,
    query: Option<&str>,
) -> std::io::Result<()> {
    let Some(query) = query else {
        return Ok(());
    };
    let query = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let directory = root.join("web-cache").join("views");
    tokio::fs::create_dir_all(&directory).await?;
    for page in &mut response.results {
        let key = format!("{}\0{query}", page.artifact.as_deref().expect("网页已归档"));
        let path = directory.join(format!(
            "{:032x}",
            xxhash_rust::xxh3::xxh3_128(key.as_bytes())
        ));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                page.view_repeated = true
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct Block {
    start: usize,
    end: usize,
    heading: Option<usize>,
    definition: bool,
    code: bool,
}

struct Document<'a> {
    lines: Vec<&'a str>,
    blocks: Vec<Block>,
    headings: Vec<usize>,
}

impl<'a> Document<'a> {
    fn new(text: &'a str) -> Self {
        let lines: Vec<_> = text.lines().collect();
        let mut blocks = Vec::new();
        let mut headings = Vec::new();
        let mut heading = None;
        let mut index = 0;
        while index < lines.len() {
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            let start = index;
            let line = lines[index].trim_start();
            let fence = fence_marker(line);
            let code = fence.is_some();
            if heading_level(line).is_some() {
                heading = Some(index);
                headings.push(index);
                index += 1;
            } else if let Some((marker, count)) = fence {
                index += 1;
                while index < lines.len() {
                    let closing = lines[index].trim();
                    index += 1;
                    if closing.bytes().take_while(|byte| *byte == marker).count() >= count
                        && closing.bytes().all(|byte| byte == marker)
                    {
                        break;
                    }
                }
            } else if field_definition(lines[index]) {
                let indent = lines[index].len() - lines[index].trim_start().len();
                let mut fence = None;
                index += 1;
                while index < lines.len() {
                    let line = lines[index].trim_start();
                    let depth = lines[index].len() - line.len();
                    if let Some((marker, count)) = fence {
                        if line.trim_end().bytes().all(|byte| byte == marker)
                            && line.trim_end().len() >= count
                        {
                            fence = None;
                        }
                    } else {
                        if heading_level(line).is_some()
                            || (depth <= indent && field_definition(line))
                        {
                            break;
                        }
                        fence = fence_marker(line);
                    }
                    index += 1;
                }
            } else {
                index += 1;
                while index < lines.len()
                    && !lines[index].trim().is_empty()
                    && heading_level(lines[index].trim_start()).is_none()
                    && fence_marker(lines[index].trim_start()).is_none()
                    && !field_definition(lines[index])
                {
                    index += 1;
                }
            }
            blocks.push(Block {
                start,
                end: index,
                heading,
                definition: field_definition(lines[start]),
                code,
            });
        }
        Self {
            lines,
            blocks,
            headings,
        }
    }

    fn view(&self, query: Option<&str>, budget: usize) -> (String, bool) {
        let mut output = String::new();
        let mut omitted = false;
        let terms: Vec<_> = query
            .into_iter()
            .flat_map(str::split_whitespace)
            .map(str::to_lowercase)
            .collect();
        if terms.is_empty() {
            output.push_str("Outline (source line numbers):\n");
            for &line in &self.headings {
                let row = format!("{}: {}\n", line + 1, self.lines[line]);
                if output.len() + row.len() > budget / 3 {
                    omitted = true;
                    break;
                }
                output.push_str(&row);
            }
        }
        let mut ranked: Vec<(usize, usize, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                if terms.is_empty() {
                    return Some((index, 0, 0));
                }
                let heading = block
                    .heading
                    .map_or(String::new(), |line| self.lines[line].to_lowercase());
                let content = self.lines[block.start..block.end].join("\n").to_lowercase();
                let hits = terms
                    .iter()
                    .filter(|term| content.contains(term.as_str()))
                    .count();
                let heading_hits = terms
                    .iter()
                    .filter(|term| heading.contains(term.as_str()))
                    .count();
                if hits == 0 && heading_hits == 0 {
                    return None;
                }
                // 定义和标题优先；示例代码的密集字段不能压过真实参数定义。
                let kind = if block.code {
                    0
                } else if block.definition || heading_level(self.lines[block.start]).is_some() {
                    2
                } else {
                    1
                };
                Some((index, kind, hits + heading_hits * 2))
            })
            .collect();
        if !terms.is_empty() {
            ranked.sort_by_key(|&(index, kind, hits)| {
                (std::cmp::Reverse(kind), std::cmp::Reverse(hits), index)
            });
            if ranked.is_empty() {
                return ("No matching sections. Search the saved document with grep or read its outline.\n".into(), true);
            }
        }
        let mut selected = Vec::new();
        let mut oversized = Vec::new();
        let mut remaining = budget.saturating_sub(output.len());
        for (index, _, _) in ranked {
            let block = &self.blocks[index];
            let heading = block.heading.filter(|line| *line != block.start);
            let heading_bytes = heading.map_or(0, |line| self.lines[line].len() + 32);
            let size = self.lines[block.start..block.end]
                .iter()
                .map(|line| line.len() + 24)
                .sum::<usize>()
                + heading_bytes
                + 64;
            if size > remaining {
                omitted = true;
                if oversized.len() < 3 {
                    oversized.push((block.start + 1, block.end));
                }
                continue;
            }
            remaining -= size;
            selected.push(index);
        }
        selected.sort_unstable();
        let selected_count = selected.len();
        for index in selected {
            let block = &self.blocks[index];
            if let Some(line) = block.heading.filter(|line| *line != block.start) {
                let _ = writeln!(output, "Section at line {}: {}", line + 1, self.lines[line]);
            }
            let _ = writeln!(output, "Lines {}-{}:", block.start + 1, block.end);
            for index in block.start..block.end {
                let _ = writeln!(output, "{}│{}", index + 1, self.lines[index]);
            }
            output.push('\n');
        }
        for (start, end) in oversized {
            let notice = format!(
                "Read lines {start}-{end} for an omitted block (too large for this preview).\n"
            );
            if output.len() + notice.len() <= budget {
                output.push_str(&notice);
            }
        }
        debug_assert!(output.len() <= budget);
        (output, omitted || selected_count < self.blocks.len())
    }
}

fn heading_level(line: &str) -> Option<usize> {
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    (level > 0 && level <= 6 && line.as_bytes().get(level) == Some(&b' ')).then_some(level)
}

fn field_definition(line: &str) -> bool {
    let line = line.trim_start();
    ["- `", "* `", "- **", "* **"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

fn fence_marker(line: &str) -> Option<(u8, usize)> {
    let marker = *line.as_bytes().first()?;
    let count = line.bytes().take_while(|byte| *byte == marker).count();
    (matches!(marker, b'`' | b'~') && count >= 3).then_some((marker, count))
}

fn short(text: &str, bytes: usize) -> String {
    let mut output = String::new();
    for character in text.chars().filter(|character| !character.is_control()) {
        if output.len() + character.len_utf8() > bytes {
            break;
        }
        output.push(character);
    }
    output
}

pub(crate) fn render(response: &ExtractResponse, query: Option<&str>, max_chars: usize) -> String {
    let pages: Vec<_> = response.results.iter().map(|page| serde_json::json!({
        "url": short(&page.url, 64), "title": page.title.as_deref().map(|title| short(title, 64)),
        "tokens": crate::compact::estimate_tokens(&page.raw_content), "truncated": page.truncated,
        "artifact": page.artifact, "cached": page.cached,
    })).chain(response.failed.iter().map(|page| serde_json::json!({
        "url": short(&page.url, 64), "error": short(&page.error, 64),
    }))).collect();
    let mut output = format!(
        "Web fetch summary: {}\n\n",
        serde_json::to_string(&pages).expect("网页元数据可序列化")
    );
    output.push_str("Saved documents contain the fetched source, not just this preview. Use read with offset/limit or grep on the artifact; do not refetch for more lines. Source truncated means missing data, preview omitted does not.\n\n");
    for (index, page) in response.results.iter().enumerate() {
        let remaining_pages = response.results.len() - index;
        let budget = DISPLAY_BYTES.saturating_sub(output.len()) / remaining_pages;
        let section = page_view(page, query, budget, max_chars);
        output.push_str(&section);
    }
    debug_assert!(output.len() <= DISPLAY_BYTES);
    output
}

fn page_view(page: &ExtractResult, query: Option<&str>, budget: usize, max_chars: usize) -> String {
    let uri = page.artifact.as_deref().expect("抓取结果已有正文快照");
    let mut text = format!(
        "## {}\nDocument: {uri}\nSource: {} · {} lines · {}\n",
        short(page.title.as_deref().unwrap_or(&page.url), 120),
        if page.truncated {
            "INCOMPLETE (response/batch limit); missing source cannot be recovered from this artifact"
        } else {
            "complete"
        },
        page.raw_content.lines().count(),
        if page.cached { "cached" } else { "fetched" }
    );
    if text.len() + 100 > budget {
        return String::new();
    }
    if page.view_repeated || (page.cached && query.is_none()) {
        text.push_str("Already fetched. Read or grep this document instead of requesting the same URL again.\n\n");
        return text;
    }
    let room = budget.saturating_sub(text.len() + 100).min(max_chars);
    if room < 160 {
        return text;
    }
    let (view, omitted) = Document::new(&page.raw_content).view(query, room);
    text.push_str(&view);
    if omitted {
        text.push_str(
            "Preview omitted other content; the document remains available at the URI above.\n",
        );
    }
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_beat_response_examples_and_ranges_keep_fences() {
        let source = "# API\n\n## Request parameters\n\n- `temperature: number`\n\n  Sampling temperature.\n\n## Response example\n\n```json\n{\"temperature\": 1, \"metadata\": {}, \"reasoning\": {}}\n```\n";
        let document = Document::new(source);
        let (view, omitted) = document.view(Some("temperature metadata reasoning"), 350);
        assert!(view.contains("- `temperature: number`"));
        assert!(!view.contains("```json"));
        assert!(omitted);
        let (view, _) = document.view(Some("temperature metadata reasoning"), 2000);
        assert!(view.contains(
            "11│```json\n12│{\"temperature\": 1, \"metadata\": {}, \"reasoning\": {}}\n13│```"
        ));
        assert!(
            document
                .view(Some("nonexistent"), 1000)
                .0
                .starts_with("No matching")
        );
    }

    #[tokio::test]
    async fn batch_budget_preserves_all_document_references_and_suppresses_repeated_views() {
        let page = ExtractResult {
            url: format!("https://example.test/{}", "中\\\"".repeat(100)),
            final_url: "https://example.test/".into(),
            title: Some("标题\n\\\"".repeat(100)),
            status: Some(200),
            content_type: Some("text/markdown".into()),
            method: "generic".into(),
            notes: Vec::new(),
            truncated: true,
            artifact: None,
            cached: false,
            view_repeated: false,
            raw_content: "# API\n\n- `temperature: number`\n\n  Controls sampling.\n\n".repeat(100),
        };
        let mut response = ExtractResponse {
            results: (0..20)
                .map(|index| ExtractResult {
                    artifact: Some(format!("artifact://artifacts/web-{index}.md")),
                    ..page.clone()
                })
                .collect(),
            failed: Vec::new(),
        };
        let first = render(&response, Some("temperature"), 500_000);
        assert!(first.len() <= DISPLAY_BYTES);
        for page in &response.results {
            assert!(first.contains(page.artifact.as_deref().unwrap()));
        }
        let root = tempfile::tempdir().unwrap();
        mark_seen(root.path(), &mut response, Some("temperature"))
            .await
            .unwrap();
        assert!(response.results.iter().all(|page| !page.view_repeated));
        mark_seen(root.path(), &mut response, Some("temperature"))
            .await
            .unwrap();
        assert!(response.results.iter().all(|page| page.view_repeated));
        let repeated = render(&response, Some("temperature"), 500_000);
        assert!(!repeated.contains("Controls sampling"));
        for page in &response.results {
            assert!(repeated.contains(page.artifact.as_deref().unwrap()));
        }
    }
}
