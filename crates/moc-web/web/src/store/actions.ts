// 跨组件共享的会话动作：发送提示、续跑（session/retry）、手动 Bash、计划审批，以及提示结束后的续发。
//
// core 在 session/prompt 的 RPC 返回前才释放提示锁，prompt_finished 推送时锁可能还没放；
// 所以本端发起的提示一律等 RPC 返回再续发队列或自动批准计划，否则会撞上 SESSION_BUSY。

import { toast } from "sonner";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type {
  BashResult,
  InputPart,
  PlanAction,
  PromptMode,
  PromptResult,
  SessionEngine,
  SessionPlanApproveResult,
} from "@/protocol/types";
import { toInput, useComposer } from "./composer";
import {
  newSession,
  openSession,
  setSessionAutonomy,
  setSessionPermission,
  syncEntryIds,
} from "./lifecycle";
import { useSessions } from "./sessions";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";
import { dispatchPaused, useUi } from "./ui";

export type SendOptions = { mode?: PromptMode; echo?: boolean };

function errorText(error: unknown): string {
  return error instanceof RpcError || error instanceof Error ? error.message : String(error);
}

/** 本端发起、RPC 尚未返回的提示所在会话。 */
const ownPrompts = new Set<string>();
/** unattended 下本端提示期间提交的计划：会话 → plan_id，等提示 RPC 返回后自动批准。 */
const autoApprovals = new Map<string, string>();

/** 该会话是否有本端发起、尚未返回的提示。 */
export function hasOwnPrompt(sessionId: string): boolean {
  return ownPrompts.has(sessionId);
}

/** 记下本端提示期间提交的计划，提示结束后按 unattended 自动批准「压缩并执行」。 */
export function deferAutoApproval(sessionId: string, planId: string): void {
  autoApprovals.set(sessionId, planId);
}

/** 发起本端提示；RPC 返回（锁已释放）后先处理待自动批准的计划，否则续发队列。 */
async function ownPrompt(sessionId: string, call: () => Promise<unknown>): Promise<void> {
  ownPrompts.add(sessionId);
  try {
    await call();
  } finally {
    ownPrompts.delete(sessionId);
    const planId = autoApprovals.get(sessionId);
    autoApprovals.delete(sessionId);
    if (planId) void autoApprove(sessionId, planId);
    else dispatchNext(sessionId);
  }
}

/** 与 TUI 的临时提示对应：成功只弹 toast，失败才写进转写，用户需要手动审阅。 */
async function autoApprove(sessionId: string, planId: string): Promise<void> {
  try {
    await approvePlan(sessionId, planId, "execute_compacted");
    toast("无人值守：已自动批准计划", { description: "压缩上下文后开始执行" });
  } catch (error) {
    useTranscript
      .getState()
      .localStatus(sessionId, "error", `计划自动批准失败：${errorText(error)}`);
    dispatchNext(sessionId);
  }
}

/** 会话空闲、没有本端提示在跑且未暂停时，发出队首的队列项。 */
export function dispatchNext(sessionId: string): void {
  if (dispatchPaused(useUi.getState()) || ownPrompts.has(sessionId)) return;
  const view = useTranscript.getState().bySession[sessionId];
  if (!view || view.running || view.readOnly) return;
  const next = (useComposer.getState().pending[sessionId] ?? []).find(
    (item) => item.kind === "queue",
  );
  if (!next) return;
  useComposer.getState().removePending(sessionId, next.key);
  void sendPrompt(sessionId, toInput(next.text, next.images), { mode: next.mode });
}

/** 草稿会话首次发送时向 core 创建真实会话，返回可用的会话 id。 */
async function materialize(sessionId: string): Promise<string> {
  if (sessionId !== "draft-session") return sessionId;
  const draft = useSessions.getState().draftSession;
  const info = await newSession({
    cwd: draft?.cwd,
    model: draft?.model,
    engine: draft?.engine,
    plan: draft?.plan,
  });
  // session/new 不带权限与自主程度参数：草稿上改过的才在创建后补设。
  const tasks: Promise<void>[] = [];
  if (draft?.permission && draft.permission !== info.permission) {
    tasks.push(setSessionPermission(info.id, draft.permission));
  }
  if (draft && draft.autonomy !== info.autonomy) {
    tasks.push(setSessionAutonomy(info.id, draft.autonomy));
  }
  for (const result of await Promise.allSettled(tasks)) {
    if (result.status === "rejected") {
      useTranscript
        .getState()
        .localStatus(info.id, "warn", `会话选项未能应用：${errorText(result.reason)}`);
    }
  }
  useSessions.getState().setDraftSession(null);
  useTabs.getState().replace("draft-session", info.id);
  useTranscript.getState().drop("draft-session");
  return info.id;
}

