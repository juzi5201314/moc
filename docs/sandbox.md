# 沙盒与路径权限

moc 为会话提供三档权限模式：`full`、`workspace` 与 `workspace-only`。新会话默认使用 `full`（不限制路径与网络）；需要收紧权限时，可在对话中用 `/permission` 或在设置页切换。

CPU 与内存上限属于另一套独立机制，不随沙盒模式变化，详见 [limits.md](limits.md)。

> 你在输入框中亲手敲入的 `!` / `!!` 命令始终以 `full` 权限运行，不受当前会话沙盒限制；下表仅针对模型发起的工具调用。

## 三档模式对比

| 模式 | Shell 文件系统 | Shell 网络 | 内置文件工具（`read`/`edit`/`write`） |
|---|---|---|---|
| `full` | 宿主机全部目录 | 与宿主机相同 | 无路径限制 |
| `workspace` | 宿主机根目录只读；工作区、会话临时目录与额外授权路径可写；Linux 下 `$HOME` 为写时复制隔离层 | 由 `[sandbox].network` 控制 | 可读宿主机（除拒绝列表）；仅允许写入工作区与授权目录，越界时弹窗确认 |
| `workspace-only` | 仅挂载只读系统运行路径、工作区、会话临时目录与空白 `$HOME`；宿主机真实 `$HOME` 不可见 | 由 `[sandbox].network` 控制 | 仅允许读写工作区与授权目录；访问宿主机 `$HOME` 或其他目录时弹窗确认 |

`workspace-only` 模式除工作区外，仅保留三类必要例外：

1. **只读系统运行路径**：维持命令行程序正常运行所需的系统目录（如 Linux 的 `/usr`、`/bin`、`/lib*`、`/opt`、`/nix`、`/etc`、`/run/systemd/resolve`；macOS 的 `/System`、`/Library`、`/usr`、`/bin`、`/private/etc` 等）。
2. **会话临时目录**：独立的 `/tmp`、`/var/tmp`、`XDG_CACHE_HOME`（`workspace-only` 下还包括合成的 `$HOME`），对宿主机不可见，会话结束后清理。
3. **显式放行路径**：配置中的 `extra_read_paths` / `extra_write_paths` 以及你在本会话中临时批准的路径。

## 危险命令拦截

即使在 `full` 模式下，`[tools.bash].deny_patterns` 依然生效。默认规则会拦截两类高风险命令：

- 作用于根目录、家目录或顶层系统目录的递归删除与权限变更（如 `rm -rf /`、`find -delete`、`chmod -R` / `chown -R`）。
- 必然造成破坏的系统操作（如格式化磁盘、写块设备、改分区、覆写 `/etc` 关键文件、关机、杀掉全部进程）。

规则能识别 Shell 语法：像 `grep "rm -rf /"` 这类引号里的普通搜索文本不会被误拦；而嵌套在 `bash -c`、`eval`、`ssh` 或管道进 Shell 中的命令仍会被检查。

命中危险规则时，不会一概硬拒，而是弹出确认框让你选择「允许一次」「本会话始终允许该完整命令」或「拒绝」（若设为 `on_denied = "deny"` 或当前前端不支持弹窗，则直接拒绝）。批准后，命令仍受当前会话的沙盒模式约束。

## 配置说明

```toml
[sandbox]
default_mode = "full"          # full | workspace | workspace_only
network = true                 # 仅约束沙盒内的 Shell 命令
on_denied = "ask"              # ask（弹窗确认）| deny（直接拒绝）
allow_unsandboxed = true       # 是否允许模型申请临时升级到 full
overlay_home = true            # Linux：是否为 $HOME 启用写时复制隔离层
extra_read_paths = []          # 额外只读路径（加载时必须已存在）
extra_write_paths = []         # 额外可写路径（真实写盘，加载时必须已存在）
deny_read_paths = []           # 强制禁止读取（优先级最高）
deny_write_paths = []          # 强制禁止写入（优先级最高）
```

