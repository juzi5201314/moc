//! 工具输出：模型可见文本的大小预算、完整输出归档，以及已有文件的限长预览。

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::config::OutputConfig;
use crate::resources::{ArtifactStore, ResourceError};

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("output file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact archival failed: {0}")]
    Resource(#[from] ResourceError),
}

/// 工具输出的来源。工具只说明输出在哪，归档和预览由统一出口处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutput {
    Text(String),
    /// 已写入文件的流式输出；`suffix` 是附在后面、不属于原始日志的说明，例如退出状态。
    File {
        path: PathBuf,
        suffix: String,
    },
    /// 工具自己挑出的片段；超出预算时引用原文件，不再复制一份日志。
    Excerpt {
        text: String,
        path: PathBuf,
    },
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

/// 只在交给模型展示时格式化结构化数据；工具执行和 JS 读到的仍是原始值。
pub(crate) async fn prepare_outcome(
    outcome: &crate::tools::ToolOutcome,
    config: &OutputConfig,
    artifacts: &ArtifactStore,
) -> Result<String, OutputError> {
    let Some(structured) = &outcome.structured_content else {
        return prepare(outcome.content.clone(), config, artifacts).await;
    };
    let json = serde_json::to_string_pretty(structured).expect("JSON Value 可序列化");
    let mut text = match &outcome.content {
        ToolOutput::Text(text) => text.clone(),
        ToolOutput::File { path, suffix } => {
            let uri = artifacts.uri_for_path(path)?;
            let mut text = preview_file(path, config, &uri).await?;
            text.push_str(suffix);
            text
        }
        ToolOutput::Excerpt { text, .. } => text.clone(),
    };
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str("[structured result]\n```json\n");
    text.push_str(&json);
    text.push_str("\n```");
    prepare(ToolOutput::Text(text), config, artifacts).await
}

/// 在持久化和发给模型之前调用；只处理文本，不改动错误、图片、diff 和提示控制。
pub(crate) async fn prepare(
    output: ToolOutput,
    config: &OutputConfig,
    artifacts: &ArtifactStore,
) -> Result<String, OutputError> {
    match output {
        ToolOutput::Text(text) => {
            if text.len() <= budget(config) {
                return Ok(text);
            }
            let uri = artifacts.archive_text(&text).await?;
            Ok(preview(&text, config, &uri))
        }
        ToolOutput::File { path, suffix } => {
            let uri = artifacts.uri_for_path(&path)?;
            let mut text = preview_file(&path, config, &uri).await?;
            if text.is_empty() {
                text.push_str("(no output)");
            }
            text.push_str(&suffix);
            Ok(text)
        }
        ToolOutput::Excerpt { text, path } => {
            if text.len() <= budget(config) {
                Ok(text)
            } else {
                let uri = artifacts.uri_for_path(&path)?;
                Ok(preview(&text, config, &uri))
            }
        }
    }
}

/// 分页工具共用的同一份预算。分页按整行截断，避免截掉的行仍然拿到可编辑的锚点。
pub(crate) fn budget(config: &OutputConfig) -> usize {
    config.head_bytes.saturating_add(config.tail_bytes)
}

fn preview(text: &str, config: &OutputConfig, uri: &str) -> String {
    let mut head_end = config.head_bytes.min(text.len());
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(config.tail_bytes);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    render_preview(
        &text[..head_end],
        &text[tail_start..],
        tail_start - head_end,
        uri,
    )
}

async fn preview_file(path: &Path, config: &OutputConfig, uri: &str) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    if length <= budget(config) as u64 {
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(length).read_to_end(&mut bytes).await?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let mut head = vec![0; config.head_bytes];
    file.read_exact(&mut head).await?;
    // 截断点落在 UTF-8 字符中间时，丢弃跨边界的残缺部分；真正无效的字节仍显示为替换字符。
    if let Err(error) = std::str::from_utf8(&head)
        && error.error_len().is_none()
    {
        head.truncate(error.valid_up_to());
    }
    let tail_start = length - config.tail_bytes as u64;
    file.seek(std::io::SeekFrom::Start(tail_start)).await?;
    let mut tail = vec![0; config.tail_bytes];
    file.read_exact(&mut tail).await?;
    let skipped = tail.iter().take_while(|byte| **byte & 0xc0 == 0x80).count();
    let tail = &tail[skipped..];
    Ok(render_preview(
        &String::from_utf8_lossy(&head),
        &String::from_utf8_lossy(tail),
        length - head.len() as u64 - tail.len() as u64,
        uri,
    ))
}

fn render_preview(head: &str, tail: &str, omitted: impl std::fmt::Display, uri: &str) -> String {
    format!("{head}\n[… {omitted} bytes omitted; full output: {uri}]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::SessionPaths;

    fn paths(root: &Path) -> SessionPaths {
        SessionPaths {
            transcript: root.join("session.jsonl"),
            sidecar: root.join("session"),
        }
    }

    #[tokio::test]
    async fn threshold_archives_losslessly_and_keeps_unicode_edges() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        let config = OutputConfig {
            head_bytes: 7,
            tail_bytes: 7,
            ..Default::default()
        };
        let at_limit = "abcdefghijklmn";
        assert_eq!(
            prepare(at_limit.into(), &config, &ArtifactStore::new(paths.clone()))
                .await
                .expect("生成输出"),
            at_limit
        );
        assert!(!paths.artifacts().exists(), "没超预算时不应归档");
        let text = "前中后尾外后前中";
        let result = prepare(text.into(), &config, &ArtifactStore::new(paths.clone()))
            .await
            .expect("归档");
        assert!(result.starts_with("前中\n"));
        assert!(result.ends_with("前中"));
        assert!(!result.contains('\u{fffd}'));
        let mut artifacts = tokio::fs::read_dir(paths.artifacts())
            .await
            .expect("归档目录");
        let path = artifacts
            .next_entry()
            .await
            .expect("枚举")
            .expect("归档文件")
            .path();
        assert_eq!(
            tokio::fs::read_to_string(path).await.expect("完整输出"),
            text
        );
    }

    #[tokio::test]
    async fn existing_file_and_excerpt_reuse_original_without_copying() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        tokio::fs::create_dir_all(paths.jobs())
            .await
            .expect("job 目录");
        let path = paths.jobs().join("job.log");
        let text = "前中后尾外后前中";
        tokio::fs::write(&path, text).await.expect("日志");
        let config = OutputConfig {
            head_bytes: 7,
            tail_bytes: 7,
            ..Default::default()
        };
        let result = prepare(
            ToolOutput::File {
                path: path.clone(),
                suffix: "\n(exit 1)".into(),
            },
            &config,
            &ArtifactStore::new(paths.clone()),
        )
        .await
        .expect("日志交付");
        assert!(result.starts_with("前中\n"));
        assert!(result.ends_with("前中\n(exit 1)"));
        assert!(!result.contains('\u{fffd}'));
        assert!(result.contains("artifact://jobs/job.log"));
        let excerpt = prepare(
            ToolOutput::Excerpt {
                text: text.into(),
                path: path.clone(),
            },
            &config,
            &ArtifactStore::new(paths.clone()),
        )
        .await
        .expect("片段交付");
        assert!(excerpt.contains("artifact://jobs/job.log"));
        assert!(!paths.artifacts().exists(), "已有输出不再复制归档");
        assert_eq!(tokio::fs::read_to_string(path).await.expect("原日志"), text);
    }
}
