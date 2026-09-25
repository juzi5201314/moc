# moc-core

`moc-core` 是编码代理内核：通过 `moc-llm` 调用上游模型，通过 `moc-protocol::AgentService` 向前端提供会话、轮次、取消、计划和压缩服务。`moc-cli` 是内嵌该内核的 REPL，不维护第二套代理循环。

## 配置与启动

全局配置为 `$MOC_HOME/config.toml`，默认 `~/.moc/config.toml`。会话还会合并其工作目录中的 `.moc/config.toml`，最后应用按顺序传入的 `--config` 覆盖层。表递归合并，标量与数组整体替换。

下面是配置结构示例，端点、模型和密钥环境变量必须替换为实际供应商设置：

```toml
[models.roles]
default = "provider/model"
smol = "provider/model"
slow = "provider/model"
plan = "provider/model"

[providers.provider]
api = "openai_chat"
base_url = "https://your-provider.example/v1"
api_key = "env:MOC_API_KEY"

[providers.provider.models.model]
context_window = 200000
max_output_tokens = 32000
input = ["text"]

[tools]
approval_mode = "prompt"
```

`api` 也支持 `openai_responses` 和 `anthropic_messages`；完整工具、上下文与模型兼容配置见 `src/config/defaults.toml`、`src/config/schema.rs` 和 `src/config/model.rs`。不要把供应商密钥提交到仓库。

供应商兼容项放在 `[providers.<provider>.models.<model>.compat]`：

- `supports_prompt_cache_key = false`：Chat/Responses 请求不发送缓存键及相关缓存选项，适用于拒绝扩展字段的上游；不关闭 Anthropic 的消息缓存断点。默认 `true`。
- `requires_reasoning_content_for_all_assistant_turns = true`：回传所有 assistant 思维链，并为缺失的历史补显式空块；不只针对工具调用。默认 `false`。
- `requires_reasoning_content_for_tool_calls` 与 `requires_assistant_content_for_tool_calls`：分别补齐工具调用历史中的推理字段和正文；显式空字符串不会被编码器转换成缺失字段。

供应商的 `connect_timeout_secs` 只限制 DNS/TCP/TLS 建连；`timeout_secs` 限制非流式整请求以及流式首包等待和后续空闲时间。默认分别为 20 秒和 600 秒，长时间思考不应被建连超时中断。

core 的供应商重试默认是最多 10 次尝试、初始退避 0.5 秒、最长退避 300 秒，并开启抖动。`retry` 表仅覆盖指定字段；连接池会保留客户端注入的选项并按建连超时复用。


```sh
cargo run -p moc-cli -- --models
cargo run -p moc-cli -- --cwd /path/to/workspace
cargo run -p moc-cli -- --print "解释当前项目的入口"
cargo run -p moc-cli -- --resume SESSION_ID
```

交互命令包括 `/mode`、`/plan`、`/approve execute|compact|new|reject`、`/compact`、`/undo`、`/new`、`/sessions`、`/model` 和 `/quit`。Ctrl+C 取消当前轮次；空闲时退出。`--print` 不输出流式通知，只打印最后一条 assistant 正文。

## 嵌入接口

- `Core::new(CoreConfig)` 加载配置、验证供应商并建立会话目录，返回 `Arc<Core>`。
- `CoreConfig` 包含 `home`、`cwd`、`overlays`、`session_dir` 与可注入的 `moc_llm::Client`。
- 将 `Core` 交给 `moc_protocol::Server`，通过协议客户端调用会话和轮次方法。
- 前端退出时调用 `Core::shutdown().await`，等待取消收尾并回收后台进程。
- `resolve_models` 提供配置中的模型列表，不进行网络探测。

## 行为与安全边界

- 默认授权模式是 `yolo`；上述示例显式使用 `prompt`。deny 规则始终生效，未声明确认能力的前端不能默许需确认的工具。
- ask/plan 不注册 edit/write，但 bash 不是操作系统沙箱。工作区检测会报告只读模式违规，不回滚用户可能同时进行的修改。
- hashline 编辑只能引用 read 实际展示的完整行。未服务、漂移或过期范围拒绝执行，并附当前锚点；重启后必须重新建立许可。
- 文件写入使用原子替换。撤销拒绝覆盖编辑之后的外部修改；跨文件 move 分别记录两个文件的撤销，需要两次 `/undo` 恢复两者。
- 取消会保存已经收到的 assistant 内容并补齐未执行调用的工具结果。后台任务继续运行，直至完成、被 kill、会话卸载或 core 退出。进程管理使用 Unix 进程组。
- 会话使用追加式 JSONL，逐条 flush；压缩与历史重写使用临时文件加 rename。它不是每条记录都 fsync 的事务数据库。
- 压缩依次尝试 remote、shake、soft；原生载荷只用于兼容模型，换模型或载荷失效时恢复日志视图。`auto_continue=false` 会在轮次内压缩完成后停止继续请求模型。

## 验证

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

测试使用固定本地上游，真实执行协议连接、文件工具、子进程、PTY 和 CLI。真实供应商验收需要自行提供配置和凭据。实现与工程改进记录位于仓库 `target/log.md`。