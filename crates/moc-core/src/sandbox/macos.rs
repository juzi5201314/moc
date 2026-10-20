//! macOS Seatbelt 后端：SBPL profile 生成、能力探测与命令包装。
//!
//! `sandbox-exec` 已弃用但仍随 macOS 提供；profile 编译失败时只提供 full，
//! 不做“看起来像沙盒”的静默降级。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use moc_protocol::SessionPermission;

use super::{
    SandboxPolicy, SandboxRunner, SandboxScratch, SandboxSupport, WrappedCommand,
    runtime_read_roots,
};
use crate::config::{OnDenied, ResolvedSandboxConfig};
use crate::shell::ShellChoice;
use crate::tools::ToolError;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub(crate) fn support() -> SandboxSupport {
    if !Path::new(SANDBOX_EXEC).is_file() {
        return SandboxSupport::full_only();
    }
    let workspace = probe_mode(SessionPermission::Workspace);
    let workspace_only = probe_mode(SessionPermission::WorkspaceOnly);
    if !workspace && !workspace_only {
        return SandboxSupport::full_only();
    }
    let mut modes = vec![SessionPermission::Full];
    if workspace {
        modes.push(SessionPermission::Workspace);
    }
    if workspace_only {
        modes.push(SessionPermission::WorkspaceOnly);
    }
    SandboxSupport {
        runner: Some(SandboxRunner::Seatbelt),
        modes,
        home_overlay: false,
    }
}

fn probe_config(mode: SessionPermission) -> ResolvedSandboxConfig {
    ResolvedSandboxConfig {
        default_mode: mode,
        network: true,
        on_denied: OnDenied::Ask,
        allow_unsandboxed: true,
        overlay_home: false,
        extra_read_paths: Vec::new(),
        extra_write_paths: Vec::new(),
        deny_read_paths: Vec::new(),
        deny_write_paths: Vec::new(),
    }
}

