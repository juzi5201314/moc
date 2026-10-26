// 转写模型：把 transcript 条目与 session/update 折叠成可渲染的块序列。
// 全部是纯函数（配合 immer 的 draft 直接修改），不碰网络与 DOM，便于单测。

import type {
  AgentInfo,
  ContentPart,
  ContextUsageResult,
  FinishReason,
  ImageSource,
  Message,
  PlanInfo,
  SessionInfo,
  SessionLockInfo,
  SessionUpdate,
  TodoPhase,
  ToolDiff,
  ToolProgressRow,
  ToolStatus,
  TranscriptChildCall,
  TranscriptEntry,
  UpstreamFieldMismatch,
  Usage,
} from "@/protocol/types";

export type ToolRunState = ToolStatus | "running";

export type ToolBlock = {
  kind: "tool";
  id: string;
  toolCallId: string;
  name: string;
  arguments: string;
  status: ToolRunState;
  result?: string;
  isError: boolean;
  images: ImageSource[];
  durationMs?: number;
  diff?: ToolDiff;
  children: ToolBlock[];
  progress?: ToolProgressRow[];
  awaitingPermission: boolean;
  batchId?: string;
};

export type StatusTone = "info" | "warn" | "error";

export type Block =
  | {
      kind: "user";
      id: string;
      entryId?: string;
      text: string;
      images: ImageSource[];
      atMs: number;
    }
  | {
      kind: "bash";
      id: string;
      toolCallId?: string;
      command: string;
      output?: string;
      atMs: number;
    }
  | { kind: "agent-message"; id: string; from: string; body: string; atMs: number }
  | { kind: "reasoning"; id: string; text: string; startedAt?: number; durationMs?: number }
  | { kind: "text"; id: string; entryId?: string; text: string }
  | ToolBlock
  | {
      kind: "status";
      id: string;
      tone: StatusTone;
      text: string;
      retryable?: boolean;
    }
  | { kind: "plan"; id: string; planId: string; title: string; path: string }
  | {
      kind: "footer";
      id: string;
      model?: string;
      durationMs?: number;
      finishReason?: FinishReason;
      atMs: number;
    };

export type BlockKind = Block["kind"];

export type LiveTurn = {
  promptId: string;
  model: string;
  startedAt: number;
  /** 当前一轮生成的思考块与正文块 id；首个增量到达时才创建。 */
  reasoningId?: string;
  textId?: string;
  usage?: Usage;
  /** tool_call_id → batch_id，来自 tool_batch_started。 */
  batchOf: Record<string, string>;
};

export type ToolDiffRecord = { toolCallId: string; name: string; atMs: number; diff: ToolDiff };

export type SessionView = {
  info: SessionInfo;
  readOnly: boolean;
  holder: SessionLockInfo | null;
  blocks: Block[];
  hasMore: boolean;
  beforeEntryId?: string;
  loadingOlder: boolean;
  running: boolean;
  live: LiveTurn | null;
  todos: TodoPhase[];
  plans: PlanInfo[];
  agents: Record<string, AgentInfo>;
  diffs: ToolDiffRecord[];
  mismatch: { fields: UpstreamFieldMismatch[]; atMs: number } | null;
  pluginNotice: { plugin: string; code: string; message: string } | null;
  /** session/context 的最近一次结果；未读到为 null。 */
  context: ContextUsageResult | null;
  compactingId: string | null;
  seq: number;
};

export function createView(info: SessionInfo): SessionView {
  return {
    info,
    readOnly: false,
    holder: info.lock ?? null,
    blocks: [],
    hasMore: false,
    loadingOlder: false,
    running: false,
    live: null,
    todos: [],
    plans: [],
    agents: {},
    diffs: [],
    mismatch: null,
    pluginNotice: null,
    context: null,
    compactingId: null,
    seq: 0,
  };
}

// ---------- 内容辅助 ----------

export function messageText(message: Message): string {
  return message.content.flatMap((part) => ("Text" in part ? [part.Text] : [])).join("");
}

