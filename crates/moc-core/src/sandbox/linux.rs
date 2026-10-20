//! Linux bubblewrap 后端：profile 生成、能力探测与命令包装。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use moc_protocol::SessionPermission;

use super::{
    SandboxPolicy, SandboxRunner, SandboxScratch, SandboxSupport, WrappedCommand, is_within,
    resolve_access_path, runtime_read_roots,
};
use crate::config::{OnDenied, ResolvedSandboxConfig};
use crate::shell::ShellChoice;
use crate::tools::ToolError;

pub(crate) fn support() -> SandboxSupport {
    if std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
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
        runner: Some(SandboxRunner::Bubblewrap),
        modes,
        home_overlay: workspace && probe_overlay(),
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
    let Ok(wrapped) = wrap(shell, "true", workspace.path(), &policy) else {
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

fn probe_overlay() -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    if !home.is_dir() {
        return false;
    }
    let Ok(scratch) = SandboxScratch::new() else {
        return false;
    };
    let Ok(workspace) = tempfile::tempdir() else {
        return false;
    };
    let Some(shell) = crate::shell::detect() else {
        return false;
    };
    let mut config = probe_config(SessionPermission::Workspace);
    config.overlay_home = true;
    let policy = SandboxPolicy::new(
        SessionPermission::Workspace,
        workspace.path().to_path_buf(),
        &config,
        &Default::default(),
        &Default::default(),
        Some(Arc::new(scratch)),
    );
    let Ok(wrapped) = wrap(shell, "true", workspace.path(), &policy) else {
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
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<WrappedCommand, ToolError> {
    let scratch = policy.scratch()?;
    let (shell_program, shell_args) = crate::shell::command_line(shell, command);
    let mut args: Vec<OsString> = Vec::new();
    push_args(
        &mut args,
        &[
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--disable-userns",
            "--cap-drop",
            "ALL",
        ],
    );
    if !policy.network {
        args.push("--unshare-net".into());
    }

    let mut envs = vec![
        (OsString::from("TMPDIR"), OsString::from("/tmp")),
        (OsString::from("TMP"), OsString::from("/tmp")),
        (OsString::from("TEMP"), OsString::from("/tmp")),
        (
            OsString::from("XDG_CACHE_HOME"),
            OsString::from("/tmp/cache"),
        ),
    ];
    let mut overlay_work = None;

    match policy.mode {
        SessionPermission::Workspace => {
            push_bind(&mut args, "--ro-bind", Path::new("/"), Path::new("/"));
            push_bind(&mut args, "--bind", &scratch.tmp, Path::new("/tmp"));
            push_bind(&mut args, "--bind", &scratch.var_tmp, Path::new("/var/tmp"));
            if policy.overlay_home
                && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
                && home.is_dir()
            {
                let home = std::fs::canonicalize(&home).unwrap_or(home);
                if home != Path::new("/") {
                    match scratch.new_overlay_work() {
                        Ok(work) => {
                            args.push("--overlay-src".into());
                            args.push(home.clone().into_os_string());
                            args.push("--overlay".into());
                            args.push(scratch.overlay_upper.clone().into_os_string());
                            args.push(work.path().as_os_str().to_owned());
                            args.push(home.into_os_string());
                            overlay_work = Some(work);
                        }
                        Err(error) => {
                            return Err(ToolError::new(
                                "E_SANDBOX_UNAVAILABLE",
                                format!("cannot create sandbox overlay workdir: {error}"),
                            ));
                        }
                    }
                }
            }
        }
        SessionPermission::WorkspaceOnly => {
            for root in runtime_read_roots() {
                let path = Path::new(root);
                if path.exists() {
                    let source = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
                    push_bind(&mut args, "--ro-bind", &source, path);
                }
            }
            push_bind(&mut args, "--bind", &scratch.tmp, Path::new("/tmp"));
            push_bind(&mut args, "--bind", &scratch.var_tmp, Path::new("/var/tmp"));
            push_bind(&mut args, "--bind", &scratch.home, Path::new("/tmp/home"));
            envs.push((OsString::from("HOME"), OsString::from("/tmp/home")));
        }
        SessionPermission::Full => unreachable!("full 模式由 wrap_command 直接处理"),
    }

    for path in &policy.extra_read {
        if !is_within(path, &policy.workspace) {
            push_bind(&mut args, "--ro-bind", path, path);
        }
    }
    for path in &policy.extra_write {
        if !is_within(path, &policy.workspace) {
            push_bind(&mut args, "--bind", path, path);
        }
    }
    // workspace 最后绑定，保证它不会被祖先目录的 extra 规则降级为只读。
    push_bind(&mut args, "--bind", &policy.workspace, &policy.workspace);
    push_args(&mut args, &["--dev", "/dev", "--proc", "/proc"]);
    push_deny_masks(&mut args, policy);
    args.push("--chdir".into());
    args.push(cwd.as_os_str().to_owned());
    args.push("--".into());
    args.push(shell_program.into_os_string());
    args.extend(shell_args);

    Ok(WrappedCommand {
        program: OsString::from("bwrap"),
        args,
        envs,
        scratch: Some(scratch),
        overlay_work,
    })
}

fn push_args(args: &mut Vec<OsString>, values: &[&str]) {
    args.extend(values.iter().map(OsString::from));
}

fn push_bind(args: &mut Vec<OsString>, flag: &str, source: &Path, destination: &Path) {
    args.push(flag.into());
    args.push(source.as_os_str().to_owned());
    args.push(destination.as_os_str().to_owned());
}

fn push_deny_masks(args: &mut Vec<OsString>, policy: &SandboxPolicy) {
    let Ok(scratch) = policy.scratch() else {
        return;
    };
    let mut seen = Vec::new();
    for path in &policy.deny_read {
        if skip_deny_mask(path, &policy.workspace, &seen) {
            continue;
        }
        seen.push(path.clone());
        let Some(resolved) = resolve_access_path(path) else {
            continue;
        };
        match std::fs::metadata(&resolved) {
            Ok(metadata) if metadata.is_dir() => {
                args.push("--tmpfs".into());
                args.push(path.as_os_str().to_owned());
                args.push("--remount-ro".into());
                args.push(path.as_os_str().to_owned());
            }
            Ok(_) => push_bind(args, "--ro-bind", &scratch.deny_file, path),
            Err(_) => {}
        }
    }
    // 只拒绝写入时保留原内容：把同一路径以只读方式重新挂载。
    for path in &policy.deny_write {
        if skip_deny_mask(path, &policy.workspace, &seen) {
            continue;
        }
        seen.push(path.clone());
        let Some(resolved) = resolve_access_path(path) else {
            continue;
        };
        if std::fs::metadata(&resolved).is_ok() {
            push_bind(args, "--ro-bind", &resolved, path);
        }
    }
}

fn skip_deny_mask(path: &Path, workspace: &Path, seen: &[PathBuf]) -> bool {
    seen.iter().any(|seen| seen.as_path() == path)
        || path == Path::new("/")
        || path == workspace
        || is_within(workspace, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Access, Decision, SandboxGrants};

    fn policy(mode: SessionPermission) -> SandboxPolicy {
        SandboxPolicy::new(
            mode,
            PathBuf::from("/work"),
            &probe_config(mode),
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        )
    }

    #[test]
    fn workspace_profile_is_readonly_root_and_writable_workspace() {
        let wrapped = wrap(
            crate::shell::detect().expect("shell"),
            "true",
            Path::new("/work"),
            &policy(SessionPermission::Workspace),
        )
        .expect("wrap");
        let args: Vec<String> = wrapped
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(args.windows(3).any(|w| w == ["--bind", "/work", "/work"]));
        assert!(!args.iter().any(|arg| arg == "--unshare-net"));
    }

    #[test]
    fn workspace_only_does_not_bind_root() {
        let wrapped = wrap(
            crate::shell::detect().expect("shell"),
            "true",
            Path::new("/work"),
            &policy(SessionPermission::WorkspaceOnly),
        )
        .expect("wrap");
        let args: Vec<String> = wrapped
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(
            args.windows(3)
                .any(|w| w[0] == "--bind" && w[2] == "/tmp/home")
        );
        assert!(
            wrapped
                .envs
                .iter()
                .any(|(key, value)| key == "HOME" && value == "/tmp/home")
        );
    }

    #[test]
    fn network_false_adds_unshare_net() {
        let mut config = probe_config(SessionPermission::Workspace);
        config.network = false;
        let policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            PathBuf::from("/work"),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let wrapped = wrap(
            crate::shell::detect().expect("shell"),
            "true",
            Path::new("/work"),
            &policy,
        )
        .expect("wrap");
        assert!(wrapped.args.iter().any(|arg| arg == "--unshare-net"));
    }

    #[test]
    fn deny_write_path_inside_workspace_is_read_only() {
        let workspace = tempfile::tempdir().expect("workspace");
        let secret = workspace.path().join("secret");
        std::fs::write(&secret, "x").unwrap();
        let mut config = probe_config(SessionPermission::Workspace);
        config.deny_write_paths = vec![secret.clone()];
        let policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            workspace.path().to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let wrapped = wrap(
            crate::shell::detect().expect("shell"),
            "true",
            workspace.path(),
            &policy,
        )
        .expect("wrap");
        assert_eq!(policy.check(&secret, Access::Write), Decision::Deny);
        let resolved = std::fs::canonicalize(&secret).expect("canonical secret");
        assert!(
            wrapped.args.windows(3).any(|w| w[0] == "--ro-bind"
                && w[1].as_os_str() == resolved.as_os_str()
                && w[2].as_os_str() == secret.as_os_str()),
            "deny_write 必须把原路径以只读方式重挂，读内容仍保留"
        );
    }

    #[test]
    fn deny_read_path_is_replaced_with_an_empty_read_only_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        let secret = workspace.path().join("secret");
        std::fs::write(&secret, "secret").unwrap();
        let mut config = probe_config(SessionPermission::Workspace);
        config.deny_read_paths = vec![secret.clone()];
        let policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            workspace.path().to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let wrapped = wrap(
            crate::shell::detect().expect("shell"),
            "true",
            workspace.path(),
            &policy,
        )
        .expect("wrap");
        assert_eq!(policy.check(&secret, Access::Read), Decision::Deny);
        let deny_file = policy.scratch().expect("scratch").deny_file.clone();
        assert!(
            wrapped
                .args
                .windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1].as_os_str() == deny_file.as_os_str()),
            "deny_read 文件必须绑定只读空文件"
        );
    }
}
