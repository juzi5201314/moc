//! 项目与全局 AGENTS.md 加载链，以及有界的 @import 展开。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_CONTEXT_BYTES: usize = 200 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// 从 git root（或 cwd）到当前目录读取 AGENTS.md，并最后拼接全局规则。
pub fn load_context_files(
    cwd: &Path,
    moc_home: &Path,
    max_import_depth: usize,
) -> Vec<ContextFile> {
    let root = git_root(cwd).unwrap_or_else(|| cwd.to_owned());
    let mut directories = Vec::new();
    let mut current = cwd.to_owned();
    while current.starts_with(&root) {
        directories.push(current.clone());
        if current == root || !current.pop() {
            break;
        }
    }
    directories.reverse();
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    for directory in directories {
        push_file(
            directory.join("AGENTS.md"),
            max_import_depth,
            &mut visited,
            &mut files,
        );
    }
    push_file(
        moc_home.join("AGENTS.md"),
        max_import_depth,
        &mut visited,
        &mut files,
    );
    deduplicate_and_bound(files)
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        let path = String::from_utf8(output.stdout).ok()?;
        return Some(PathBuf::from(path.trim()));
    }
    let mut current = cwd;
    while let Some(parent) = current.parent() {
        if current.join(".git").exists() {
            return Some(current.to_owned());
        }
        current = parent;
    }
    Some(current.to_owned())
}

fn push_file(
    path: PathBuf,
    max_depth: usize,
    visited: &mut HashSet<PathBuf>,
    output: &mut Vec<ContextFile>,
) {
    let Ok(path) = fs::canonicalize(&path) else {
        return;
    };
    if !visited.insert(path.clone()) {
        return;
    }
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    if content.trim().is_empty() {
        return;
    }
    let expanded = expand_imports(&path, &content, 0, max_depth, visited, output);
    output.push(ContextFile {
        path,
        content: expanded,
    });
}

fn expand_imports(
    source_path: &Path,
    content: &str,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<PathBuf>,
    output: &mut Vec<ContextFile>,
) -> String {
    let mut result = String::with_capacity(content.len());
    let mut fenced = false;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            result.push_str(line);
            continue;
        }
        if fenced {
            result.push_str(line);
            continue;
        }
        let Some(import) = parse_import_line(line) else {
            result.push_str(line);
            continue;
        };
        if depth >= max_depth {
            result.push_str(line);
            continue;
        }
        let Some(path) = resolve_import(source_path, import) else {
            result.push_str(line);
            continue;
        };
        let Ok(canonical) = fs::canonicalize(&path) else {
            result.push_str(line);
            continue;
        };
        if !visited.insert(canonical.clone()) {
            continue;
        }
        let Ok(imported) = fs::read_to_string(&canonical) else {
            result.push_str(line);
            continue;
        };
        let expanded = expand_imports(&canonical, &imported, depth + 1, max_depth, visited, output);
        if !expanded.trim().is_empty() {
            result.push_str(&format!(
                "\n<file path=\"{}\">\n{}\n</file>\n",
                canonical.display(),
                expanded
            ));
        }
    }
    let _ = output;
    result
}

fn parse_import_line(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let import = trimmed.strip_prefix('@')?;
    if import.is_empty() || import.contains('`') || import.chars().any(char::is_whitespace) {
        return None;
    }
    Some(import)
}

fn resolve_import(source: &Path, import: &str) -> Option<PathBuf> {
    if let Some(rest) = import
        .strip_prefix("~/")
        .or_else(|| import.strip_prefix("~\\"))
    {
        return crate::paths::home_dir().map(|home| home.join(rest));
    }
    let path = Path::new(import);
    Some(if path.is_absolute() {
        path.to_owned()
    } else {
        source.parent()?.join(path)
    })
}

fn deduplicate_and_bound(files: Vec<ContextFile>) -> Vec<ContextFile> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut bytes = 0usize;
    let mut omitted = 0usize;
    for mut file in files {
        if !seen.insert(file.content.as_bytes().to_vec()) {
            continue;
        }
        if bytes + file.content.len() > MAX_CONTEXT_BYTES {
            let available = MAX_CONTEXT_BYTES.saturating_sub(bytes);
            if available > 0 {
                file.content =
                    String::from_utf8_lossy(&file.content.as_bytes()[..available]).into_owned();
                bytes += file.content.len();
                result.push(file);
            }
            omitted += 1;
            continue;
        }
        bytes += file.content.len();
        result.push(file);
    }
    if omitted > 0
        && let Some(last) = result.last_mut()
    {
        last.content.push_str(&format!(
            "\n[已省略 {omitted} 个上下文文件，共超过 {MAX_CONTEXT_BYTES} 字节]"
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn loads_ancestor_chain_imports_once_and_ignores_code_examples() {
        let root = tempfile::tempdir().expect("临时项目");
        let home = tempfile::tempdir().expect("临时 home");
        fs::create_dir_all(root.path().join("sub")).expect("子目录");
        fs::write(
            root.path().join("AGENTS.md"),
            "root rules\n@shared.md\n```md\n@ignored.md\n```\n",
        )
        .expect("根规则");
        fs::write(root.path().join("shared.md"), "shared rules\n@details.md\n").expect("shared");
        fs::write(root.path().join("details.md"), "nested detail\n").expect("details");
        fs::write(
            root.path().join("sub/AGENTS.md"),
            "local rules\n@../shared.md\n",
        )
        .expect("本地规则");
        fs::write(root.path().join("ignored.md"), "not imported\n").expect("示例文件");
        fs::write(home.path().join("AGENTS.md"), "global rules\n").expect("全局规则");
        let files = load_context_files(&root.path().join("sub"), home.path(), 5);
        let prompt = files
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("root rules"));
        assert!(prompt.contains("shared rules"));
        assert!(prompt.contains("nested detail"));
        assert!(prompt.contains("local rules"));
        assert!(prompt.contains("global rules"));
        assert!(!prompt.contains("not imported"));
        assert_eq!(prompt.matches("shared rules").count(), 1);
    }

    #[test]
    fn fenced_and_inline_import_markers_are_not_expanded() {
        let root = tempfile::tempdir().expect("临时目录");
        fs::write(
            root.path().join("AGENTS.md"),
            "`@missing.md`\n```\n@missing.md\n```\n@absent.md\n",
        )
        .expect("写规则");
        let files = load_context_files(root.path(), root.path().join("none").as_path(), 5);
        assert!(files[0].content.contains("@missing.md"));
        assert!(files[0].content.contains("@absent.md"));
    }
}
