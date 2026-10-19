//! 内部 URI 在 shell 里的后备处理：执行前把没加引号的 URI 词换成可访问的路径。
//!
//! 提示词禁止把 URI 交给 shell（应该用 read/grep/find），但模型可能不守规则，用户手敲的
//! `!` 命令也可能带 URI，这些情况都不能悄悄失败。
//!
//! Unix 上按 scheme 在会话 sidecar 里维护一个由符号链接组成的目录（下称 urlfs 目录），
//! 把 `artifact://jobs/x.log` 这样的词换到该目录下。Windows 标准用户默认不能创建符号
//! 链接，junction 又只适用于本地 NTFS 目录，所以 Windows 直接把 scheme 解析到真实路径；
//! 外部程序（rg、git、python 等）同样能直接读这些资源。
//!
//! 引号里的词按字面量保留：搜索模式、heredoc 内容和数据都不能被改成路径。通过变量或
//! 命令替换拼出来的 URI 也不解析，那只有内嵌 shell 才做得到。PowerShell 与 cmd 的
//! 词法规则和 POSIX 不同，由 `windows` 子模块单独扫描。

use crate::session::store::SessionPaths;
use crate::shell::ShellKind;
use crate::skills::SkillIndex;
use crate::tools::ToolError;

mod windows;

/// 在 urlfs 目录里有本地映射的 scheme（`viking://`、`ov://` 是远程资源，没有本地路径）。
const FARM_SCHEMES: [&str; 4] = ["artifact", "local", "history", "skill"];

/// 一个待改写的 URI 词及其在命令里的字节范围。
struct UriWord {
    start: usize,
    end: usize,
    scheme: String,
    remainder: String,
}

/// 把命令里没加引号的内部 URI 词换成当前 shell 可访问的路径；没有这类词时返回 `None`。
pub(crate) fn prepare(
    paths: &SessionPaths,
    skills: &SkillIndex,
    command: &str,
) -> Result<Option<String>, ToolError> {
    let Some(shell) = crate::shell::detect() else {
        return Ok(None);
    };
    prepare_for(shell.kind, paths, skills, command)
}

/// 按显式 shell 类型改写；测试可以绕过进程级 shell 探测。
pub(crate) fn prepare_for(
    kind: ShellKind,
    paths: &SessionPaths,
    skills: &SkillIndex,
    command: &str,
) -> Result<Option<String>, ToolError> {
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (kind, paths, skills, command);
        return Ok(None);
    }
    #[cfg(any(unix, windows))]
    {
        let Some(targets) = collect_targets(kind, command) else {
            return Ok(None);
        };
        let rewrites = render_rewrites(kind, paths, skills, targets)?;
        // 从后往前替换：先改前面的词会让后面的字节偏移失效。
        let mut rewritten = command.to_owned();
        for (start, end, text) in rewrites.into_iter().rev() {
            rewritten.replace_range(start..end, &text);
        }
        Ok(Some(rewritten))
    }
}

fn collect_targets(kind: ShellKind, command: &str) -> Option<Vec<UriWord>> {
    let words = match kind {
        ShellKind::Bash | ShellKind::Zsh | ShellKind::Sh | ShellKind::GitBash => {
            super::syntax::scan(command)?.bare_words
        }
        ShellKind::PowerShell | ShellKind::Cmd => windows::scan(command, kind),
    };
    let targets: Vec<UriWord> = words
        .into_iter()
        .filter_map(|word| {
            let (scheme, remainder) = split_scheme(&word.text)?;
            Some(UriWord {
                start: word.start,
                end: word.end,
                scheme: scheme.to_owned(),
                remainder: remainder.to_owned(),
            })
        })
        .collect();
    (!targets.is_empty()).then_some(targets)
}

/// 拆出词里的 scheme 和 `scheme://` 之后的部分；scheme 不在 urlfs 映射里时返回 `None`。
fn split_scheme(word: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = word.split_once("://")?;
    FARM_SCHEMES
        .contains(&scheme)
        .then_some((scheme, remainder))
}

