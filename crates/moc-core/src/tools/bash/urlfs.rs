//! 内部 URI 在 shell 里的后备处理：执行前把没加引号的 URI 词换成 urlfs 目录里的真实路径。
//!
//! 提示词禁止把 URI 交给 shell（应该用 read/grep/find），但模型可能不守规则，用户手敲的
//! `!` 命令也可能带 URI，这些情况都不能悄悄失败。所以按 scheme 在会话 sidecar 里维护
//! 一个由符号链接组成的目录（下称 urlfs 目录），把 `artifact://jobs/x.log` 这样的词换成
//! 真实路径再执行。外部程序（rg、git、python 等）因此也能直接读这些资源。
//!
//! 引号里的词按字面量保留：搜索模式、heredoc 内容和数据都不能被改成路径。通过变量或
//! 命令替换拼出来的 URI 也不解析，那只有内嵌 shell 才做得到。

use crate::session::store::SessionPaths;
use crate::skills::SkillIndex;
use crate::tools::ToolError;

/// 在 urlfs 目录里有本地映射的 scheme（`viking://`、`ov://` 是远程资源，没有本地路径）。
#[cfg(unix)]
const FARM_SCHEMES: [&str; 4] = ["artifact", "local", "history", "skill"];

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::{Path, PathBuf};

/// 把命令里没加引号的内部 URI 词换成 urlfs 路径；没有这类词时返回 `None`。
pub(crate) fn prepare(
    paths: &SessionPaths,
    skills: &SkillIndex,
    command: &str,
) -> Result<Option<String>, ToolError> {
    #[cfg(not(unix))]
    {
        // urlfs 目录靠符号链接实现，只在 unix 上有；Windows 上 URI 保持原样。
        let _ = (paths, skills, command);
        return Ok(None);
    }
    #[cfg(unix)]
    {
        let Some(scan) = super::syntax::scan(command) else {
            return Ok(None);
        };
        let mut targets = Vec::new();
        for word in scan.bare_words {
            let Some((scheme, remainder)) = split_scheme(&word.text) else {
                continue;
            };
            targets.push((
                word.start,
                word.end,
                scheme.to_owned(),
                remainder.to_owned(),
            ));
        }
        if targets.is_empty() {
            return Ok(None);
        }
        let farm = ensure_farm(paths, skills)
            .map_err(|error| ToolError::new("E_URLFS", error.to_string()))?;
        let mut rewrites = Vec::with_capacity(targets.len());
        for (start, end, scheme, remainder) in targets {
            let (head, tail) = split_glob(&remainder);
            let mut target = farm.to_string_lossy().into_owned();
            target.push('/');
            target.push_str(&scheme);
            target.push('/');
            target.push_str(head);
            rewrites.push((start, end, format!("{}{tail}", quote(&target))));
        }
        // 从后往前替换：先改前面的词会让后面的字节偏移失效。
        let mut rewritten = command.to_owned();
        for (start, end, text) in rewrites.into_iter().rev() {
            rewritten.replace_range(start..end, &text);
        }
        Ok(Some(rewritten))
    }
}

/// 拆出词里的 scheme 和 `scheme://` 之后的部分；scheme 不在 urlfs 映射里时返回 `None`。
#[cfg(unix)]
fn split_scheme(word: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = word.split_once("://")?;
    FARM_SCHEMES
        .contains(&scheme)
        .then_some((scheme, remainder))
}

/// 在第一个 glob 元字符处拆成两段：路径前缀加引号，glob 部分原样留给 shell 展开
/// （`artifact://jobs/*.log` → `'<farm>/artifact/jobs/'*.log`）。
#[cfg(unix)]
fn split_glob(remainder: &str) -> (&str, &str) {
    match remainder.find(['*', '?', '[']) {
        Some(index) => remainder.split_at(index),
        None => (remainder, ""),
    }
}

/// 单引号包裹；词内单引号按 `'\''` 拆开，shell 拿到的仍是同一个路径。
#[cfg(unix)]
fn quote(text: &str) -> String {
    if !text.contains('\'') {
        return format!("'{text}'");
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// 创建或刷新 urlfs 目录并返回其根路径。用相对链接，会话目录整体搬走后链接仍然有效。
#[cfg(unix)]
fn ensure_farm(paths: &SessionPaths, skills: &SkillIndex) -> std::io::Result<PathBuf> {
    let farm = paths.sidecar.join("urlfs");
    let artifact = farm.join("artifact");
    let history = farm.join("history");
    let skill = farm.join("skill");
    fs::create_dir_all(&artifact)?;
    fs::create_dir_all(&history)?;
    fs::create_dir_all(&skill)?;
    link(&artifact.join("jobs"), Path::new("../../jobs"))?;
    link(&artifact.join("artifacts"), Path::new("../../artifacts"))?;
    link(&farm.join("local"), Path::new("../plans"))?;
    link(&history.join("compaction"), Path::new("../../compaction"))?;
    // transcript 在会话目录下、sidecar 之外，所以要往上多走一层。
    let Some(name) = paths.transcript.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "transcript path has no file name",
        ));
    };
    link(
        &history.join("transcript"),
        &Path::new("../../../").join(name),
    )?;
    sync_skill_links(&skill, skills)?;
    Ok(farm)
}

