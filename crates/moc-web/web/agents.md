# WebUI AI Elements 开发规范与准则 (AGENTS.md)

本项目 WebUI 严格采用 [shadcn AI Elements](https://ui.shadcn.com/docs/ai-elements) 与官方 shadcn/ui 组件库进行重构与开发。所有协作 Agent 及开发者必须严格遵守以下规则：

## 1. 核心铁律

1. **完全采用 AI Elements**：所有的 Agent 交互界面（对话流、消息、思考过程、工具调用、终端执行、计划与队列、权限审批、模型选择、提示输入、代码展示等）必须完全使用 `components/ai-elements/*` 以及 `components/ui/*`。
2. **尽量不手写 UI 样式**：严禁随意手写自定义 CSS、内联复杂样式或编写大量重复的非标准包装层。直接使用 AI Elements 和 shadcn/ui 提供的标准属性、插槽（Slots）与布局原语。
3. **特例与边界**：**除非 AI Elements 和 shadcn/ui 确实没有提供相关功能或组件**，才允许使用最少量的标准 Tailwind 组合必要逻辑，并在代码注释中说明原因。

---

## 2. 组件映射规范 (Component Mappings)

| 业务场景                    | 采用的标准组件 / AI Elements                                                                                                                                              | 说明                                            |
| :-------------------------- | :------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | :---------------------------------------------- |
| **对话流容器**              | `Conversation`, `ConversationContent`, `ConversationScrollButton`, `ConversationEmptyState` (`components/ai-elements/conversation.tsx`)                                   | 自动保持滚动边缘、支持平滑流式追随与空状态展示  |
| **消息轮次 (Turn)**         | `Message`, `MessageAvatar`, `MessageContent`, `MessageActions` (`components/ai-elements/message.tsx`)                                                                     | 区分 User 与 Assistant 角色，标准化对齐与头像   |
| **思考 / 链式推理**         | `Reasoning`, `ReasoningTrigger`, `ReasoningContent` (`components/ai-elements/reasoning.tsx`)                                                                              | 展示模型思考（thinking）、实时耗时与折叠展开    |
| **工具调用 (Tool Calls)**   | `Tool`, `ToolHeader`, `ToolContent`, `ToolInput`, `ToolOutput` (`components/ai-elements/tool.tsx`)                                                                        | 标准化工具调用折叠卡片，结构化呈现入参与出参    |
| **终端执行 / Bash**         | `Terminal`, `TerminalHeader`, `TerminalContent` (`components/ai-elements/terminal.tsx`)                                                                                   | 命令行运行与控制台流式输出                      |
| **计划与步骤**              | `Plan`, `PlanHeader`, `PlanContent`, `PlanStep` (`components/ai-elements/plan.tsx`)                                                                                       | Agent 执行计划与步骤推进                        |
| **待办与任务队列**          | `Queue`, `QueueItem`, `QueueItemTitle`, `QueueItemDescription` (`components/ai-elements/queue.tsx`), `Task`, `TaskHeader`, `TaskItem` (`components/ai-elements/task.tsx`) | 任务项状态追踪、已完成/剩余任务                 |
| **权限与确认 (HITL)**       | `Confirmation`, `ConfirmationTitle`, `ConfirmationDescription`, `ConfirmationActions` (`components/ai-elements/confirmation.tsx`)                                         | 工具调用确认、高危权限审批（Allow/Deny/Always） |
| **交互提问 (Ask Question)** | `Question`, `QuestionContent` (`components/ai-elements/question.tsx`) / `components/ui/radio-group`                                                                       | 针对用户选项与输入的交互式问答卡片              |
| **输入框 / Composer**       | `PromptInput`, `PromptInputTextarea`, `PromptInputActions`, `PromptInputButton`, `PromptInputTools` (`components/ai-elements/prompt-input.tsx`)                           | 自适应输入框、快捷操作按钮与工具入口            |
| **模型与思考选择器**        | `ModelSelector`, `ModelSelectorTrigger`, `ModelSelectorContent` (`components/ai-elements/model-selector.tsx`)                                                             | 模型切换与思考强度选择                          |
| **上下文用量指示**          | `Context`, `ContextTrigger`, `ContextContent` (`components/ai-elements/context.tsx`)                                                                                      | Token 用量比例与上下文窗口监控                  |
| **代码展示与 Diff**         | `CodeBlock`, `CodeBlockHeader`, `CodeBlockContent`, `CodeBlockCopyButton` (`components/ai-elements/code-block.tsx`)                                                       | 代码高亮、复制与差异呈现                        |
| **文件树与工作区**          | `FileTree`, `FileTreeFolder`, `FileTreeFile` (`components/ai-elements/file-tree.tsx`)                                                                                     | 工作区文件树展示与文件定位                      |
| **子代理 (Subagent)**       | `Subagent`, `SubagentHeader`, `SubagentContent` (`components/ai-elements/subagent.tsx`)                                                                                   | 子任务派发与嵌套执行追踪                        |
| **检查点与历史分支**        | `Checkpoint`, `CheckpointIcon`, `CheckpointTrigger` (`components/ai-elements/checkpoint.tsx`)                                                                             | 会话分叉、回退与版本锚点                        |
| **快捷提示 / 建议**         | `Suggestion`, `SuggestionList` (`components/ai-elements/suggestion.tsx`)                                                                                                  | 点击一键填充/发送的快捷引导词                   |

---

## 3. 设计原则

- **标准性优先**：一切组件优先从 `@/components/ai-elements/*` 与 `@/components/ui/*` 导入。
- **无状态呈现与数据解耦**：UI 组件专注于数据呈现与用户事件分发，底层通过 Zustand store 和 WebSocket RPC 驱动状态同步。
- **深浅色主题无缝融合**：遵循 shadcn CSS variables，所有 AI Elements 自动适配 Dark / Light 主题。
