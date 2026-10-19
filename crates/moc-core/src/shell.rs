//! 平台 shell 的选择、启动参数与 PTY 输入。
//!
//! 目标只有一个：同一段命令只依赖确定的解释器语义。Unix 只接受 bash/zsh
//! （其余 shell 回退），Windows 按 Git Bash → PowerShell → cmd 回退。选择结果
//! 在进程内缓存，供 bash 工具、`!command` 密钥解析和系统提示共用。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 实际执行命令的 shell 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Sh,
    GitBash,
    PowerShell,
    Cmd,
}

impl ShellKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::GitBash => "git-bash",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd",
        }
    }
}

/// 选中的 shell：类型与可执行文件路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellChoice {
    pub kind: ShellKind,
    pub program: PathBuf,
}

impl ShellChoice {
    pub fn label(&self) -> &'static str {
        self.kind.label()
    }

    /// 提示词里展示的 `bash (/bin/bash)` 形式。
    pub fn summary(&self) -> String {
        format!("{} ({})", self.label(), self.program.display())
    }
}

static DETECTED: OnceLock<Option<ShellChoice>> = OnceLock::new();

/// 进程内第一次调用时探测并缓存；所有调用方看到同一个 shell。
pub fn detect() -> Option<&'static ShellChoice> {
    DETECTED
        .get_or_init(|| {
            resolve(
                std::env::var_os("SHELL").as_deref(),
                std::env::var_os("PATH").as_deref(),
                std::env::var_os("MOC_GIT_BASH_PATH").as_deref(),
            )
        })
        .as_ref()
}

/// 纯探测逻辑，文件系统与环境由调用方给出，便于测试。
pub fn resolve(
    shell: Option<&OsStr>,
    path: Option<&OsStr>,
    git_bash: Option<&OsStr>,
) -> Option<ShellChoice> {
    #[cfg(unix)]
    {
        let _ = git_bash;
        resolve_unix(shell, path)
    }
    #[cfg(windows)]
    {
        let _ = shell;
        resolve_windows(path, git_bash)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (shell, path, git_bash);
        None
    }
}

/// 生成非交互执行参数。返回程序路径和除程序名以外的参数。
pub fn command_line(choice: &ShellChoice, command: &str) -> (PathBuf, Vec<OsString>) {
    let mut args = Vec::new();
    match choice.kind {
        ShellKind::Bash | ShellKind::GitBash => {
            args.extend(["--noprofile", "--norc", "-c"].map(OsString::from));
            args.push(command.into());
        }
        ShellKind::Zsh => {
            // zsh 的 -f 等价于 no-rcs，避免用户配置改变非交互行为。
            args.extend(["-f", "-c"].map(OsString::from));
            args.push(command.into());
        }
        ShellKind::Sh => {
            args.extend(["-c"].map(OsString::from));
            args.push(command.into());
        }
        ShellKind::PowerShell => {
            args.extend(
                ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"].map(OsString::from),
            );
            args.push(wrap_powershell(command).into());
        }
        ShellKind::Cmd => {
            args.extend(["/D", "/S", "/C"].map(OsString::from));
            // chcp 让 cmd 的内建命令与重定向使用 UTF-8；括号隔离用户命令里的 &。
            args.push(format!("chcp 65001>nul & ({command})").into());
        }
    }
    (choice.program.clone(), args)
}