/// skill 链接用绝对路径：索引里的 base_dir 可能在工作区，也可能在用户级目录。
#[cfg(unix)]
fn sync_skill_links(directory: &Path, skills: &SkillIndex) -> std::io::Result<()> {
    let mut wanted = Vec::new();
    for skill in skills.list() {
        let Some(name) = link_name(&skill.name) else {
            continue;
        };
        wanted.push(name.to_owned());
        link(&directory.join(name), &skill.base_dir)?;
    }
    // 清掉本次索引里没有的旧链接，让目录只反映当前索引。
    for entry in fs::read_dir(directory)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if wanted.contains(&name) {
            continue;
        }
        if entry
            .file_type()
            .is_ok_and(|file_type| file_type.is_symlink())
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// 检查 skill 名能否用作目录项：名字就是 URI 的 authority，不能含路径分隔符。
#[cfg(unix)]
fn link_name(name: &str) -> Option<&str> {
    let valid = !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..";
    valid.then_some(name)
}

/// 创建符号链接；已有指向相同目标的链接就跳过，已有其它类型的项先删除。
#[cfg(unix)]
fn link(path: &Path, target: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() && fs::read_link(path)? == target {
                return Ok(());
            }
            if metadata.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::os::unix::fs::symlink(target, path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn paths(root: &Path) -> SessionPaths {
        SessionPaths {
            transcript: root.join("ses_1.jsonl"),
            sidecar: root.join("ses_1"),
        }
    }

    #[test]
    fn farm_links_point_at_sidecar_directories_and_can_be_refreshed() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        fs::create_dir_all(paths.sidecar.join("plans")).expect("plans");
        let farm = ensure_farm(&paths, &SkillIndex::empty()).expect("创建 urlfs 目录");
        assert_eq!(
            fs::read_link(farm.join("artifact/jobs")).expect("jobs 链接"),
            Path::new("../../jobs")
        );
        assert_eq!(
            fs::read_link(farm.join("artifact/artifacts")).expect("artifacts 链接"),
            Path::new("../../artifacts")
        );
        assert_eq!(
            fs::read_link(farm.join("local")).expect("local 链接"),
            Path::new("../plans")
        );
        assert_eq!(
            fs::read_link(farm.join("history/compaction")).expect("compaction 链接"),
            Path::new("../../compaction")
        );
        assert_eq!(
            fs::read_link(farm.join("history/transcript")).expect("transcript 链接"),
            Path::new("../../../ses_1.jsonl")
        );
        // 相对链接必须真的指向 sidecar：通过 urlfs 目录读到的就是会话目录里的文件。
        fs::write(paths.sidecar.join("plans/plan_1.md"), "# plan\n").expect("计划");
        assert_eq!(
            fs::read_to_string(farm.join("local/plan_1.md")).expect("通过 urlfs 读计划"),
            "# plan\n"
        );
        // 幂等：重复调用不改链接目标。
        let again = ensure_farm(&paths, &SkillIndex::empty()).expect("再次创建 urlfs 目录");
        assert_eq!(farm, again);
        assert_eq!(
            fs::read_link(farm.join("artifact/jobs")).expect("jobs 链接"),
            Path::new("../../jobs")
        );
    }

    #[test]
    fn unquoted_uris_become_farm_paths_and_quoted_ones_stay_literal() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        let skills = SkillIndex::empty();
        let command =
            "tail -3 artifact://jobs/job_1.log; grep -n local: 'artifact://jobs' local://";
        let rewritten = prepare(&paths, &skills, command)
            .expect("改写")
            .expect("有 URI 就必须改写");
        let farm = paths.sidecar.join("urlfs");
        let farm = farm.display();
        assert_eq!(
            rewritten,
            format!(
                "tail -3 '{farm}/artifact/jobs/job_1.log'; grep -n local: 'artifact://jobs' '{farm}/local/'"
            )
        );
        assert!(
            prepare(&paths, &skills, "cargo test --workspace")
                .expect("无 URI")
                .is_none()
        );
        assert!(
            prepare(&paths, &skills, "rg -n 'artifact://' crates/")
                .expect("引号内是字面量")
                .is_none()
        );
        assert!(
            prepare(&paths, &skills, "wc -l viking://resources/a.md ov://repo")
                .expect("远程 scheme 不映射")
                .is_none()
        );
    }

    #[test]
    fn glob_tails_stay_unquoted_and_last_wins_for_repeated_uris() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        let skills = SkillIndex::empty();
        let rewritten = prepare(
            &paths,
            &skills,
            "ls artifact://jobs/*.log local://a.md local://b.md",
        )
        .expect("改写")
        .expect("有 URI");
        let farm = paths.sidecar.join("urlfs");
        let farm = farm.display();
        assert_eq!(
            rewritten,
            format!("ls '{farm}/artifact/jobs/'*.log '{farm}/local/a.md' '{farm}/local/b.md'")
        );
    }

    #[test]
    fn heredoc_comments_redirections_and_assignments_stay_literal() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        let skills = SkillIndex::empty();
        for command in [
            "cat <<EOF\nartifact://jobs/x.log\nEOF",
            "echo hi # artifact://jobs/x.log",
            "echo hi > artifact://jobs/x.log",
            "FOO=artifact://jobs/x.log",
        ] {
            assert!(
                prepare(&paths, &skills, command).expect("解析").is_none(),
                "不应改写：{command}"
            );
        }
    }

    #[test]
    fn multibyte_prefix_keeps_byte_offsets() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        let skills = SkillIndex::empty();
        let command = "echo 中文 artifact://jobs/x.log";
        let rewritten = prepare(&paths, &skills, command)
            .expect("改写")
            .expect("有 URI");
        let farm = paths.sidecar.join("urlfs");
        assert_eq!(
            rewritten,
            format!("echo 中文 '{}/artifact/jobs/x.log'", farm.display())
        );
    }
}
