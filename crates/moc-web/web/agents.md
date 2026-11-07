# WebUI AI Elements 开发规范与准则 (AGENTS.md)

本项目 WebUI 严格采用 [shadcn AI Elements](https://ui.shadcn.com/docs/ai-elements) 与官方 shadcn/ui 组件库进行重构与开发。所有协作 Agent 及开发者必须严格遵守以下规则：

## 1. 核心铁律

1. **优先采用 AI Elements**：实现 Agent 交互界面（对话流、消息、权限审批、代码展示、工作流画布等）时优先使用 `components/ai-elements/*` 与 `components/ui/*`。组件按需 vendor——只保留有实际引用的拷贝；缺少所需组件时通过 shadcn CLI 添加对应文件，不全量引入整套组件库。
2. **尽量不手写 UI 样式**：严禁随意手写自定义 CSS、内联复杂样式或编写大量重复的非标准包装层。直接使用 AI Elements 和 shadcn/ui 提供的标准属性、插槽（Slots）与布局原语。
3. **特例与边界**：**除非 AI Elements 和 shadcn/ui 确实没有提供相关功能或组件**，才允许使用最少量的标准 Tailwind 组合必要逻辑，并在代码注释中说明原因。

---

## 2. 组件映射规范 (Component Mappings)

| 业务场景                    | 采用的标准组件 / AI Elements                                                                                                            | 说明                                            |
| :-------------------------- | :-------------------------------------------------------------------------------------------------------------------------------------- | :---------------------------------------------- |
| **对话流容器**              | `Conversation`, `ConversationContent`, `ConversationScrollButton`, `ConversationEmptyState` (`components/ai-elements/conversation.tsx`) | 自动保持滚动边缘、支持平滑流式追随与空状态展示  |
| **消息轮次 (Turn)**         | `Message`, `MessageContent`, `MessageActions`, `MessageBranch` (`components/ai-elements/message.tsx`)                                   | 区分 User 与 Assistant 角色，标准化对齐与头像   |
| **权限与确认 (HITL)**       | `Confirmation`, `ConfirmationTitle`, `ConfirmationRequest`, `ConfirmationActions` (`components/ai-elements/confirmation.tsx`)           | 工具调用确认、高危权限审批（Allow/Deny/Always） |
| **交互提问 (Ask Question)** | `Question`, `QuestionPrompt`, `QuestionOptions`, `QuestionOption` (`components/ai-elements/question.tsx`)                               | 针对用户选项与输入的交互式问答卡片              |
| **代码展示与 Diff**         | `CodeBlock`, `CodeBlockHeader`, `CodeBlockContent`, `CodeBlockCopyButton` (`components/ai-elements/code-block.tsx`)                     | 代码高亮、复制与差异呈现                        |
| **工作流画布**              | `Canvas`, `Node`, `Edge`, `Controls`, `Panel` (`components/ai-elements/{canvas,node,edge,controls,panel}.tsx`)                          | 工作流 DAG 图渲染与缩放平移                     |
| **附件展示**                | `Attachments`, `Attachment`, `AttachmentPreview` (`components/ai-elements/attachments.tsx`)                                             | 输入附件的缩略图与预览                          |
| **加载微光**                | `Shimmer` (`components/ai-elements/shimmer.tsx`)                                                                                        | 流式生成中的文本微光效果                        |

---

## 3. 设计原则

- **标准性优先**：一切组件优先从 `@/components/ai-elements/*` 与 `@/components/ui/*` 导入。
- **无状态呈现与数据解耦**：UI 组件专注于数据呈现与用户事件分发，底层通过 Zustand store 和 WebSocket RPC 驱动状态同步。
- **深浅色主题无缝融合**：遵循 shadcn CSS variables，所有 AI Elements 自动适配 Dark / Light 主题。
- **视觉语言**：配色、阴影、字号阶梯、组件外观与禁忌以 [`DESIGN.md`](./DESIGN.md) 为准，改界面前先读。