/// Windows PTY 先起一个空闲 shell：portable-pty 不能在挂起状态创建进程，
/// 必须先加入 Job Object 再发送命令，否则命令派生的子进程会逃出作业。
#[cfg(windows)]
pub fn pty_idle_command(choice: &ShellChoice) -> (PathBuf, Vec<OsString>) {
    let args = match choice.kind {
        ShellKind::Bash | ShellKind::GitBash => {
            vec![
                OsString::from("--noprofile"),
                OsString::from("--norc"),
                OsString::from("-i"),
            ]
        }
        ShellKind::Zsh => vec![OsString::from("-f"), OsString::from("-i")],
        ShellKind::Sh => vec![OsString::from("-i")],
        ShellKind::PowerShell => vec![
            OsString::from("-NoLogo"),
            OsString::from("-NoProfile"),
            OsString::from("-NoExit"),
        ],
        ShellKind::Cmd => vec![
            OsString::from("/D"),
            OsString::from("/Q"),
            OsString::from("/K"),
            OsString::from("chcp 65001>nul & prompt $S"),
        ],
    };
    (choice.program.clone(), args)
}

/// 写入空闲 shell 的命令与用户 stdin。Windows 的分隔符与收尾语法按 shell 区分。
#[cfg(windows)]
pub fn pty_input(choice: &ShellChoice, command: &str, stdin: Option<&str>) -> Vec<u8> {
    let command = if command.trim().is_empty() {
        "rem"
    } else {
        command
    };
    let stdin = stdin.unwrap_or_default();
    match choice.kind {
        ShellKind::Cmd => {
            let command = command
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .replace('\n', "\r");
            // exit 和用户命令写在同一行，免得没读完的 stdin 被外层 cmd 当成命令执行。
            format!("({command}) 2>&1 & exit\r{stdin}").into_bytes()
        }
        ShellKind::Bash | ShellKind::GitBash | ShellKind::Zsh | ShellKind::Sh => {
            let command = command.replace("\r\n", "\n");
            format!("{command}\nexit\n{stdin}").into_bytes()
        }
        ShellKind::PowerShell => {
            let command = command.replace("\r\n", "\n");
            format!("{command}\nexit\n{stdin}").into_bytes()
        }
    }
}

/// 系统提示与工具描述共用的语法说明。
pub fn dialect(choice: &ShellChoice) -> &'static str {
    match choice.kind {
        ShellKind::Bash | ShellKind::GitBash => "POSIX/bash syntax",
        ShellKind::Zsh => "POSIX/bash-compatible syntax (zsh semantics apply)",
        ShellKind::Sh => "POSIX sh syntax",
        ShellKind::PowerShell => "PowerShell syntax",
        ShellKind::Cmd => "cmd.exe syntax",
    }
}

/// PowerShell 的 UTF-8 与退出码包装：`$LASTEXITCODE` 优先，cmdlet 失败回退 `$?`。
fn wrap_powershell(command: &str) -> String {
    format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8;\n\
         {command}\n\
         $moc_ok=$?; $moc_code=$LASTEXITCODE; \
         if ($null -ne $moc_code) {{ exit [int]$moc_code }}; \
         if (-not $moc_ok) {{ exit 1 }}; exit 0"
    )
}

#[cfg(unix)]
fn resolve_unix(shell: Option<&OsStr>, path: Option<&OsStr>) -> Option<ShellChoice> {
    let requested = shell
        .map(PathBuf::from)
        .filter(|candidate| is_bash_or_zsh(candidate) && is_executable(candidate));
    let prefer_bash = requested.as_ref().is_some_and(|candidate| {
        candidate
            .file_name()
            .is_some_and(|name| name.to_string_lossy().to_ascii_lowercase().contains("bash"))
    });
    let order = if prefer_bash {
        [("bash", ShellKind::Bash), ("zsh", ShellKind::Zsh)]
    } else {
        [("zsh", ShellKind::Zsh), ("bash", ShellKind::Bash)]
    };

    if let Some(program) = requested {
        let kind = kind_for(&program);
        return Some(ShellChoice { kind, program });
    }
    for (name, kind) in order {
        if let Some(program) = find_in_path(name, path) {
            return Some(ShellChoice { kind, program });
        }
    }
    for (name, kind) in order {
        for directory in ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            let program = Path::new(directory).join(name);
            if is_executable(&program) {
                return Some(ShellChoice { kind, program });
            }
        }
    }
    // 最后回退 POSIX sh：至少能执行基本命令，避免整个工具不可用。
    let program = PathBuf::from("/bin/sh");
    program.is_file().then_some(ShellChoice {
        kind: ShellKind::Sh,
        program,
    })
}

