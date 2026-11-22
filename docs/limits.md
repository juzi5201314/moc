# 命令资源限制与后台任务

你可以为模型发起的 Shell 命令单独设置 CPU 和内存上限，避免大型编译或测试拖慢整台机器。资源限制与路径/网络沙盒相互独立：无论当前是否开启沙盒都能单独启用，默认关闭。

## 资源限制配置

在 `~/.moc/config.toml` 或项目 `.moc/config.toml` 中配置（也可在 `/config` 的「工具 → 命令执行 → 资源限制」中直接编辑）：

```toml
[tools.bash.limits]
enabled = false                # 总开关；关闭时零额外探测与开销
cpu_cores = 4                  # CPU 核数上限；0 表示不限
memory_high_bytes = 8589934592 # 8 GiB 软限：超出后降速节流，尽量不杀进程；0 表示不设
memory_max_bytes = 12884901888 # 12 GiB 硬限：兜底防爆内存；0 表示不设
memory_swap_bytes = 4294967296 # 4 GiB swap 预算；0 表示不限
```

开启 `enabled = true` 时至少需配置其中一项，且 `memory_high_bytes` 不能大于 `memory_max_bytes`。

## 各平台机制

### Linux（推荐）

Linux 下通过 `systemd-run --user --scope` 为每条命令创建独立 cgroup 作用域（每次调用约增加 25–30 ms 开销）：

- **CPU 配额**：`nproc` 与 Rust 的并发探测会自动识别配额，`cargo` 等构建工具会随之降低并行线程数，内存峰值也成比例回落。
- **内存软限（`memory_high_bytes`）**：超过后内核优先回收与限速，编译会变慢但尽量保持成功。需要系统开启 swap；若检测到宿主机无 swap（`SwapTotal = 0`），会自动跳过软限以免进程卡死，只保留硬限。
- **内存硬限与 swap 上限**：防止单条命令拖垮宿主机。若进程收到 `SIGKILL` 或返回 137，结果中会附带内存排查提示（具体原因仍需结合系统日志确认）。
- **独立配额**：并行运行的多个后台任务各自享有完整额度，互不挤压。

### Windows

通过系统 Job Object 限制 CPU 时间比例（`cpu_cores / 逻辑核数`）。该机制不会改变程序读取到的核数，因此 `cargo` 仍可能按全部核数启动线程；Windows 也缺乏“只降速不杀进程”的内存软限，`memory_*` 字段会被忽略（压内存建议传 `cargo -j`）。超过 64 核的机器不支持此限制，启用时会给出一次提示并按不限速运行。

### 其他平台

macOS 等平台暂不提供限流后端，启用时会提示一次并按原样执行。

### 为何不采用 `ulimit` / `taskset` / `cpulimit`

- `ulimit`（`RLIMIT_AS` / `RLIMIT_DATA`）属于硬性拒绝，触顶后分配直接失败，编译器容易立刻崩溃。
- `taskset` 只绑核不限速，多线程任务仍会把绑定核心跑满。
- `cpulimit` 靠外部采样频繁暂停/恢复进程，对大量短命编译子进程极不稳定。

## 限制边界

- 覆盖所有由模型发起的 Shell 命令（前台、异步、持久终端、PTC 与子代理）。
- 你亲手输入的 `!命令` 不受限制。
- 模型无法通过参数绕过或修改配额。
- 首次环境探测不可用（如容器内无 `systemd --user`）时会警告一次并放行；探测通过后若某次创建作用域失败，命令不会执行，并提示检查 `systemd --user` 或暂时关闭限制。

## 后台任务与并发规则

### 自动转后台与并发上限

- 前台 `bash` 命令运行超过 `tools.bash.foreground_after_secs`（默认 60 秒）会自动转入后台，并附带尾部输出立即返回，卡片标题显示 `· 后台 job_X`。
- 每个会话默认最多同时运行 `tools.job.max_running`（默认 8）个后台任务。名额占满时，新的异步命令会被拒绝，前台命令则继续留在前台直到完成或被取消，并在输出中说明原因。

### 增量读取、停滞检测与空闲唤醒

- 模型通过 `job` 工具读取日志时按已读位置增量返回，避免重复消耗上下文；界面查看任务日志不影响模型的读取进度。
- 后台任务超过 `tools.bash.stall_after_secs`（默认 120 秒）没有任何新输出时，会触发一次停滞提醒（同一段无输出只提醒一次，恢复输出后重新计时）。
- 后台任务结束时，如果当前会话空闲，会自动开启新一轮对话将结果交给模型（若此前是你主动取消了对话，则不会自动唤醒）。
- 随时可在界面输入 `/jobs` 查看任务状态、日志尾部或终止任务（TUI 中连按两次 `x` 确认终止）。

## Linux 排错命令

```bash
# 确认命令已落入独立 scope（输出中应包含 moc-bash-*.scope）
bash -c 'cat /proc/self/cgroup; nproc'

# 查看当前 cgroup 的内存硬限
cat /sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/memory.max

# 检查用户级 systemd 是否正常运行
systemctl --user is-system-running
systemd-run --user --scope --quiet -- true
```
