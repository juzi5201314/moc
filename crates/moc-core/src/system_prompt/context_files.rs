//! 加载项目和全局的 AGENTS.md，并展开其中的 `@` 导入（有大小上限）。

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 单个指令文件和整条 `@` 导入链的字节上限。解码和 token 预算都在读取之后才生效，
/// 所以读取时就得截断：否则 `@/dev/zero` 会无限增长，FIFO 会卡住启动。
const MAX_CONTEXT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_IMPORT_TOTAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// 从仓库根（不在仓库里则从 cwd）逐级读到 cwd 的 AGENTS.md，最后拼上全局规则。
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
    let project_files = files.len();
    if config.user {
        push_file(
            moc_home.join("AGENTS.md"),
            &canonical_or_owned(moc_home),
            config.import_depth,
            &mut visited,
            &mut files,
        );
    }
    deduplicate_and_bound(files, project_files, config.max_tokens)
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

pub(crate) fn git_root(cwd: &Path) -> Option<PathBuf> {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        && output.status.success()
        && let Ok(path) = String::from_utf8(output.stdout)
    {
        return Some(PathBuf::from(path.trim()));
    }
    enclosing_repository(cwd)
}

/// git 不可用或报错时，向上查找 `.git` 标记；找不到仓库就返回 `None`。
/// 不能退到文件系统根，否则 `/` 下的 AGENTS.md 会被当成项目指令。
fn enclosing_repository(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(directory) = current {
        if directory.join(".git").exists() {
            return Some(directory.to_owned());
        }
        current = directory.parent();
    }
    None
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
    let Some(content) = read_bounded_text(&path) else {
        return;
    };
    if content.trim().is_empty() {
        return;
    }
    let mut budget = MAX_IMPORT_TOTAL_BYTES;
    let expanded = expand_imports(&path, &content, root, 0, max_depth, visited, &mut budget);
    output.push(ContextFile {
        path,
        content: expanded,
    });
}

/// 只读取大小在上限内的普通文件；目录、设备、FIFO 和超限文件跳过并告警。
fn read_bounded_text(path: &Path) -> Option<String> {
    let Ok(metadata) = fs::metadata(path) else {
        return None;
    };
    if !metadata.is_file() || metadata.len() > MAX_CONTEXT_FILE_BYTES {
        tracing::warn!(
            path = %path.display(),
            "跳过不是普通文件或超出上限的指令文件"
        );
        return None;
    }
    let mut content = String::new();
    fs::File::open(path)
        .ok()?
        // 检查元数据之后文件仍可能变大，读取量也要封顶。
        .take(MAX_CONTEXT_FILE_BYTES + 1)
        .read_to_string(&mut content)
        .ok()?;
    Some(content)
}

fn expand_imports(
    source_path: &Path,
    content: &str,
    root: &Path,
    depth: usize,
    max_depth: usize,
    visited: &mut HashSet<PathBuf>,
    budget: &mut usize,
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
        let Some(imported) = read_bounded_text(&canonical) else {
            result.push_str(line);
            continue;
        };
        if imported.len() > *budget {
            if *budget > 0 {
                tracing::warn!("@ 导入总量超过上限，后续导入被忽略");
                *budget = 0;
            }
            result.push_str(line);
            continue;
        }
        *budget -= imported.len();
        let expanded = expand_imports(
            &canonical,
            &imported,
            root,
            depth + 1,
            max_depth,
            visited,
            budget,
        );
        if !expanded.trim().is_empty() {
            result.push_str(&format!(
                "\n<file path=\"{}\">\n{}\n</file>\n",
                canonical.display(),
                expanded
            ));
        }
    }
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

/// 把 `@` 导入解析成规范路径。绝对路径、`~/` 和相对路径都必须在指令根之内；
/// 符号链接按解析后的真实路径判断，越界就拒绝。
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

fn deduplicate_and_bound(
    files: Vec<ContextFile>,
    project_files: usize,
    budget: u64,
) -> Vec<ContextFile> {
    let files = drop_contained(files, project_files);
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

/// 按段落去重：离工作目录更近的项目文件完整包含了前面某个项目文件时，不再注入前面那份。
/// 近处的文件本身更显眼，重复注入只会挤占指令预算。用户级文件是全局指令，不参与去重，
/// 不会因为仓库里某个文件包含了它就消失。
///
/// `files` 里的项目文件按「仓库根 → 当前目录」排列，只在项目文件之间比较。
fn drop_contained(files: Vec<ContextFile>, project_files: usize) -> Vec<ContextFile> {
    if project_files < 2 {
        return files;
    }
    let paragraphs: Vec<Vec<String>> = files
        .iter()
        .map(|file| {
            file.content
                .split("\n\n")
                .map(|paragraph| paragraph.trim().to_owned())
                .filter(|paragraph| !paragraph.is_empty())
                .collect()
        })
        .collect();
    let mut keep = vec![true; files.len()];
    for index in 0..project_files {
        if paragraphs[index].is_empty() {
            continue;
        }
        let contained = (index + 1..project_files)
            .any(|later| contains_sequence(&paragraphs[later], &paragraphs[index]));
        if contained {
            keep[index] = false;
        }
    }
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, file)| keep[index].then_some(file))
        .collect()
}

