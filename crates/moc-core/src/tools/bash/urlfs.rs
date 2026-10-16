//! 内部 URI 在 shell 里的兜底：执行前把未加引号的 URI 词解析成 urlfs 农场路径。
//!
//! 提示词禁止把 URI 交给 shell（read/grep/find 才是它们的出口），但模型不守规则、
//! 用户手敲 `!` 命令时都不能静默落空，于是按 scheme 在会话 sidecar 里维护一棵符号
//! 链接农场，把 `artifact://jobs/x.log` 这样的词换成真实路径再执行。外部程序
//! （rg、git、python…）因此也能直接读这些资源。
//!
//! 引号内的词保持字面量：搜索模式、heredoc 内容与数据都不能被改成路径。变量或
//! 命令替换里拼出的 URI 同样不解析——那条路只有内嵌 shell 才能做到。

use crate::session::store::SessionPaths;
use crate::skills::SkillIndex;
use crate::tools::ToolError;

/// 有本地农场映射的 scheme（`viking://`/`ov://` 是远程资源，没有本地路径）。
#[cfg(unix)]
const FARM_SCHEMES: [&str; 4] = ["artifact", "local", "history", "skill"];

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use super::scan_words;

/// 把命令里未加引号的内部 URI 词换成农场路径；没有此类词时返回 `None`。
pub(crate) fn prepare(
    paths: &SessionPaths,
    skills: &SkillIndex,
    command: &str,
) -> Result<Option<String>, ToolError> {
    #[cfg(not(unix))]
    {
        // 农场是符号链接布局，只有 unix 有；Windows 上 URI 保持字面量。
        let _ = (paths, skills, command);
        return Ok(None);
    }
    #[cfg(unix)]
    {
        let mut targets = Vec::new();
        for word in scan_words(command) {
            if word.quoted {
                continue;
            }
            let Some((scheme, remainder)) = split_scheme(word.text) else {
                continue;
            };
            targets.push((word.start, word.end, scheme, remainder));
        }
        if targets.is_empty() {
            return Ok(None);
        }
        let farm = ensure_farm(paths, skills)
            .map_err(|error| ToolError::new("E_URLFS", error.to_string()))?;
        let mut rewrites = Vec::with_capacity(targets.len());
        for (start, end, scheme, remainder) in targets {
            let (head, tail) = split_glob(remainder);
            let mut target = farm.to_string_lossy().into_owned();
            target.push('/');
            target.push_str(scheme);
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

/// 词里的 scheme 与 `scheme://` 之后的部分；不是农场 scheme 时返回 `None`。
#[cfg(unix)]
fn split_scheme(word: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = word.split_once("://")?;
    FARM_SCHEMES
        .contains(&scheme)
        .then_some((scheme, remainder))
}

/// 第一个 glob 元字符前后的两段：路径前缀加引号，glob 尾巴原样保留让 shell 展开
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

/// 建立/刷新农场并返回其根目录；相对链接保证会话目录整体搬移后仍有效。
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

/// skill 链接是绝对路径：索引里的 base_dir 可能在工作区、也可能是用户级目录。
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
    // 本次索引里没有的旧链接一并清掉：农场只反映当前索引。
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

/// skill 名字要能当目录项：名字就是 URI authority，路径分隔符在这里没有意义。
#[cfg(unix)]
fn link_name(name: &str) -> Option<&str> {
    let valid = !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..";
    valid.then_some(name)
}

/// 建立符号链接；目标相同直接跳过，旧的其它类型先移除。
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
        let farm = ensure_farm(&paths, &SkillIndex::empty()).expect("农场");
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
        // 相对链接要真的指到 sidecar：写进农场等于写进会话目录。
        fs::write(paths.sidecar.join("plans/plan_1.md"), "# plan\n").expect("计划");
        assert_eq!(
            fs::read_to_string(farm.join("local/plan_1.md")).expect("农场读计划"),
            "# plan\n"
        );
        // 幂等：重复调用不改链接目标。
        let again = ensure_farm(&paths, &SkillIndex::empty()).expect("农场");
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
}