export function reasoningText(part: ContentPart): string {
  if (!("Reasoning" in part)) return "";
  const block = part.Reasoning;
  if ("Text" in block) return block.Text.text;
  return "";
}

function messageImages(message: Message): ImageSource[] {
  return message.content.flatMap((part) => ("Image" in part ? [part.Image.source] : []));
}

function newTool(
  id: string,
  toolCallId: string,
  name: string,
  args: string,
  batchId?: string,
): ToolBlock {
  return {
    kind: "tool",
    id,
    toolCallId,
    name,
    arguments: args,
    status: "running",
    isError: false,
    images: [],
    children: [],
    awaitingPermission: false,
    batchId,
  };
}

/** 在块序列（含子调用）里从后往前找工具块。 */
export function findTool(blocks: Block[], toolCallId: string): ToolBlock | undefined {
  for (let index = blocks.length - 1; index >= 0; index--) {
    const block = blocks[index];
    if (block.kind !== "tool") continue;
    if (block.toolCallId === toolCallId) return block;
    const child = findTool(block.children, toolCallId);
    if (child) return child;
  }
  return undefined;
}

// ---------- 历史 → 块 ----------

/**
 * 把 transcript 条目转成块序列。`idPrefix` 让同一会话多次分页的 id 不冲突。
 */
export function entriesToBlocks(
  entries: TranscriptEntry[],
  childCalls: TranscriptChildCall[] = [],
  idPrefix = "h",
): Block[] {
  const blocks: Block[] = [];
  const tools = new Map<string, ToolBlock>();
  let seq = 0;
  const nextId = () => `${idPrefix}${seq++}`;

  for (const entry of entries) {
    const message = entry.message;
    if (entry.bash) {
      blocks.push({
        kind: "bash",
        id: nextId(),
        command: entry.bash.command,
        output: messageText(message) || toolResultText(message),
        atMs: entry.at_ms,
      });
      continue;
    }
    if (entry.agent) {
      blocks.push({
        kind: "agent-message",
        id: nextId(),
        from: entry.agent.from_title,
        body: messageText(message),
        atMs: entry.at_ms,
      });
      continue;
    }
    switch (message.role) {
      case "User":
        blocks.push({
          kind: "user",
          id: nextId(),
          entryId: entry.id,
          text: messageText(message),
          images: messageImages(message),
          atMs: entry.at_ms,
        });
        break;
      case "Assistant": {
        let hasToolCall = false;
        let hasText = false;
        for (const part of message.content) {
          if ("Reasoning" in part) {
            const text = reasoningText(part);
            if (text) blocks.push({ kind: "reasoning", id: nextId(), text });
          } else if ("Text" in part) {
            if (!part.Text) continue;
            hasText = true;
            blocks.push({ kind: "text", id: nextId(), entryId: entry.id, text: part.Text });
          } else if ("ToolCall" in part) {
            hasToolCall = true;
            const call = part.ToolCall;
            const block = newTool(nextId(), call.id, call.name, call.arguments, entry.id);
            tools.set(call.id, block);
            blocks.push(block);
          }
        }
        if (hasText && !hasToolCall) {
          blocks.push({ kind: "footer", id: nextId(), atMs: entry.at_ms });
        }
        break;
      }
      case "Tool": {
        for (const part of message.content) {
          if (!("ToolResult" in part)) continue;
          const result = part.ToolResult;
          let block = tools.get(result.tool_call_id);
          if (!block) {
            block = newTool(nextId(), result.tool_call_id, "工具结果", "");
            blocks.push(block);
          }
          block.result = result.content;
          block.isError = result.is_error;
          block.images = result.images ?? [];
          block.status = entry.tool?.status ?? (result.is_error ? "error" : "completed");
          block.durationMs = entry.tool?.duration_ms;
        }
        break;
      }
      default:
        break;
    }
  }

  for (const child of childCalls) {
    const parent = tools.get(child.parent_tool_call_id);
    if (!parent) continue;
    const block = newTool(nextId(), child.id, child.name, child.arguments);
    block.result = child.result;
    block.status = child.status;
    block.isError = child.status === "error";
    block.durationMs = child.duration_ms;
    parent.children.push(block);
  }
  // 历史里还没有结果的调用（被中断）不应显示为运行中。
  for (const block of tools.values()) {
    if (block.status === "running") block.status = "cancelled";
  }
  return blocks;
}