/// `needle` 是否作为连续片段出现在 `haystack` 中；空 `needle` 不算包含。
fn contains_sequence(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
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

    #[test]
    fn nearer_file_containing_the_ancestor_replaces_it() {
        let root = tempfile::tempdir().expect("临时项目");
        let home = tempfile::tempdir().expect("临时 home");
        fs::create_dir_all(root.path().join("sub")).expect("子目录");
        fs::create_dir_all(root.path().join(".git")).expect("仓库标记");
        fs::write(
            root.path().join("AGENTS.md"),
            "always run cargo fmt\n\nnever commit secrets\n",
        )
        .expect("根规则");
        fs::write(
            root.path().join("sub/AGENTS.md"),
            "always run cargo fmt\n\nnever commit secrets\n\nsubdir uses pnpm\n",
        )
        .expect("子目录规则");
        let files = load_context_files(
            &root.path().join("sub"),
            home.path(),
            &crate::config::ContextFilesConfig::default(),
        );
        let paths = files
            .iter()
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            paths.len(),
            1,
            "更近的副本完整包含祖先副本时只保留一份：{paths:?}"
        );
        assert!(paths[0].ends_with("sub/AGENTS.md"));
        assert!(prompt_for(&root.path().join("sub"), home.path()).contains("subdir uses pnpm"));
    }

    #[test]
    fn user_file_never_replaces_project_files_by_containment() {
        let root = tempfile::tempdir().expect("临时项目");
        let home = tempfile::tempdir().expect("临时 home");
        fs::create_dir_all(root.path().join("sub")).expect("子目录");
        fs::create_dir_all(root.path().join(".git")).expect("仓库标记");
        fs::create_dir_all(home.path()).expect("home");
        fs::write(root.path().join("AGENTS.md"), "project specific first\n").expect("根规则");
        fs::write(
            home.path().join("AGENTS.md"),
            "user global header\n\nproject specific first\n",
        )
        .expect("用户规则");
        let files = load_context_files(
            &root.path().join("sub"),
            home.path(),
            &crate::config::ContextFilesConfig::default(),
        );
        assert_eq!(
            files.len(),
            2,
            "用户级文件包含项目文件不得让项目文件消失：{:?}",
            files.iter().map(|f| f.path.clone()).collect::<Vec<_>>()
        );
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

    #[cfg(unix)]
    #[test]
    fn imports_reject_special_and_oversized_files() {
        use std::sync::mpsc;

        let root = tempfile::tempdir().expect("临时项目");
        let home = tempfile::tempdir().expect("临时 home");
        fs::create_dir_all(root.path().join(".git")).expect("仓库标记");
        fs::create_dir_all(root.path().join("sub")).expect("子目录");
        let fifo = root.path().join("pipe.md");
        let raw = std::ffi::CString::new(fifo.to_str().expect("路径是 UTF-8")).expect("CString");
        // SAFETY：mkfifo 只创建测试用的命名管道。
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
        fs::write(
            root.path().join("big.md"),
            "OVERSIZED-MARKER ".repeat(120_000),
        )
        .expect("超限文件");
        fs::write(root.path().join("ok.md"), "small rules\n").expect("小文件");
        fs::write(
            root.path().join("AGENTS.md"),
            "root rules\n@pipe.md\n@big.md\n@ok.md\n",
        )
        .expect("根规则");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let prompt = prompt_for(&root.path().join("sub"), home.path());
            let _ = sender.send(prompt);
        });
        let prompt = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("导入不得阻塞启动");
        assert!(prompt.contains("small rules"), "普通文件应保留：{prompt}");
        assert!(!prompt.contains("OVERSIZED-MARKER"), "超限文件不得进入提示");
    }

    #[test]
    fn repository_discovery_never_falls_back_to_the_filesystem_root() {
        let directory = tempfile::tempdir().expect("临时目录");
        assert_eq!(
            git_root(directory.path()),
            None,
            "非 Git 目录不得把 / 当作仓库根"
        );
        let repository = directory.path().join("repo");
        fs::create_dir_all(repository.join(".git")).expect("伪仓库目录");
        assert_eq!(
            git_root(&repository),
            Some(repository.clone()),
            "没有可用 git 命令时按 .git 标记向上查找"
        );
    }
}