配置要点：

- 路径支持 `~`，展开后必须为绝对路径；`extra_write_paths` 自动包含读取权限，`deny_*` 永远优先于允许规则。
- `network = false` 只禁用沙盒内 Shell 子进程的网络，不影响 `web_fetch` 或 MCP 工具。
- `allow_unsandboxed = false` 会禁止模型申请以 `full` 无沙盒运行，但单个路径授权不受影响。
- `deny_read_paths` / `deny_write_paths` 不能直接覆盖整个会话工作区（工作区内的子路径可以单独屏蔽）。在 Linux 下，禁止写入的路径会以只读挂载，禁止读取的路径会用空白只读文件/目录遮蔽。
- 旧配置中已废弃 `[tools.bash].allow_patterns`；如果加载时提示未知字段，直接删掉该项即可。

## 权限升级与临时放行

所有非 `full` 模式的命令都会先在沙盒内执行。若因权限不足失败，模型可以附带理由申请一次更宽松的权限（例如从 `workspace-only` 升级到 `workspace`，或申请 `full`），届时弹窗供你选择：

- 允许一次；
- 本会话始终允许该命令；
- 本会话始终允许 `<前缀> *`（仅限简单命令，如 `cargo *`）；
- 拒绝。

路径越界同样提供「允许一次 / 本会话始终允许该路径 / 拒绝」。这些临时授权仅保存在当前会话内存中，切换权限模式或执行 `/permission clear` 即刻清空，且不会遗传给子代理。

> 批准升级到 `workspace` 只是切换到更宽的沙盒配置，仍然受沙盒隔离；只有 `full` 才在沙盒外执行。

## 包管理器缓存

无需手动维护 `~/.cargo`、`~/.npm` 等缓存白名单：

- **Linux `workspace`**：`$HOME` 默认以写时复制（overlayfs）挂载，包管理器可以正常写入缓存，改动落在会话临时目录，不污染宿主机 `$HOME`（关闭 `overlay_home` 后 `$HOME` 变为只读）。
- **`workspace-only`**：`$HOME` 指向会话空白目录，允许联网时包管理器可正常下载，但每次新会话需重新拉取。
- **macOS `workspace`**：系统不提供 overlayfs，真实 `$HOME` 为只读；如需持久化写入缓存，可将缓存目录加入 `extra_write_paths` 或切换为 `full`。

## 平台支持

- **Linux**：基于 `bwrap`（bubblewrap）隔离文件系统、命名空间与内核能力。
- **macOS**：基于 `/usr/bin/sandbox-exec`；`workspace-only` 下文件内容不可读，但部分目录名仍可能可见。
- **原生 Windows**：暂不提供操作系统级沙盒，`/permission` 仅提供 `full`；WSL2 自动走 Linux 沙盒。
- 沙盒程序或临时目录不可用时直接返回错误，绝不静默降级为无沙盒执行。

## 边界说明

- MCP 服务器、`web_fetch`、PTC 代码运行环境及配置/技能索引运行在主进程中，不受操作系统沙盒约束。
- `workspace` 模式下宿主机 Unix Socket（如 Docker、SSH Agent）仍可读；若需更强隔离，请使用 `workspace-only`（该模式不挂载宿主机 `/run` 与 `/var`，仅保留 DNS 解析与临时目录）。
- 在 `workspace-only` 下，若 `rustup` 等工具链安装在真实 `$HOME` 中，请改用 `workspace` 模式，或将工具链目录加入 `extra_read_paths` 并配置 `RUSTUP_HOME` / `CARGO_HOME`。该模式下 Shell 也无法直接读取会话内部的 `artifact://` 与 `local://` 文件，需通过 `read` 工具访问。
- 目前暂不支持域名级网络白名单、Landlock、Windows ACL、自动凭证遮罩与跨会话持久化授权。