async function afterPrompt(sessionId: string): Promise<void> {
  void syncEntryIds(sessionId).catch(() => undefined);
  void useSessions
    .getState()
    .refresh()
    .catch(() => undefined);
}

/**
 * 发送一条提示。本地先插入用户块（`echo` 默认 true），流式内容走 session/update；
 * 返回时提示已结束，再补 entry id 并刷新侧栏。
 */
export async function sendPrompt(
  sessionId: string,
  input: InputPart[],
  options: SendOptions = {},
): Promise<void> {
  if (input.length === 0) return;
  let target: string;
  try {
    target = await materialize(sessionId);
  } catch (error) {
    useTranscript.getState().localStatus(sessionId, "error", `创建会话失败：${errorText(error)}`);
    return;
  }

  if (options.echo ?? true) {
    const text = input.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("\n");
    const images = input.flatMap((part) => (part.type === "image" ? [part.source] : []));
    useTranscript.getState().localUser(target, text, images);
  }
  await ownPrompt(target, async () => {
    try {
      await rpc.call<PromptResult>("session/prompt", {
        session_id: target,
        input,
        ...(options.mode ? { mode: options.mode } : {}),
      });
    } catch (error) {
      useTranscript.getState().localStatus(target, "error", `发送失败：${errorText(error)}`);
      return;
    }
    await afterPrompt(target);
  });
}

/** 接着现有上下文继续跑：不追加用户消息，上一轮没跑完时 core 会先剪掉半截回合。 */
export async function retrySession(sessionId: string): Promise<void> {
  await ownPrompt(sessionId, async () => {
    try {
      await rpc.call<PromptResult>("session/retry", { session_id: sessionId });
    } catch (error) {
      useTranscript.getState().localStatus(sessionId, "error", `重试失败：${errorText(error)}`);
      return;
    }
    await afterPrompt(sessionId);
  });
}

/**
 * 手动执行 Bash：`!cmd` 写入 agent 上下文，`!!cmd` 只在界面显示。
 * 输出由 bash_started / tool_finished 事件落进转写。
 */
export async function runBash(
  sessionId: string,
  command: string,
  excludeFromContext: boolean,
): Promise<void> {
  let target: string;
  try {
    target = await materialize(sessionId);
  } catch (error) {
    useTranscript.getState().localStatus(sessionId, "error", `创建会话失败：${errorText(error)}`);
    return;
  }
  try {
    await rpc.call<BashResult>("session/bash", {
      session_id: target,
      command: { command, exclude_from_context: excludeFromContext },
    });
  } catch (error) {
    useTranscript.getState().localStatus(target, "error", `命令执行失败：${errorText(error)}`);
  }
  void useSessions
    .getState()
    .refresh()
    .catch(() => undefined);
}

/**
 * 审批计划：`execute_new_session` 切到新会话，core 给出 `auto_prompt` 时立即发出执行提示。
 * 失败时抛出，由调用方决定展示位置。
 */
export async function approvePlan(
  sessionId: string,
  planId: string,
  action: PlanAction,
  executionEngine?: SessionEngine,
): Promise<void> {
  const result = await rpc.call<SessionPlanApproveResult>("session/plan_approve", {
    session_id: sessionId,
    plan_id: planId,
    action,
    ...(executionEngine ? { execution_engine: executionEngine } : {}),
  });
  useTranscript.getState().mutate(sessionId, (view) => {
    const current = view.plans.find((item) => item.plan_id === planId);
    if (current) current.status = action === "reject" ? "rejected" : "approved";
  });
  if (action === "execute_new_session") await openSession(result.session_id);
  if (result.auto_prompt) {
    void sendPrompt(result.session_id, [{ type: "text", text: result.auto_prompt }]);
  }
}
