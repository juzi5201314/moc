# moc

Rust 编写的终端 AI 编码助手（Cargo workspace，edition 2024，Rust 1.88+）。默认 TUI 前端，另有 Web（`moc web`）、ACP（`moc acp`）与行式降级；核心逻辑在 `moc-core`，每个前端进程内嵌一个 core。

## 命令

- 构建：`cargo build`（调试）/ `cargo build --release`（产物在 `target/release/moc`）
- 测试：`cargo test --workspace --locked`（CI 同款）；迭代期跑单 crate：`cargo test -p <crate>`
- 前端（`crates/moc-web/web/`）：`pnpm lint`（biome）/ `pnpm build`（tsc + vite）/ `pnpm test`（vitest）

## 环境要求

- `moc-web` 的 build.rs 会自动 `pnpm install && pnpm build` 前端，**cargo build/test 需要 pnpm + Node ≥22 在 PATH**
- Linux 构建需要 `nasm`（aws-lc-sys 汇编）
- `.cargo/config.toml` 已限制 `jobs = 6` 防链接峰值爆内存；全速构建用 `cargo --jobs N` 覆盖

## Workspace 结构

- `crates/moc-core` — Agent 内核：会话、提示调度、工具执行、循环保护
- `crates/moc-llm` — LLM 传输与编解码（OpenAI / Anthropic / 兼容网关）
- `crates/moc-protocol` — JSON-RPC 协议层与 schema
- `crates/moc-mcp` — MCP 客户端（rmcp）
- `crates/moc-plugin` / `moc-ptc-protocol` / `moc-ptc-worker` — 插件系统与 PTC（内嵌 QuickJS worker）
- `crates/moc-tui` / `moc-web` / `moc-acp` — 三个前端
- `crates/moc-cli` — 命令行入口与内嵌 PTC worker 入口
- `webui` — `crates/moc-web/web` 的 symlink
- `docs/` — 专题文档（acp、web、tui、plugins、workflow、compaction 等）
- `scripts/` — 压缩实验 Python 脚本（`test_*.py` 为 unittest：`cd scripts && python -m unittest test_compaction_lab`）

## 工作规则

- 中文优先：commit message、文档、代码注释一律中文
- Commit 风格：conventional commit `type(scope): 描述`，如 `fix(tui): …`、`feat(transcript): …`
- 改 `crates/moc-web/web/` 前必读 [`crates/moc-web/web/agents.md`](crates/moc-web/web/agents.md) 与 [`DESIGN.md`](crates/moc-web/web/DESIGN.md)

## 生成文件边界（不要手改）

- `crates/moc-web/web/src/protocol/types.gen.ts` — 由 schema 生成。流程：Rust 侧改 DTO → `MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol` → `cd crates/moc-web/web && pnpm gen:protocol`（`schema.test.ts` 校验新鲜度）
- `crates/moc-web/web/dist/` — 构建产物，已 gitignore

## 边界

- **Always**：行为变更要补/改测试；收尾跑 `cargo test --workspace --locked`
- **Ask first**：增删依赖、改 CI/release 流程、改协议 schema
- **Never**：提交密钥与本地配置（`~/.moc/`）；手改生成物；为绕过测试而精简测试

## Definition of done

- Rust 改动：`cargo test --workspace --locked` 通过（或明确说明只跑了哪些）
- 前端改动：`pnpm lint && pnpm build && pnpm test` 通过
- diff 不含无关改动；报告验证了什么、还有什么未知

## 维护本文件

命令、路径、crate 职责变化时，在同一 PR 内更新本文件；新 crate 加进 workspace 时补一行说明。
