# 真实上下文压缩实验

实验使用 moc 的真实 Core、配置解析、上下文构建与完整压缩管线；模型由 `--model provider/model:level` 指定。它面向真实 OMP 会话的连续消息流，支持按目标上下文大小多次补充、压缩、录制和离线重放。

## 运行

需要 Python 3.11+、可用的 moc 供应商配置及 Rust 构建环境。当前 HTTP 网关支持 `openai_chat`。

```bash
cargo build -p moc-core --example compaction_lab
python3 scripts/compaction_lab.py \
  --source /absolute/path/to/session.jsonl \
  --model cpa-chat/deepseek-v4.1-flash:high \
  --window 1000000 --targets 400000,600000 --rounds 3 \
  --output /tmp/moc-compaction-run
```

输出目录必须不存在，权限设为 0700。原会话只读，模型凭据仅在代理进程内用于转发。临时配置使用本地网关占位凭据，产物不复制用户全局配置。

`--prepare-only` 只冻结和转换语料，不调用模型。要执行新的真实重复实验，重新运行上述命令并指定新输出目录。真实模型重新抽样不保证生成相同摘要，应比较多次结果分布。

## 实验语义

- 目标大小是 moc 的**完整模型上下文本地 token 估算**，包含指令和工具定义。ASCII 每四字节估算一个 token，非 ASCII 每字符一个；供应商返回的实际 usage 另记。
- 消息按有效父链选择，保持顺序；每轮追加尚未消费的完整消息组，跨组不重复。到达目标时可能因保留工具调用／结果组完整性稍有超出，实际数值必须查看该轮报告。
- 模型窗口按 `--window` 覆盖；每组压缩触发阈值按该组目标覆盖。压缩方法仍为 `moc`，`instructions=None`，保留默认近期保护和修剪参数。驱动显式发起 `session/compact`，不会启用自动续写。
- 完整管线先 shake；空间恢复充分时不进入模型摘要。这是正常实验结果，不能把它称为“模型摘要成功”。以 HTTP 请求、Usage 增量和阶段事件判断发生了什么，不能只看继承下来的方法名。
- soft 摘要本身将输出上限限制为 32,768 tokens。每次实际请求体会录制，以检查输入预算是否又丢弃了历史。
- 外部 MCP、技能和项目上下文文件在隔离环境中关闭，避免引入本机额外数据。语料中的原始工具调用只是历史数据，不重新执行历史任务。

## 真实语料与来源

转换器保留用户正文、助手正文、工具名称／参数与工具结果，排除思考、重复供应商响应字段和旧压缩摘要。OMP 曾修剪的结果若明确指向 `artifact://N (region R)`，从同名 sidecar 中的 `N.shake.log` 恢复；只读取被引用的区域，并冻结实际使用的归档文件。

语料清单记录源哈希、有效分支、转换计数、恢复情况和未能恢复的占位。**归档缺失不能归因于本次压缩**。原始会话与归档快照保存在实验目录内，可追溯每个消息组的原文行号和条目 ID。

## 观测与产物

每组保存隔离配置、驱动参数、运行日志、会话和 sidecar。每轮保存压缩前后上下文、压缩前后会话快照、阶段事件及结果。HTTP 目录逐次保存请求、响应原始字节、状态、哈希和耗时，包含失败重试与摘要修复请求。

根目录保存代码版本、实验源码快照、语料哈希和二进制哈希。`driver.log` 与 `http/events.jsonl` 在运行中持续追加，可用于观察正在进行的轮次和请求。

实验文件包含真实任务内容，应按原会话同等对待。凭据不会专门写入，但源会话本身若含敏感内容，快照同样会保留。

## 离线重放

```bash
python3 scripts/compaction_lab.py \
  --bin /tmp/moc-compaction-run/implementation/compaction_lab \
  --replay /tmp/moc-compaction-run \
  --output /tmp/moc-compaction-replay
```

重放不加载真实供应商凭据，也不连接上游。须使用原二进制；从相同规范化语料重新运行管线，逐次匹配请求后返回录制响应，检查所有录制请求均被消费。只允许明确记录的实验路径映射和非语义缓存键归一化；消息内容变化必须失败。录制文件哈希不匹配同样失败。重放耗时不代表真实模型耗时。

流式响应目前完整缓冲后转交，不测首 token 延迟。压缩摘要默认使用非流式请求。

若上游的非流式路径在长请求时超时，可显式加 `--stream-bridge`。它只把上游请求改为 SSE，再将完整流聚合为 moc 原本期待的 JSON；不改变压缩提示、模型或思考等级。此时会额外保存 `*.upstream-request.json` 与 `*.upstream-response.sse` 及哈希，清单标记 `stream_bridge=true`。原生非流式失败与桥接成功必须分别报告，不能把桥接成功称为原生传输通过。流没有 `[DONE]` 或结束原因时明确失败。

## 独立记忆探针

问题文件为 JSON，`prompt` 是发送给模型的题目；证据和参考答案可放在其他字段，不会随题目发送。

```bash
python3 scripts/compaction_lab_probe.py /tmp/moc-compaction-run /tmp/questions.json \
  --output /tmp/moc-compaction-direct --mode direct
python3 scripts/compaction_lab_probe.py /tmp/moc-compaction-run /tmp/questions.json \
  --output /tmp/moc-compaction-retrieval --mode retrieval \
  --select 400k/round-03,600k/round-03
```

`direct` 硬性禁用所有工具；`retrieval` 只开放 `read/grep/find`。两种探针均使用 `workspace_only` 权限、越界直接拒绝，仅额外开放该副本 transcript 与 compaction 归档，不能读取全量语料、其他轮次或参考答案文件。每个探针从独立会话及归档副本出发，使用正常 `SessionPrompt` 的上下文构建，避免旁路问答自己的预算裁剪。`--stage pre` 可在压缩前快照回答同样问题，帮助区分原本不会答与压缩后丢失；正常 Ask 模式提醒和工具白名单属于记录的评测条件。探针输出上限为8192，不改变主实验。

## 判断遗忘

先根据原文建立带出处的事实清单，再逐轮比较：

1. 必须随上下文保留的目标、禁止事项、用户修正、已决策方案、未完成工作和当前阻塞。
2. 可从文件／归档恢复的细节，例如旧日志数字、大段代码和已经完成的局部步骤。
3. 已失效的信息，例如被替代方案、旧状态和随后纠正的结论。

摘要没有某个字符串并不证明模型忘记；有字符串也不证明它理解正确。应结合真实模型闭卷回答、独立副本上的历史回查及原文人工核对，区分正确保留、压缩遗漏、被后文覆盖、恢复失败和无法判断。探针答案与题目不写回主压缩会话。

## 本地验证

```bash
python3 -m unittest discover -s scripts -p 'test_compaction_lab*.py' -v
cargo check -p moc-core --example compaction_lab
```

这些检查验证导入、工具组边界、分支、归档恢复、录制与重放，不调用真实模型。真实大上下文实验独立运行，不进入单元测试。
