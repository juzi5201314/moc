# moc × FrontierHarness 评测适配器

用 Harbor 的 `BaseInstalledAgent` 把 `moc --print` 接进 FrontierHarness 评测。
本目录只是接入层代码，**不包含任何已执行的评测结果**。

```
eval/frontierharness/
├── moc_agent.py          # Harbor InstalledAgent 适配器（-a moc_agent:MocAgent 加载）
├── run.sh                # 一条命令入口：run.sh <base_url> <模型id> <key> [api格式] [harbor参数]
├── config.example.toml   # 生成的 moc provider 配置形状说明 + 覆盖层示例
└── README.md
```

## 工作原理

- `install()`：把宿主机预编译的 Linux `moc` 二进制上传到任务环境
  `/installed-agent/moc/moc` 并软链到 `/usr/local/bin/moc`。
- `configure()`：在环境内生成 `/installed-agent/moc/config.toml`——一个名为
  `eval` 的 `openai_chat` provider，`api_key = "env:MOC_API_KEY"`（变量名可用
  `--ak api_key_env=` 改），`base_url` 与模型 id 由 `--ak`/`-m` 决定。
  **密钥不落盘**，只留环境变量引用。
- `run()`：任务说明写入 `instruction.txt`，执行
  `moc --print --hide-thinking --model eval/<model> --config ... --session-dir ... < instruction.txt`，
  输出 `tee` 到 agent 日志目录的 `moc.log`。`--print` 单次提示模式下 moc
  内部仍跑完整 agent loop（工具调用、压缩、plan 自动批准），跑完即退出。
- `populate_context_post_run()`：聚合会话 JSONL 里全部 `{"type":"usage"}`
  记录回填 `AgentContext` 的 input/cache/output token；首轮 cache read 与
  cache write 量另记入 `metadata`（供 first-cold 重定价用）。
  **不填 `cost_usd`**——moc 没有可靠的第三方供应商定价源，成本由 Harbor
  侧模型价目或评测脚本自行折算。

## 前置条件

```bash
# Harbor + Runta 环境（官方 v1.1 推荐路径，任务跑在 Runta 云 runtime 上）
uv tool install "harbor[runta]"
npm install -g @runta/runta-cli
runta login

# 预编译 Linux x86_64 moc 二进制（产物 target/release/moc）
# 注意 moc-web 的 build.rs 需要 pnpm + Node >= 22，Linux 还需要 nasm
cargo build --release
```

## 一条命令（推荐）

只要 base_url、模型 id、明文 key 三样（api 格式默认 `openai_chat`，可选
`openai_responses`/`anthropic_messages`），其余 harbor 参数原样追加：

```bash
eval/frontierharness/run.sh \
  https://api.fireworks.ai/inference/v1 \
  accounts/fireworks/models/kimi-k3 \
  sk-xxxx openai_chat \
  -i 'regex-log' --jobs-dir ./jobs
```

脚本自动处理 `PYTHONPATH`、参数透传，并先 `cd` 到仓库根（让默认的
`target/release/moc` 找得到）。换供应商就是换前三个参数。

### 只跑一部分 / 限制时长

harbor 的任务过滤与超时 flag（全部经 run.sh 透传）：

| flag | 作用 |
| --- | --- |
| `-i/--include-task-name <glob>` | 只跑匹配的任务（可重复，glob） |
| `-x/--exclude-task-name <glob>` | 排除匹配的任务（可重复） |
| `-l/--n-tasks N` | 过滤后最多跑 N 个任务 |
| `-n/--n-concurrent N` | 并发 trial 数 |
| `-k/--n-attempts N` | 每任务尝试次数 |
| `--agent-timeout-multiplier 0.3` | agent 阶段超时按任务声明值打折，用于硬限制总时长 |

例：`-i 'terminal-bench-4/*' -l 3 -n 3 --agent-timeout-multiplier 0.3`。
注意任务环境的 provisioning/镜像准备不算在 agent 超时里，第一次跑要额外
留几分钟。

