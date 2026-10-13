//! 项目与全局 AGENTS.md 加载链，以及有界的 @import 展开。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// 从 git root（或 cwd）到当前目录读取 AGENTS.md，并最后拼接全局规则。
pub fn load_context_files(
    cwd: &Path,
    moc_home: &Path,
    config: &crate::config::ContextFilesConfig,
) -> Vec<ContextFile> {
    let root = git_root(cwd).unwrap_or_else(|| cwd.to_owned());
    let import_root = canonical_or_owned(&root);
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
    if config.project {
        for directory in directories {
            push_file(
                directory.join("AGENTS.md"),
                &import_root,
                config.import_depth,
                &mut visited,
                &mut files,
            );
        }
    }
    if config.user {
        push_file(
            moc_home.join("AGENTS.md"),
            &canonical_or_owned(moc_home),
            config.import_depth,
            &mut visited,
            &mut files,
        );
    }
    deduplicate_and_bound(files, config.max_tokens)
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

pub(crate) fn git_root(cwd: &Path) -> Option<PathBuf> {
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
    root: &Path,
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
    let expanded = expand_imports(&path, &content, root, 0, max_depth, visited, output);
    output.push(ContextFile {
        path,
        content: expanded,
    });
}

fn expand_imports(
    source_path: &Path,
    content: &str,
    root: &Path,
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
        let Some(canonical) = resolve_import(source_path, import, root) else {
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
        let expanded = expand_imports(
            &canonical,
            &imported,
            root,
            depth + 1,
            max_depth,
            visited,
            output,
        );
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

/// 把 `@` 导入解析为规范路径：绝对路径、`~/` 与相对路径都必须落在指令根之内，
/// 符号链接按解析后的真实路径判定，越界一律拒绝。
fn resolve_import(source: &Path, import: &str, root: &Path) -> Option<PathBuf> {
    let joined = if let Some(rest) = import
        .strip_prefix("~/")
        .or_else(|| import.strip_prefix("~\\"))
    {
        crate::paths::home_dir()?.join(rest)
    } else {
        let path = Path::new(import);
        if path.is_absolute() {
            path.to_owned()
        } else {
            source.parent()?.join(path)
        }
    };
    let canonical = fs::canonicalize(joined).ok()?;
    if !canonical.starts_with(root) {
        tracing::warn!(path = %canonical.display(), "拒绝指令根之外的 @ 导入");
        return None;
    }
    Some(canonical)
}

fn deduplicate_and_bound(files: Vec<ContextFile>, budget: u64) -> Vec<ContextFile> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut remaining = budget;
    let mut omitted = 0usize;
    for mut file in files {
        if !seen.insert(file.content.clone()) {
            continue;
        }
        let tokens = crate::compact::estimate_tokens(&file.content);
        if tokens > remaining {
            let included =
                crate::compact::prefix_within_tokens(&file.content, remaining.saturating_sub(24));
            if !included.is_empty() {
                file.content = format!(
                    "{included}\n[This file was truncated by the instruction budget; read the original if needed]"
                );
                result.push(file);
            }
            remaining = 0;
            omitted += 1;
        } else {
            remaining -= tokens;
            result.push(file);
        }
    }
    if omitted > 0
        && let Some(last) = result.last_mut()
    {
        last.content.push_str(&format!(
            "\n[Instruction budget: {budget} tokens; {omitted} files were not fully injected]"
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
        fs::create_dir_all(root.path().join(".git")).expect("仓库标记");
        fs::write(
            root.path().join("AGENTS.md"),
            "root rules\n@shared.md\n```md\n@ignored.md\n```\n`@missing.md`\n@absent.md\n",
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
        let files = load_context_files(
            &root.path().join("sub"),
            home.path(),
            &crate::config::ContextFilesConfig::default(),
        );
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
        // 行内反引号标记与解析失败的标记都原样保留，不展开也不清除。
        assert!(files[0].content.contains("`@missing.md`"));
        assert!(files[0].content.contains("@absent.md"));
    }

    fn prompt_for(cwd: &Path, home: &Path) -> String {
        load_context_files(cwd, home, &crate::config::ContextFilesConfig::default())
            .iter()
            .map(|file| file.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn imports_outside_the_instruction_root_are_rejected() {
        let parent = tempfile::tempdir().expect("临时父目录");
        let root = parent.path().join("project");
        let home = parent.path().join("home");
        fs::create_dir_all(root.join("sub")).expect("项目目录");
        fs::create_dir_all(root.join(".git")).expect("仓库标记");
        fs::create_dir_all(&home).expect("home");
        let secret = parent.path().join("secret.md");
        fs::write(&secret, "outside secret\n").expect("工作区外文件");
        fs::write(root.join("shared.md"), "inside shared\n").expect("工作区内文件");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, root.join("link.md")).expect("越界符号链接");
        fs::write(
            root.join("AGENTS.md"),
            format!(
                "root rules\n@shared.md\n@{}\n@../secret.md\n@link.md\n",
                secret.display()
            ),
        )
        .expect("根规则");
        let prompt = prompt_for(&root.join("sub"), &home);
        assert!(prompt.contains("inside shared"), "根内导入应保留：{prompt}");
        assert!(
            !prompt.contains("outside secret"),
            "指令根之外的文件不得进入系统提示：{prompt}"
        );
        assert!(prompt.contains("../secret.md"));
    }
}
