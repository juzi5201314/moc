# 沙盒与路径权限

MOC 的会话权限分为 `full`、`workspace`、`workspace-only` 三档。默认是新会话使用
`full`，保持既有 yolo 行为；需要隔离时用 `/permission` 或设置页切换。

## 模式矩阵

| 模式 | bash 文件系统 | bash 网络 | 内置文件工具 |
|---|---|---|---|
| `full` | 宿主全部 | 宿主全部 | 无边界 |
| `workspace` | 宿主 `/` 只读；工作区、会话 scratch、额外写路径可写；`$HOME` 为会话 COW 层（Linux） | 由 `[sandbox].network` 决定 | 读宿主（除 deny）；写允许根，越界申请 |
| `workspace-only` | 只读运行时系统路径 + 工作区 + scratch + 合成 HOME；宿主 HOME 不存在 | 由 `[sandbox].network` 决定 | 读允许根；宿主 HOME 越界申请 |

`workspace-only` 的“工作区外完全禁止”有三个必要例外：

1. 运行程序所需的只读系统路径（Linux：`/usr`、`/bin`、`/sbin`、`/lib*`、`/opt`、
   `/nix`、`/etc`、`/run/systemd/resolve`；macOS：`/System`、`/Library`、`/usr`、
   `/bin`、`/sbin`、`/private/etc` 等）。
2. 会话 scratch：`/tmp`、`/var/tmp`、`XDG_CACHE_HOME`，workspace-only 下还包括合成
   `$HOME`。宿主不可见，会话结束删除。
3. 用户显式配置的 `extra_read_paths` / `extra_write_paths` 与会话授权。

`deny_read_paths` / `deny_write_paths` 优先于所有允许规则；`full` 不执行路径与网络边界，
但 `[tools.bash].deny_patterns` 仍然生效。

## 配置

```toml
[sandbox]
default_mode = "full"          # full | workspace | workspace_only
network = true                 # 仅约束沙盒内 bash
on_denied = "ask"              # ask | deny
allow_unsandboxed = true       # 是否允许模型申请 full
overlay_home = true            # Linux：$HOME 的会话 COW 层
extra_read_paths = []          # 只读扩权，必须存在
extra_write_paths = []         # 读写扩权，真实落盘，必须存在
deny_read_paths = []           # 强制拒绝读取，优先于允许
deny_write_paths = []          # 强制拒绝写入，优先于允许
```

- 路径支持 `~`，展开后必须是绝对路径；`extra_*` 在加载时必须存在。
- `extra_write_paths` 隐含可读；`deny_*` 优先。
- `network=false` 不影响 `web_fetch`、`web_search`、MCP，只影响 bash 子进程。
- `allow_unsandboxed=false` 时模型不能申请 `full`，但路径授权仍可用。
- `on_denied="deny"` 时越界直接返回 `E_SANDBOX_DENIED`，不弹权限窗。
- `deny_read_paths` / `deny_write_paths` 不能等于或包含会话工作区，否则配置加载报错；
  工作区内部的子路径可以单独遮蔽。指向不存在路径时，内置文件工具仍按词法拒绝，但 bash
  沙盒无法预先挂载遮蔽。
- Linux 下 `deny_write_paths` 把原路径只读重挂（读内容保留、写入 `EROFS`）；
  `deny_read_paths` 用只读空文件或只读 tmpfs 遮蔽内容。
- `[tools.bash].allow_patterns` 已移除：旧配置会因未知字段报错，删除该字段即可；所有
  非 full 命令都先进沙盒，不再有“批准后以 full 执行”的旁路。

## 权限与升级

所有非 full 的 bash 命令先进入沙盒。命令失败且 stderr 命中拒绝签名时，结果会提示模型
重试一次，并带：

```json
{
  "sandbox_permissions": "workspace",
  "justification": "cargo needs to write ~/.cargo"
}
```

`sandbox_permissions` 只能是严格更宽的模式（`workspace-only < workspace < full`），
且必须与 `justification` 同时出现。用户可：

- 允许一次；
- 本会话始终允许该命令；
- 本会话始终允许 `<prefix> *`（仅简单命令提供，如 `cargo *`）；
- 拒绝。

路径越界弹窗提供“允许一次 / 本会话始终允许该路径 / 拒绝”。会话授权只存在内存中，
切换模式或 `/permission clear` 即清空；子代理不继承父会话授权。

只有 `full` 是沙盒外执行；批准 `workspace` 只是重建更宽的沙盒 profile，不会跳过隔离。

## 包管理器缓存

不维护 `~/.cargo`、`~/.npm`、`~/.cache` 等清单：

- Linux `workspace`：`$HOME` 以 `overlayfs` 的 COW 方式挂载；任何工具写 `$HOME` 都成功，
  写入落在会话 scratch，宿主 HOME 不变。`overlay_home=false` 时 HOME 只读。
- `workspace-only`：`$HOME` 指向会话合成目录，包管理器可正常写缓存并联网下载，但每次
  会话重新下载，宿主不受影响。
- 需要持久化某个缓存：加入 `extra_write_paths`（真实写）或切到 `full`。
- macOS 没有 overlayfs：`workspace` 下真实 HOME 只读，需要 `extra_write_paths` 或
  `full`；`workspace-only` 使用合成 HOME，可工作但重新下载。

## 平台

- Linux：`bwrap`，`--ro-bind / /` / 最小根、`--overlay`、`--unshare-*`、`--cap-drop ALL`。
- macOS：`/usr/bin/sandbox-exec` + SBPL；`workspace-only` 的读限制只到
  `file-read-data`，目录元数据仍可见；`sandbox-exec` 已弃用。
- 原生 Windows：不提供 OS 沙盒，`/permission` 只显示 `full`；WSL2 走 Linux 实现。
- runner 或 scratch 不可用时返回 `E_SANDBOX_UNAVAILABLE`，不会静默无沙盒执行。

## 明确不覆盖

- MCP 服务、OpenViking、`web_fetch`、`web_search`、PTC worker，以及 core 自身的配置、
  `context_files`、skill 索引在 core 进程运行，不受 OS 沙盒约束。
- `workspace` 可读宿主 Unix socket（如 Docker/SSH agent）；`workspace-only` 不挂载
  宿主 `/run`（仅 `/run/systemd/resolve`）和 `/var`（仅 scratch `/var/tmp`），隔离更强。
- 内置文件工具的路径检查是进程内策略围栏，存在解析到写入的 TOCTOU 窗口；执行前复校验
  已缩小，未消除。
- `workspace-only` 的合成 HOME 不含真实用户 dotfiles/工具链；rustup 安装在真实 HOME
  时，用 `workspace`，或把工具链目录加入 `extra_read_paths` 并用 `RUSTUP_HOME` /
  `CARGO_HOME` 指向真实路径。该模式下 bash 也不能读取会话 `artifact://` / `local://`
  文件，改用 `read` 工具。
- 无域名白名单代理、无 Landlock 降级、无 Windows ACL、无凭证 mask、无持久化会话授权。