function toolResultText(message: Message): string {
  return message.content
    .flatMap((part) => ("ToolResult" in part ? [part.ToolResult.content] : []))
    .join("");
}

/**
 * 直播阶段插入的用户块没有 entry id；提示结束后按最新一页的用户条目从后往前补上，
 * 回退功能依赖它。
 */
export function attachEntryIds(blocks: Block[], entries: TranscriptEntry[]): void {
  const users = entries.filter((entry) => entry.message.role === "User" && !entry.bash);
  let cursor = users.length - 1;
  for (let index = blocks.length - 1; index >= 0 && cursor >= 0; index--) {
    const block = blocks[index];
    if (block.kind !== "user") continue;
    if (block.entryId) {
      while (cursor >= 0 && users[cursor].id !== block.entryId) cursor--;
      cursor--;
      continue;
    }
    if (messageText(users[cursor].message) === block.text) {
      block.entryId = users[cursor].id;
      cursor--;
    }
  }
}

// ---------- 直播更新 ----------

function nextId(view: SessionView): string {
  view.seq += 1;
  return `l${view.seq}`;
}

function pushStatus(view: SessionView, tone: StatusTone, text: string, retryable?: boolean) {
  view.blocks.push({ kind: "status", id: nextId(view), tone, text, retryable });
}

function closeReasoning(view: SessionView, now: number) {
  const live = view.live;
  if (!live?.reasoningId) return;
  const block = view.blocks.find((item) => item.id === live.reasoningId);
  if (
    block?.kind === "reasoning" &&
    block.startedAt !== undefined &&
    block.durationMs === undefined
  ) {
    block.durationMs = now - block.startedAt;
  }
}

function ensureLive(view: SessionView, promptId: string, now: number): LiveTurn {
  if (!view.live || view.live.promptId !== promptId) {
    view.live = { promptId, model: view.info.model, startedAt: now, batchOf: {} };
    view.running = true;
  }
  return view.live;
}