/// 在第一个 glob 元字符处拆成两段：路径前缀加引号，glob 部分原样留给 shell 展开
/// （`artifact://jobs/*.log` → `'<farm>/artifact/jobs/'*.log`）。
fn split_glob(remainder: &str) -> (&str, &str) {
    match remainder.find(['*', '?', '[']) {
        Some(index) => remainder.split_at(index),
        None => (remainder, ""),
    }
}

/// 单引号包裹；词内单引号按 `'\''` 拆开，shell 拿到的仍是同一个路径。
fn quote_posix(text: &str) -> String {
    if !text.contains('\'') {
        return format!("'{text}'");
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn render_rewrites(
    kind: ShellKind,
    paths: &SessionPaths,
    skills: &SkillIndex,
    targets: Vec<UriWord>,
) -> Result<Vec<(usize, usize, String)>, ToolError> {
    let farm =
        ensure_farm(paths, skills).map_err(|error| ToolError::new("E_URLFS", error.to_string()))?;
    let mut rewrites = Vec::with_capacity(targets.len());
    for target in targets {
        let (head, tail) = split_glob(&target.remainder);
        let mut text = farm.to_string_lossy().into_owned();
        text.push('/');
        text.push_str(&target.scheme);
        text.push('/');
        text.push_str(head);
        rewrites.push((
            target.start,
            target.end,
            format!("{}{tail}", windows::quote(kind, &text)),
        ));
    }
    Ok(rewrites)
}

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::{Path, PathBuf};

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

#[cfg(windows)]
use std::path::PathBuf;

#[cfg(windows)]
fn render_rewrites(
    kind: ShellKind,
    paths: &SessionPaths,
    skills: &SkillIndex,
    targets: Vec<UriWord>,
) -> Result<Vec<(usize, usize, String)>, ToolError> {
    let mut rewrites = Vec::with_capacity(targets.len());
    for target in targets {
        let path = direct_target(paths, skills, &target.scheme, &target.remainder)?;
        let text = command_path(&path, kind);
        let (head, tail) = split_glob(&text);
        rewrites.push((
            target.start,
            target.end,
            format!("{}{tail}", windows::quote(kind, head)),
        ));
    }
    Ok(rewrites)
}

/// Windows 的 shell 不经过 urlfs 目录，直接访问后备目录；Git Bash 需要 `/` 分隔。
/// skill 的 `base_dir` 来自 canonicalize，会带 `\\?\` 前缀；cmd 内建命令不认这种
/// 路径，所以先还原成普通绝对路径。
#[cfg(windows)]
fn command_path(path: &std::path::Path, kind: ShellKind) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        text = format!(r"\\{rest}");
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        text = rest.to_owned();
    }
    match kind {
        ShellKind::Bash | ShellKind::Zsh | ShellKind::Sh | ShellKind::GitBash => {
            text.replace('\\', "/")
        }
        ShellKind::PowerShell | ShellKind::Cmd => text,
    }
}

/// 把 URI 解析到真实路径。没有链接农场时，无法从一个目录同时列出 `artifact://`
/// 这类跨目录命名空间；裸根返回 `E_URLFS`，具体资源仍然全部可用。
#[cfg(windows)]
fn direct_target(
    paths: &SessionPaths,
    skills: &SkillIndex,
    scheme: &str,
    remainder: &str,
) -> Result<PathBuf, ToolError> {
    match scheme {
        "artifact" => {
            if remainder.is_empty() {
                return Err(bare_namespace_error("artifact"));
            }
            let (kind, rest) = remainder.split_once('/').unwrap_or((remainder, ""));
            match kind {
                "jobs" => Ok(paths.jobs().join(rest)),
                "artifacts" => Ok(paths.artifacts().join(rest)),
                _ => Ok(unsupported_path(paths, scheme, remainder)),
            }
        }
        "local" => Ok(paths.plans().join(remainder)),
        "history" => {
            if remainder.is_empty() {
                return Err(bare_namespace_error("history"));
            }
            if remainder == "transcript" {
                return Ok(paths.transcript.clone());
            }
            if let Some(rest) = remainder.strip_prefix("transcript/") {
                return Ok(paths.transcript.join(rest));
            }
            if remainder == "compaction" {
                return Ok(paths.compaction());
            }
            if let Some(rest) = remainder.strip_prefix("compaction/") {
                return Ok(paths.compaction().join(rest));
            }
            Ok(unsupported_path(paths, scheme, remainder))
        }
        "skill" => {
            if remainder.is_empty() {
                return Err(bare_namespace_error("skill"));
            }
            let (name, rest) = remainder.split_once('/').unwrap_or((remainder, ""));
            match skills.get(name) {
                Some(skill) => Ok(skill.base_dir.join(rest)),
                None => Ok(unsupported_path(paths, scheme, remainder)),
            }
        }
        _ => Ok(unsupported_path(paths, scheme, remainder)),
    }
}

