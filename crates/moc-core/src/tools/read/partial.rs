//! 超长行按 UTF-8 字节边界续读；片段不登记为可编辑行。

use serde_json::json;

use crate::config::ReadConfig;
use crate::tools::{ToolError, ToolOutcome, output::ToolOutput};

pub(super) fn read(
    path: &str,
    line: &str,
    line_no: usize,
    start: usize,
    chunk_bytes: usize,
    config: &ReadConfig,
) -> Result<ToolOutcome, ToolError> {
    if start > line.len() || !line.is_char_boundary(start) {
        return Err(ToolError::new(
            "E_ARGUMENT",
            "byte_offset must be a UTF-8 character boundary within the selected line",
        ));
    }
    let header = format!(
        "[{path}]\n[Read-only line {line_no}; byte_offset={start}; total_bytes={}]\n",
        line.len()
    );
    let mut next = json!({"path":path,"offset":line_no,"limit":1,"byte_offset":line.len()});
    let footer_budget = format!("\n[Continue with read {next}]\n").len();
    let budget = chunk_bytes.min(
        config
            .max_bytes
            .saturating_sub(header.len() + footer_budget),
    );
    let mut end = start.saturating_add(budget).min(line.len());
    while end > start && !line.is_char_boundary(end) {
        end -= 1;
    }
    if end == start && start < line.len() {
        return Err(ToolError::new(
            "E_OUTPUT_LIMIT",
            "read byte budget cannot fit one UTF-8 character and its continuation; increase tools.read.max_bytes or tools.output head_bytes/tail_bytes",
        ));
    }
    let text = &line[start..end];
    let mut content = header;
    content.push_str(text);
    let next = if end < line.len() {
        next["byte_offset"] = end.into();
        content.push_str(&format!("\n[Continue with read {next}]"));
        Some(next)
    } else {
        None
    };
    let mut outcome = ToolOutcome::success(ToolOutput::Verbatim(content));
    outcome.structured_content =
        Some(json!({"text":text,"line":line_no,"byte_offset":start,"next":next}));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_line_round_trips_with_exact_continuations() {
        let line = "ab中🙂尾".repeat(1000);
        let config = ReadConfig {
            max_bytes: 512,
            ..Default::default()
        };
        let mut offset = 0;
        let mut reconstructed = String::new();
        loop {
            let result = read("history://raw", &line, 9, offset, 31, &config).unwrap();
            let data = result.structured_content.unwrap();
            reconstructed.push_str(data["text"].as_str().unwrap());
            let Some(next) = data["next"]["byte_offset"].as_u64() else {
                break;
            };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert_eq!(reconstructed, line);
        assert!(read("history://raw", &line, 9, 3, 31, &config).is_err());

        // 共享输出预算下一块远大于 line_max_bytes(4KB)：20KB 的 ASCII 行一次读完。
        let line = "x".repeat(20 * 1024);
        let chunk = crate::tools::output::budget(&Default::default());
        let result = read("file.txt", &line, 1, 0, chunk, &ReadConfig::default()).unwrap();
        let data = result.structured_content.unwrap();
        assert_eq!(data["text"].as_str().unwrap(), line);
        assert!(data["next"].is_null());
    }
}