fn probe_mode(mode: SessionPermission) -> bool {
    let Ok(scratch) = SandboxScratch::new() else {
        return false;
    };
    let Ok(workspace) = tempfile::tempdir() else {
        return false;
    };
    let Some(shell) = crate::shell::detect() else {
        return false;
    };
    let config = probe_config(mode);
    let policy = SandboxPolicy::new(
        mode,
        workspace.path().to_path_buf(),
        &config,
        &Default::default(),
        &Default::default(),
        Some(Arc::new(scratch)),
    );
    let Ok(wrapped) = wrap(shell, "true", &policy) else {
        return false;
    };
    std::process::Command::new(&wrapped.program)
        .args(&wrapped.args)
        .envs(wrapped.envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn wrap(
    shell: &ShellChoice,
    command: &str,
    policy: &SandboxPolicy,
) -> Result<WrappedCommand, ToolError> {
    let scratch = policy.scratch()?;
    let (shell_program, shell_args) = crate::shell::command_line(shell, command);
    let profile = profile(policy, &scratch);
    let mut args: Vec<OsString> = vec!["-p".into(), profile.into(), "--".into()];
    args.push(shell_program.into_os_string());
    args.extend(shell_args);

    let mut envs = vec![
        (OsString::from("TMPDIR"), scratch.tmp.as_os_str().to_owned()),
        (OsString::from("TMP"), scratch.tmp.as_os_str().to_owned()),
        (OsString::from("TEMP"), scratch.tmp.as_os_str().to_owned()),
        (
            OsString::from("XDG_CACHE_HOME"),
            scratch.tmp.join("cache").into_os_string(),
        ),
    ];
    if policy.mode == SessionPermission::WorkspaceOnly {
        envs.push((OsString::from("HOME"), scratch.home.as_os_str().to_owned()));
    }

    Ok(WrappedCommand {
        program: OsString::from(SANDBOX_EXEC),
        args,
        envs,
        scratch: Some(scratch),
        overlay_work: None,
    })
}

fn profile(policy: &SandboxPolicy, scratch: &SandboxScratch) -> String {
    let mut rules = vec!["(version 1)".to_owned(), "(allow default)".to_owned()];
    if policy.mode == SessionPermission::WorkspaceOnly {
        rules.push("(deny file-read-data)".to_owned());
        let mut reads = runtime_read_roots()
            .iter()
            .copied()
            .map(PathBuf::from)
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        reads.push(policy.workspace.clone());
        reads.push(scratch.tmp.clone());
        reads.push(scratch.var_tmp.clone());
        reads.push(scratch.home.clone());
        reads.extend(policy.extra_read.iter().cloned());
        reads.extend(policy.extra_write.iter().cloned());
        reads.extend(policy.grant_read.iter().cloned());
        reads.extend(policy.grant_write.iter().cloned());
        push_allow(&mut rules, "file-read-data", &reads);
        push_deny(&mut rules, "file-read-data", &policy.deny_read);
    } else {
        push_deny(&mut rules, "file-read-data", &policy.deny_read);
    }

    rules.push("(deny file-write*)".to_owned());
    let mut writes = vec![
        policy.workspace.clone(),
        scratch.tmp.clone(),
        scratch.var_tmp.clone(),
        scratch.home.clone(),
    ];
    writes.extend(policy.extra_write.iter().cloned());
    writes.extend(policy.grant_write.iter().cloned());
    writes.push(PathBuf::from("/dev"));
    push_allow(&mut rules, "file-write*", &writes);
    push_deny(&mut rules, "file-write*", &policy.deny_write);

    if !policy.network {
        rules.push("(deny network*)".to_owned());
    }
    rules.join(" ")
}

fn push_allow(rules: &mut Vec<String>, operation: &str, paths: &[PathBuf]) {
    let paths = unique_existing(paths);
    if paths.is_empty() {
        return;
    }
    let forms = paths
        .iter()
        .map(|path| format!("(subpath {})", sbpl_string(path)))
        .collect::<Vec<_>>()
        .join(" ");
    rules.push(format!("(allow {operation} {forms})"));
}

fn push_deny(rules: &mut Vec<String>, operation: &str, paths: &[PathBuf]) {
    let paths = unique_existing(paths);
    if paths.is_empty() {
        return;
    }
    let forms = paths
        .iter()
        .map(|path| format!("(subpath {})", sbpl_string(path)))
        .collect::<Vec<_>>()
        .join(" ");
    rules.push(format!("(deny {operation} {forms})"));
}

fn unique_existing(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut output: Vec<PathBuf> = Vec::new();
    for path in paths {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if path.exists() && !output.contains(&path) {
            output.push(path);
        }
    }
    output.sort();
    output
}

fn sbpl_string(path: &Path) -> String {
    let text = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{text}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxGrants;

    fn policy(mode: SessionPermission, workspace: &Path) -> SandboxPolicy {
        SandboxPolicy::new(
            mode,
            workspace.to_path_buf(),
            &probe_config(mode),
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        )
    }

    #[test]
    fn workspace_profile_allows_writes_only_under_roots() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = policy(SessionPermission::Workspace, workspace.path());
        let scratch = policy.scratch().expect("scratch");
        let profile = profile(&policy, &scratch);
        let workspace = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
        assert!(profile.starts_with("(version 1) (allow default)"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains(format!("(subpath {})", sbpl_string(&workspace)).as_str()));
        assert!(!profile.contains("(deny network*)"));
    }

    #[test]
    fn workspace_only_denies_reads_and_network() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut config = probe_config(SessionPermission::WorkspaceOnly);
        config.network = false;
        let policy = SandboxPolicy::new(
            SessionPermission::WorkspaceOnly,
            workspace.path().to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let scratch = policy.scratch().expect("scratch");
        let profile = profile(&policy, &scratch);
        assert!(profile.contains("(deny file-read-data)"));
        assert!(profile.contains("(deny network*)"));
    }

    #[test]
    fn sbpl_escapes_quotes_and_backslashes() {
        assert_eq!(sbpl_string(Path::new(r#"/a"b\c"#)), r#""/a\"b\\c""#);
    }
}