#[cfg(unix)]
fn kind_for(program: &Path) -> ShellKind {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("zsh") {
        ShellKind::Zsh
    } else if name.contains("bash") {
        ShellKind::Bash
    } else {
        ShellKind::Sh
    }
}

#[cfg(unix)]
fn is_bash_or_zsh(program: &Path) -> bool {
    program.file_name().is_some_and(|name| {
        let name = name.to_string_lossy().to_ascii_lowercase();
        name.contains("bash") || name.contains("zsh")
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(unix)]
fn find_in_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(windows)]
fn resolve_windows(path: Option<&OsStr>, git_bash: Option<&OsStr>) -> Option<ShellChoice> {
    // 1. 显式指定的 Git Bash（测试与高级用户的逃生口）。
    if let Some(program) = git_bash
        .map(PathBuf::from)
        .filter(|candidate| is_bash_program(candidate) && candidate.is_file())
    {
        return Some(ShellChoice {
            kind: ShellKind::GitBash,
            program,
        });
    }
    // 2. Git for Windows 的默认安装位置。
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(directory) = std::env::var_os(key) {
            let program = PathBuf::from(directory)
                .join("Git")
                .join("bin")
                .join("bash.exe");
            if program.is_file() {
                return Some(ShellChoice {
                    kind: ShellKind::GitBash,
                    program,
                });
            }
        }
    }
    // 3. 从 PATH 里的 git.exe 推导 <root>/bin/bash.exe。
    if let Some(git) = find_in_path_windows("git", path)
        && let Some(root) = git.parent().and_then(Path::parent)
    {
        let program = root.join("bin").join("bash.exe");
        if program.is_file() {
            return Some(ShellChoice {
                kind: ShellKind::GitBash,
                program,
            });
        }
    }
    // 4. PowerShell 7，其次 Windows PowerShell 5.1。
    if let Some(program) = find_in_path_windows("pwsh", path).or_else(|| {
        std::env::var_os("ProgramFiles").map(|directory| {
            PathBuf::from(directory)
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe")
        })
    }) && program.is_file()
    {
        return Some(ShellChoice {
            kind: ShellKind::PowerShell,
            program,
        });
    }
    if let Some(program) = find_in_path_windows("powershell", path).or_else(|| {
        std::env::var_os("SystemRoot").map(|directory| {
            PathBuf::from(directory)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
    }) && program.is_file()
    {
        return Some(ShellChoice {
            kind: ShellKind::PowerShell,
            program,
        });
    }
    // 5. cmd.exe 始终存在；COMSPEC 只是优先提供绝对路径。
    let program = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("cmd.exe"));
    Some(ShellChoice {
        kind: ShellKind::Cmd,
        program,
    })
}

#[cfg(windows)]
fn is_bash_program(program: &Path) -> bool {
    program.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().to_ascii_lowercase().as_str(),
            "bash.exe" | "sh.exe" | "bash" | "sh"
        )
    })
}