**注意**：明文 key 走 `--ak api_key=` 会进入 shell 历史和 Harbor job 元数据；
介意的话改用环境变量（见下方手动命令），适配器会把宿主机 `MOC_API_KEY`
的值注入任务环境，容器内配置里仍只有 `env:MOC_API_KEY` 引用、不落盘。

## 手动命令（等价形式）

```bash
cd /path/to/moc
PYTHONPATH=$PWD/eval/frontierharness harbor run \
  -d frontier-harness-eval/frontier-harness-eval@v1.1.0 \
  -a moc_agent:MocAgent \
  -m fireworks_ai/accounts/fireworks/models/kimi-k3 \
  --ak moc_base_url=https://api.fireworks.ai/inference/v1 \
  -e runta --override-storage-mb 32768 -i 'regex-log' --jobs-dir ./jobs
```

当前 harbor 的 `-a/--agent` 直接接受 `module.path:ClassName`（旧的
`--agent-import-path` 已 deprecated）。

`-m` 按 LiteLLM 惯例写 `provider路由/上游模型id`；适配器取第一个 `/` 之后
的部分作为 moc 侧模型 id（上面的例子里是 `accounts/fireworks/models/kimi-k3`）。
`-m` 前缀只是路由标签，moc 不校验它；模型 id 也可以完全不走 `-m`，直接
`--ak model=<上游模型id>` 给定。

## agent 参数（`--ak key=value`）

| 参数 | 默认 | 说明 |
| --- | --- | --- |
| `binary` | `target/release/moc` | 宿主机上预编译二进制路径 |
| `moc_base_url` | `$MOC_BASE_URL` | provider 端点，**必填** |
| `api_key` | `$MOC_API_KEY` | 明文 key；缺省读宿主机 `api_key_env` 同名环境变量 |
| `api_format` | `openai_chat` | `openai_chat` / `openai_responses` / `anthropic_messages` |
| `api_key_env` | `MOC_API_KEY` | 容器内 moc 读 key 的环境变量名 |
| `model` | `-m` 去前缀 | 上游模型 id；`-m` 形式不满足时用此项直接给 |
| `thinking` | 无 | selector 追加思考等级，如 `high` → `eval/<model>:high` |
| `context_window` | `262144` | 模型上下文窗口 |
| `max_output_tokens` | `32768` | 单次响应输出上限 |
| `reasoning` | `false` | 模型表里的 `reasoning` 开关 |
| `moc_args` | 无 | 追加给 moc 命令行的参数，如 `--ak 'moc_args=--engine ptc'` |
| `workdir` | 环境默认 | moc 进程 cwd；缺省即任务工作区 |
| `config` | 无 | moc TOML 文件路径，作为第二个 `--config` 覆盖层传入 |

## 注意事项

- **任务环境隔离**：moc 默认 `permission = full`，工具不做沙盒；隔离靠
  Runta runtime 边界，评测任务本就如此设计。
- **联网**：任务 `network_mode` 由 Runta egress 策略执行；no-network 任务里
  moc 的 LLM 请求走不通属于预期——v1.1 的 62 个任务均为 Terminal-Bench
  系，按数据集声明放行对应出口即可。
- **分数口径**：自行 `harbor run` 的分数是自测口径；官方榜另有
  matched-control 流程（每任务 4 次计分尝试、同环境限额），要上榜需走
  官方流程。
- **成本字段**：`cost_usd` 由 Harbor/评测脚本按模型价目折算，适配器只提供
  token 计数；若任务失败也会尽力回填已发生的 usage。
- **二进制架构**：Runta runtime 是 Linux x86_64；在 macOS 上构建需交叉编译
  或在 Linux 容器/CI 里构建。
- **验证**：跑全量前先 `-l 1` 加 `-i <任务名>` 单任务 smoke，确认
  `jobs/<run>/.../agent/moc.log` 与 `moc-session.jsonl` 正常落盘、
  `agent_result` 里 token 字段非空。