#[cfg(windows)]
fn unsupported_path(paths: &SessionPaths, scheme: &str, remainder: &str) -> PathBuf {
    paths.sidecar.join("urlfs").join(scheme).join(remainder)
}

#[cfg(windows)]
fn bare_namespace_error(scheme: &str) -> ToolError {
    ToolError::new(
        "E_URLFS",
        format!(
            "cannot list {scheme}:// from a Windows shell; use read, grep, or find with the URI"
        ),
    )
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
        let rewritten = prepare_for(ShellKind::Bash, &paths, &skills, command)
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
            prepare_for(ShellKind::Bash, &paths, &skills, "cargo test --workspace")
                .expect("无 URI")
                .is_none()
        );
        assert!(
            prepare_for(
                ShellKind::Bash,
                &paths,
                &skills,
                "rg -n 'artifact://' crates/"
            )
            .expect("引号内是字面量")
            .is_none()
        );
        assert!(
            prepare_for(
                ShellKind::Bash,
                &paths,
                &skills,
                "wc -l viking://resources/a.md ov://repo"
            )
            .expect("远程 scheme 不映射")
            .is_none()
        );
    }

    #[test]
    fn glob_tails_stay_unquoted_and_last_wins_for_repeated_uris() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        let skills = SkillIndex::empty();
        let rewritten = prepare_for(
            ShellKind::Bash,
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
                prepare_for(ShellKind::Bash, &paths, &skills, command)
                    .expect("解析")
                    .is_none(),
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
        let rewritten = prepare_for(ShellKind::Bash, &paths, &skills, command)
            .expect("改写")
            .expect("有 URI");
        let farm = paths.sidecar.join("urlfs");
        assert_eq!(
            rewritten,
            format!("echo 中文 '{}/artifact/jobs/x.log'", farm.display())
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn paths(root: &Path) -> SessionPaths {
        SessionPaths {
            transcript: root.join("ses_1.jsonl"),
            sidecar: root.join("ses_1"),
        }
    }

    /// canonicalize 在 Windows 上返回 `\\?\` 前缀，覆盖 skill base_dir 的形态。
    fn canonical_session_base(root: &Path) -> PathBuf {
        fs::canonicalize(root)
            .expect("canonicalize")
            .join("session dir")
    }

    fn read_command(kind: ShellKind) -> &'static str {
        match kind {
            ShellKind::Bash | ShellKind::Zsh | ShellKind::Sh | ShellKind::GitBash => {
                "cat artifact://jobs/job_1.log"
            }
            ShellKind::PowerShell => "Get-Content artifact://jobs/job_1.log",
            ShellKind::Cmd => "type artifact://jobs/job_1.log",
        }
    }

    /// 找到本机可用的 shell 可执行文件；测试机器缺少某种 shell 时跳过。
    fn shell_program(kind: ShellKind) -> Option<PathBuf> {
        let first_file =
            |candidates: Vec<PathBuf>| candidates.into_iter().find(|path| path.is_file());
        match kind {
            ShellKind::GitBash => {
                if let Some(shell) = crate::shell::detect().filter(|shell| shell.kind == kind) {
                    return Some(shell.program.clone());
                }
                let base = std::env::var_os("ProgramFiles")?;
                first_file(vec![
                    PathBuf::from(base).join("Git").join("bin").join("bash.exe"),
                ])
            }
            ShellKind::PowerShell => {
                let mut candidates = Vec::new();
                if let Some(base) = std::env::var_os("ProgramFiles") {
                    candidates.push(PathBuf::from(base).join("PowerShell/7/pwsh.exe"));
                }
                if let Some(base) = std::env::var_os("SystemRoot") {
                    candidates.push(
                        PathBuf::from(base).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                    );
                }
                first_file(candidates)
            }
            ShellKind::Cmd => {
                if let Some(program) = std::env::var_os("COMSPEC")
                    .map(PathBuf::from)
                    .filter(|path| path.is_file())
                {
                    return Some(program);
                }
                let base = std::env::var_os("SystemRoot")?;
                first_file(vec![PathBuf::from(base).join("System32/cmd.exe")])
            }
            _ => None,
        }
    }

    #[test]
    fn direct_targets_cover_every_scheme() {
        let root = tempfile::tempdir().expect("临时目录");
        let paths = paths(root.path());
        fs::create_dir_all(paths.jobs()).expect("jobs");
        fs::create_dir_all(paths.artifacts()).expect("artifacts");
        fs::create_dir_all(paths.plans()).expect("plans");
        fs::create_dir_all(paths.compaction()).expect("compaction");
        fs::write(paths.jobs().join("job_1.log"), "job").expect("job");
        fs::write(paths.artifacts().join("out.txt"), "out").expect("artifact");
        fs::write(paths.plans().join("plan_1.md"), "plan").expect("plan");
        fs::write(paths.compaction().join("INDEX.md"), "index").expect("index");
        fs::write(&paths.transcript, "{}\n").expect("transcript");
        let skills = SkillIndex::empty();
        let read = |scheme: &str, remainder: &str| {
            let path = direct_target(&paths, &skills, scheme, remainder).expect("映射");
            fs::read_to_string(path).expect("读取")
        };
        assert_eq!(read("artifact", "jobs/job_1.log"), "job");
        assert_eq!(read("artifact", "artifacts/out.txt"), "out");
        assert_eq!(read("local", "plan_1.md"), "plan");
        assert_eq!(read("history", "transcript"), "{}\n");
        assert_eq!(read("history", "compaction/INDEX.md"), "index");
        assert_eq!(
            direct_target(&paths, &skills, "artifact", "jobs").expect("jobs 目录"),
            paths.jobs()
        );
        assert_eq!(
            direct_target(&paths, &skills, "local", "").expect("plans 目录"),
            paths.plans()
        );
        for scheme in ["artifact", "history", "skill"] {
            let error = direct_target(&paths, &skills, scheme, "").expect_err("裸命名空间");
            assert_eq!(error.code, "E_URLFS");
        }
        let missing =
            direct_target(&paths, &skills, "skill", "missing/SKILL.md").expect("未知 skill 映射");
        assert!(!missing.exists());
    }

    #[test]
    fn command_path_strips_verbatim_prefixes() {
        let normal = Path::new(r"C:\Users\me\skill");
        assert_eq!(command_path(normal, ShellKind::Cmd), r"C:\Users\me\skill");
        assert_eq!(
            command_path(normal, ShellKind::PowerShell),
            r"C:\Users\me\skill"
        );
        assert_eq!(
            command_path(normal, ShellKind::GitBash),
            "C:/Users/me/skill"
        );

        let verbatim = Path::new(r"\\?\C:\Users\me\skill");
        assert_eq!(command_path(verbatim, ShellKind::Cmd), r"C:\Users\me\skill");
        assert_eq!(
            command_path(verbatim, ShellKind::GitBash),
            "C:/Users/me/skill"
        );

        let unc = Path::new(r"\\?\UNC\server\share\skill");
        assert_eq!(command_path(unc, ShellKind::Cmd), r"\\server\share\skill");
        assert_eq!(
            command_path(unc, ShellKind::GitBash),
            "//server/share/skill"
        );
    }

    #[test]
    fn skill_targets_follow_discovered_base_dir() {
        let root = tempfile::tempdir().expect("临时目录");
        let skills_root = root.path().join("skills");
        let skill = skills_root.join("demo");
        fs::create_dir_all(&skill).expect("skill 目录");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo\ndescription: 演示\n---\nbody\n",
        )
        .expect("SKILL.md");
        let config = crate::config::SkillsConfig {
            moc_user: false,
            moc_project: false,
            agents_user: false,
            agents_project: false,
            dirs: vec![skills_root.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let index = crate::skills::discovery::discover_index(
            root.path(),
            &root.path().join("home"),
            None,
            &config,
        );
        assert!(index.get("demo").is_some(), "测试 skill 必须被发现");
        let paths = paths(root.path());
        let resolved = direct_target(&paths, &index, "skill", "demo/SKILL.md").expect("skill 映射");
        assert_eq!(
            fs::read_to_string(resolved).expect("读取 skill"),
            "---\nname: demo\ndescription: 演示\n---\nbody\n"
        );
    }

    #[test]
    fn prepare_for_quotes_uris_for_each_shell() {
        let root = tempfile::tempdir().expect("临时目录");
        let base = root.path().join("session dir");
        let paths = SessionPaths {
            transcript: base.join("ses_1.jsonl"),
            sidecar: base.join("ses_1"),
        };
        fs::create_dir_all(paths.jobs()).expect("jobs");
        let skills = SkillIndex::empty();

        let git = prepare_for(
            ShellKind::GitBash,
            &paths,
            &skills,
            "cat artifact://jobs/*.log",
        )
        .expect("修正")
        .expect("有 URI");
        let jobs = paths.jobs().to_string_lossy().replace('\\', "/");
        assert_eq!(git, format!("cat '{jobs}/'*.log"));

        let powershell = prepare_for(
            ShellKind::PowerShell,
            &paths,
            &skills,
            "Get-Content artifact://jobs/*.log",
        )
        .expect("修正")
        .expect("有 URI");
        let jobs_dir = paths.jobs();
        let jobs = jobs_dir.to_string_lossy();
        assert_eq!(powershell, format!("Get-Content '{jobs}\\'*.log"));

        let cmd = prepare_for(
            ShellKind::Cmd,
            &paths,
            &skills,
            "type artifact://jobs/*.log",
        )
        .expect("修正")
        .expect("有 URI");
        let jobs_dir = paths.jobs();
        let jobs = jobs_dir.to_string_lossy();
        assert_eq!(cmd, format!("type \"{jobs}\"\\*.log"));
    }

    #[test]
    fn active_shell_can_read_rewritten_uri() {
        let root = tempfile::tempdir().expect("临时目录");
        let base = canonical_session_base(root.path());
        let paths = SessionPaths {
            transcript: base.join("ses_1.jsonl"),
            sidecar: base.join("ses_1"),
        };
        fs::create_dir_all(paths.jobs()).expect("jobs");
        fs::write(paths.jobs().join("job_1.log"), "urlfs-ok\n").expect("日志");
        let Some(shell) = crate::shell::detect() else {
            return;
        };
        let command = read_command(shell.kind);
        let rewritten = prepare(&paths, &SkillIndex::empty(), command)
            .expect("修正")
            .expect("有 URI");
        let mut process = std::process::Command::new(&shell.program);
        crate::shell::apply_arguments(&mut process, shell, &rewritten);
        let output = process
            .current_dir(root.path())
            .output()
            .expect("启动 shell");
        assert!(
            output.status.success(),
            "shell 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("urlfs-ok"),
            "输出：{:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn every_available_shell_can_read_rewritten_uri() {
        let root = tempfile::tempdir().expect("临时目录");
        let base = canonical_session_base(root.path());
        let paths = SessionPaths {
            transcript: base.join("ses_1.jsonl"),
            sidecar: base.join("ses_1"),
        };
        fs::create_dir_all(paths.jobs()).expect("jobs");
        fs::write(paths.jobs().join("job_1.log"), "urlfs-ok\n").expect("日志");
        let skills = SkillIndex::empty();
        for kind in [ShellKind::GitBash, ShellKind::PowerShell, ShellKind::Cmd] {
            let Some(program) = shell_program(kind) else {
                continue;
            };
            let rewritten = prepare_for(kind, &paths, &skills, read_command(kind))
                .expect("修正")
                .expect("有 URI");
            let choice = crate::shell::ShellChoice { kind, program };
            let mut process = std::process::Command::new(&choice.program);
            crate::shell::apply_arguments(&mut process, &choice, &rewritten);
            let output = process
                .current_dir(root.path())
                .output()
                .expect("启动 shell");
            assert!(
                output.status.success(),
                "{kind:?} 失败：{}\n命令：{rewritten}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("urlfs-ok"),
                "{kind:?} 输出：{:?}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}