#[cfg(windows)]
fn find_in_path_windows(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for directory in std::env::split_paths(path) {
        for candidate in [
            directory.join(name),
            directory.join(format!("{name}.exe")),
            directory.join(format!("{name}.cmd")),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("写入假 shell");
        let mut permissions = std::fs::metadata(&path).expect("读取权限").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("设置执行位");
        path
    }

    #[test]
    fn fish_user_shell_falls_back_to_bash_on_path() {
        let directory = tempfile::tempdir().expect("临时目录");
        let bash = executable(directory.path(), "bash");
        let choice = resolve(
            Some(OsStr::new("/usr/bin/fish")),
            Some(directory.path().as_os_str()),
            None,
        )
        .expect("探测到 bash");
        assert_eq!(choice.kind, ShellKind::Bash);
        assert_eq!(choice.program, bash);
    }

    #[test]
    fn zsh_user_shell_wins_when_it_exists() {
        let directory = tempfile::tempdir().expect("临时目录");
        let zsh = executable(directory.path(), "zsh");
        let bash = executable(directory.path(), "bash");
        let choice = resolve(
            Some(zsh.as_os_str()),
            Some(directory.path().as_os_str()),
            None,
        )
        .expect("探测到 zsh");
        assert_eq!(choice.kind, ShellKind::Zsh);
        assert_eq!(choice.program, zsh);
        assert_ne!(choice.program, bash);
    }

    #[test]
    fn nonexistent_user_shell_is_ignored() {
        let directory = tempfile::tempdir().expect("临时目录");
        let bash = executable(directory.path(), "bash");
        let choice = resolve(
            Some(OsStr::new("/missing/zsh")),
            Some(directory.path().as_os_str()),
            None,
        )
        .expect("探测到 bash");
        assert_eq!(choice.kind, ShellKind::Bash);
        assert_eq!(choice.program, bash);
    }

    #[test]
    fn bash_arguments_disable_profile_and_rc() {
        let choice = ShellChoice {
            kind: ShellKind::Bash,
            program: "bash".into(),
        };
        let (program, args) = command_line(&choice, "echo hi");
        assert_eq!(program, PathBuf::from("bash"));
        assert_eq!(
            args,
            vec![
                OsString::from("--noprofile"),
                OsString::from("--norc"),
                OsString::from("-c"),
                OsString::from("echo hi"),
            ]
        );
    }

    #[test]
    fn zsh_arguments_skip_rc() {
        let choice = ShellChoice {
            kind: ShellKind::Zsh,
            program: "zsh".into(),
        };
        let (_, args) = command_line(&choice, "echo hi");
        assert_eq!(
            args,
            vec![
                OsString::from("-f"),
                OsString::from("-c"),
                OsString::from("echo hi"),
            ]
        );
    }

    #[test]
    fn powershell_wraps_utf8_and_exit_code() {
        let choice = ShellChoice {
            kind: ShellKind::PowerShell,
            program: "pwsh".into(),
        };
        let (_, args) = command_line(&choice, "Write-Output hi");
        assert_eq!(args[0], OsString::from("-NoLogo"));
        assert_eq!(args[1], OsString::from("-NoProfile"));
        assert_eq!(args[2], OsString::from("-NonInteractive"));
        assert_eq!(args[3], OsString::from("-Command"));
        let script = args[4].to_string_lossy();
        assert!(script.contains("[Console]::OutputEncoding"));
        assert!(script.contains("$LASTEXITCODE"));
        assert!(script.contains("Write-Output hi"));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn powershell_arguments_use_no_profile_and_command() {
        let choice = ShellChoice {
            kind: ShellKind::PowerShell,
            program: "pwsh.exe".into(),
        };
        let (_, args) = command_line(&choice, "Get-Date");
        assert_eq!(args[0], OsString::from("-NoLogo"));
        assert_eq!(args[1], OsString::from("-NoProfile"));
        assert_eq!(args[2], OsString::from("-NonInteractive"));
        assert_eq!(args[3], OsString::from("-Command"));
    }

    #[test]
    fn cmd_arguments_keep_utf8_prefix() {
        let choice = ShellChoice {
            kind: ShellKind::Cmd,
            program: "cmd.exe".into(),
        };
        let (_, args) = command_line(&choice, "echo hi");
        assert_eq!(args[0], OsString::from("/D"));
        assert_eq!(args[1], OsString::from("/S"));
        assert_eq!(args[2], OsString::from("/C"));
        assert!(args[3].to_string_lossy().contains("chcp 65001"));
    }
}
