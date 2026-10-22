# bash 资源限制

模型发起的每条 bash 可以各自套一层资源上限，避免 Rust 编译之类的任务吃光 CPU 和内存。
限制与文件/网络沙盒相互独立：`full` 模式下同样生效，`workspace` / `workspace-only` 下叠加在
沙盒之外。默认关闭。

## 配置

```toml
[tools.bash.limits]
enabled = false                # 总开关；关闭时完全不介入，无额外探测与延迟
cpu_cores = 4                  # CPU 核数上限；0 = 不限
memory_high_bytes = 8589934592 # 8 GiB 软限：超过就节流，慢但尽量不失败；0 = 不节流
memory_max_bytes = 12884901888 # 12 GiB 硬顶：兜底 OOM；0 = 不设
memory_swap_bytes = 4294967296 # 4 GiB swap 预算；0 = 不限制
```

字段说明同步出现在 `/config` 的「工具 → 命令执行 → 资源限制」，可直接在 TUI 里编辑。
`enabled = true` 时至少要配置一项，否则加载配置会报错；`memory_high_bytes` 不得超过
`memory_max_bytes`。

## 机制

### Linux

每条命令包装成一次 `systemd-run --user --scope`：

```
systemd-run --user --scope --quiet --collect --unit=moc-bash-<uuid> \
  -p CPUQuota=400% -p MemoryHigh=... -p MemoryMax=... -p MemorySwapMax=... \
  -- <shell> <args>
```

- `CPUQuota` 按核时间限速；`nproc` 与 Rust 的 `available_parallelism` 都会读取 cgroup 配额，
  因此 `cargo` 会自动把并发降到该核数，rustc 线程也随之减少，内存峰值大致按比例下降。
- `MemoryHigh` 是“慢但不杀”的关键：超过后内核开始回收与节流，编译会明显变慢但继续推进。
- `MemoryMax` 只是兜底，超过按 OOM 终止（退出码 137，结果里会给出提示）。
- `MemorySwapMax` 限制单条命令能占用的 swap，避免一个任务挤掉整机。
- `--scope` 把载荷原地放进新 scope：退出码、PTY、stdin/stdout、进程组回收都保持原有语义，
  只是多了一层 `systemd-run` 父子进程（超时或取消时整组一起杀）。
- 每条命令一个独立 scope，额度不共享：并发的后台任务各自都有完整的配置额度。
- 每次调用额外开销约 25–30 ms。

### Windows

使用现有 Job Object 的 CPU rate control：`CpuRate = cpu_cores / 逻辑核数`（占全部
处理器时间的比例），`ENABLE | HARD_CAP`。只限制 CPU 时间份额，**不会**改变
`GetSystemInfo`，所以 `cargo` 仍按全部核数开线程，内存峰值不会因此下降。Windows 没有
“节流不杀”的内存机制，`memory_*` 字段在 Windows 直接忽略；需要压内存请减少 `cargo -j`。
超过 64 个逻辑处理器的机器不支持 CPU rate control，此时会警告一次并按不限速执行。

### 其它平台

没有可用的限流后端：请求限制时警告一次，命令按原样执行。

## 为什么不用 ulimit / taskset / cpulimit

- `RLIMIT_AS` / `RLIMIT_DATA` 是硬上限，超了直接分配失败，rustc 会以内存不足退出。
- `taskset` 只限制亲和性，不限制 CPU 时间；多个编译线程仍能把绑定的核跑满，还会被钉死在固定核上。
- `cpulimit` 需要额外守护进程，按采样周期发送 SIGSTOP/SIGCONT，对短命的编译子进程不友好。

## swap 依赖

`MemoryHigh` 需要宿主有 swap 才能把匿名页换出并继续推进。实测在没有 swap 的机器上，
`MemoryHigh` 会让进程无限期停等（不是失败，而是永远不前进）。因此运行时检测到
`SwapTotal = 0` 时会自动跳过 `MemoryHigh`，只保留 `MemoryMax`，系统提示里的那一行也会
如实省略软限。

## 行为边界

- 只约束模型发起的 bash（包括 async、persistent、PTY、自动转后台、PTC 与子代理内的调用）。
- 用户手打的 `!命令` 不受限制：输入本身即授权。
- 模型无法通过工具参数调整或绕过限制。
- moc 被 SIGKILL 时 scope 可能残留，与现有孤儿进程行为一致；正常结束由 `--collect` 回收。
- 探测失败（没有 systemd user manager、容器内没有 `systemd-run`）时警告一次，命令照常执行。
- 探测成功后如果某次 scope 创建失败，命令不会执行，结果里会提示检查 `systemd --user` 或临时关闭限制。

## 排错

```bash
# 限制是否生效：应看到 moc-bash-*.scope
bash -c 'cat /proc/self/cgroup; nproc'

# 查看当前值
cat /sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/memory.max

# user manager 是否可用
systemctl --user is-system-running
systemd-run --user --scope --quiet -- true
```