/** 把一条 `session/update` 折进视图；未知类型保持不变。`now` 便于测试注入。 */
export function applyUpdate(view: SessionView, update: SessionUpdate, now = Date.now()): void {
  switch (update.type) {
    case "prompt_started":
      view.live = { promptId: update.prompt_id, model: update.model, startedAt: now, batchOf: {} };
      view.running = true;
      view.mismatch = null;
      break;
    case "generation_started": {
      closeReasoning(view, now);
      const live = ensureLive(view, update.prompt_id, now);
      live.reasoningId = undefined;
      live.textId = undefined;
      break;
    }
    case "assistant_delta": {
      const live = ensureLive(view, update.prompt_id, now);
      const delta = update.delta;
      if (delta.type === "usage") {
        live.usage = delta.usage;
      } else if (delta.type === "reasoning") {
        let block = live.reasoningId
          ? view.blocks.find((item) => item.id === live.reasoningId)
          : undefined;
        if (block?.kind !== "reasoning") {
          const id = nextId(view);
          view.blocks.push({ kind: "reasoning", id, text: "", startedAt: now });
          live.reasoningId = id;
          block = view.blocks.at(-1);
        }
        if (block?.kind === "reasoning") block.text += delta.delta;
      } else if (delta.type === "text") {
        closeReasoning(view, now);
        let block = live.textId ? view.blocks.find((item) => item.id === live.textId) : undefined;
        if (block?.kind !== "text") {
          const id = nextId(view);
          view.blocks.push({ kind: "text", id, text: "" });
          live.textId = id;
          block = view.blocks.at(-1);
        }
        if (block?.kind === "text") block.text += delta.delta;
      }
      break;
    }
    case "tool_batch_started": {
      const live = ensureLive(view, update.prompt_id, now);
      for (const call of update.calls) live.batchOf[call.tool_call_id] = update.batch_id;
      break;
    }
    case "tool_started": {
      closeReasoning(view, now);
      if (findTool(view.blocks, update.tool_call_id)) break;
      const block = newTool(
        nextId(view),
        update.tool_call_id,
        update.name,
        update.arguments,
        view.live?.batchOf[update.tool_call_id],
      );
      const parent = update.parent_tool_call_id
        ? findTool(view.blocks, update.parent_tool_call_id)
        : undefined;
      if (parent) parent.children.push(block);
      else view.blocks.push(block);
      break;
    }
    case "tool_finished": {
      let block = findTool(view.blocks, update.tool_call_id);
      if (!block) {
        block = newTool(nextId(view), update.tool_call_id, update.name, update.arguments);
        const parent = update.parent_tool_call_id
          ? findTool(view.blocks, update.parent_tool_call_id)
          : undefined;
        if (parent) parent.children.push(block);
        else view.blocks.push(block);
      }
      block.status = update.status;
      block.result = update.result.content;
      block.isError = update.result.is_error;
      block.images = update.result.images ?? [];
      block.durationMs = update.duration_ms;
      block.awaitingPermission = false;
      if (update.diff) {
        block.diff = update.diff;
        view.diffs.push({
          toolCallId: update.tool_call_id,
          name: update.name,
          atMs: now,
          diff: update.diff,
        });
      }
      break;
    }
    case "bash_started":
      view.blocks.push({
        kind: "bash",
        id: nextId(view),
        toolCallId: update.tool_call_id,
        command: update.command.command,
        atMs: now,
      });
      break;
    case "tool_progress": {
      const block = findTool(view.blocks, update.tool_call_id);
      if (block) block.progress = update.rows;
      break;
    }
    case "permission_requested": {
      const block = findTool(view.blocks, update.tool_call_id);
      if (block) block.awaitingPermission = true;
      break;
    }
    case "permission_resolved":
      for (const block of view.blocks) {
        if (block.kind === "tool" && block.awaitingPermission) block.awaitingPermission = false;
      }
      break;
    case "prompt_finished": {
      closeReasoning(view, now);
      const live = view.live;
      // 最后一轮的正文与思考以权威 message 为准。
      const text = messageText(update.message);
      const reasoning = update.message.content.map(reasoningText).join("");
      if (live) {
        const textBlock = live.textId
          ? view.blocks.find((item) => item.id === live.textId)
          : undefined;
        if (textBlock?.kind === "text") textBlock.text = text;
        else if (text) view.blocks.push({ kind: "text", id: nextId(view), text });
        const reasoningBlock = live.reasoningId
          ? view.blocks.find((item) => item.id === live.reasoningId)
          : undefined;
        if (reasoningBlock?.kind === "reasoning" && reasoning) reasoningBlock.text = reasoning;
      } else if (text) {
        view.blocks.push({ kind: "text", id: nextId(view), text });
      }
      // 还挂着的工具调用随提示一起收尾。
      for (const block of view.blocks) {
        if (block.kind === "tool" && block.status === "running") block.status = "cancelled";
      }
      view.blocks.push({
        kind: "footer",
        id: nextId(view),
        model: live?.model,
        durationMs: live ? now - live.startedAt : undefined,
        finishReason: update.finish_reason,
        atMs: now,
      });
      view.live = null;
      view.running = false;
      break;
    }
    case "guard_notice":
      pushStatus(view, update.terminal ? "error" : "warn", update.message);
      break;
    case "error":
      pushStatus(view, "error", update.message, update.retryable);
      break;
    case "plugin_notice":
      view.pluginNotice = { plugin: update.plugin, code: update.code, message: update.message };
      pushStatus(view, "warn", `插件 ${update.plugin} 已停用（${update.code}）：${update.message}`);
      break;
    case "title_changed":
      view.info.title = update.title;
      break;
    case "lock_changed":
      view.holder = update.lock;
      break;
    case "permission_changed":
      view.info.permission = update.permission;
      break;
    case "mode_changed":
      view.info.mode = update.mode;
      break;
    case "model_changed":
      view.info.model = update.model;
      break;
    case "todo_changed":
      view.todos = update.phases;
      break;
    case "todo_reminder":
      pushStatus(view, "info", `提醒：还有 ${update.open_items} 项待办未完成`);
      break;
    case "plan_submitted": {
      const plan: PlanInfo = {
        plan_id: update.plan_id,
        title: update.title,
        path: update.path,
        markdown: update.markdown,
        status: "submitted",
      };
      const index = view.plans.findIndex((item) => item.plan_id === plan.plan_id);
      if (index >= 0) view.plans[index] = plan;
      else view.plans.push(plan);
      view.blocks.push({
        kind: "plan",
        id: nextId(view),
        planId: plan.plan_id,
        title: plan.title,
        path: plan.path,
      });
      break;
    }
    case "context_compaction_started": {
      const id = nextId(view);
      view.blocks.push({ kind: "status", id, tone: "info", text: "正在压缩上下文…" });
      view.compactingId = id;
      break;
    }
    case "context_compacted":
    case "context_compaction_failed": {
      const text =
        update.type === "context_compacted"
          ? `上下文已压缩（${update.method}）：${update.tokens_before} → ${update.tokens_after} tokens`
          : `上下文压缩失败：${update.message}`;
      const tone: StatusTone = update.type === "context_compacted" ? "info" : "error";
      const block = view.blocks.find((item) => item.id === view.compactingId);
      if (block?.kind === "status") {
        block.text = text;
        block.tone = tone;
      } else {
        pushStatus(view, tone, text);
      }
      view.compactingId = null;
      break;
    }
    case "job_finished":
      pushStatus(
        view,
        update.exit_code === 0 ? "info" : "warn",
        `后台任务结束：${update.command}（退出码 ${update.exit_code ?? "未知"}）`,
      );
      break;
    case "workspace_changed":
      if (update.violation) {
        pushStatus(
          view,
          "warn",
          `工作区外的变更：${update.changes.map((change) => change.path).join("、")}`,
        );
      }
      break;
    case "upstream_mismatch":
      view.mismatch = { fields: update.fields, atMs: now };
      break;
    case "steering_applied":
      view.blocks.push({
        kind: "user",
        id: nextId(view),
        text: update.text,
        images: [],
        atMs: now,
      });
      break;
    case "agent_started":
      view.agents[update.agent.id] = update.agent;
      break;
    case "agent_status_changed": {
      const agent = view.agents[update.agent_id];
      if (agent) {
        agent.status = update.status;
        if (update.last_message !== undefined) agent.last_message = update.last_message;
        agent.updated_at_ms = now;
      }
      break;
    }
    case "agent_message":
      view.blocks.push({
        kind: "agent-message",
        id: nextId(view),
        from: update.meta.from_title,
        body: update.body,
        atMs: now,
      });
      break;
    default:
      break;
  }
}

/** 断线时收尾：core 已经随连接取消了提示。 */
export function interruptLive(view: SessionView): void {
  if (!view.live && !view.running) return;
  for (const block of view.blocks) {
    if (block.kind === "tool" && block.status === "running") block.status = "cancelled";
  }
  view.live = null;
  view.running = false;
  pushStatus(view, "warn", "连接中断，本次生成已停止");
}

export function appendLocalUser(view: SessionView, text: string, images: ImageSource[]): void {
  view.blocks.push({ kind: "user", id: nextId(view), text, images, atMs: Date.now() });
}

export function appendLocalStatus(view: SessionView, tone: StatusTone, text: string): void {
  pushStatus(view, tone, text);
}

export function finishLabel(reason: FinishReason | undefined): string {
  if (!reason) return "";
  if (typeof reason === "object") return reason.Other;
  const labels: Record<string, string> = {
    Stop: "",
    StopSequence: "",
    ToolCalls: "",
    Length: "截断",
    Cancelled: "已取消",
    Interrupted: "已中断",
    LoopDetected: "循环中止",
    ContentFilter: "被拦截",
    Refusal: "已拒绝",
    PauseTurn: "暂停",
  };
  return labels[reason] ?? reason;
}
