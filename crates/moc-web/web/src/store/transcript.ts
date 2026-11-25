// 每个已打开会话的视图（SessionView），以及 session/update 路由与反向请求队列。
//
// 反向请求（权限、提问）的 resolver 放在模块级 Map，store 里只存可序列化的参数，
// 避免 immer 草稿里混入函数。

import { enablePatches, produceWithPatches } from "immer";
import { create } from "zustand";
import { recordBlockChange } from "@/lib/block-changes";
import { t } from "@/lib/i18n";
import { routeOf } from "@/lib/route";
import {
  appendLocalStatus,
  appendLocalUser,
  applyUpdate,
  createView,
  interruptLive,
  type SessionView,
  type UserAttachment,
} from "@/lib/transcript-model";
import { invalidateGitBranch } from "@/lib/use-git-branch";
import { rpc } from "@/protocol/client";
import type {
  AskParams,
  AskResult,
  ContextUsageResult,
  ImageSource,
  JobListResult,
  PermissionRequestParams,
  PermissionResult,
  SessionInfo,
  SessionUpdate,
  SessionUpdateParams,
} from "@/protocol/types";
import { usePrefs } from "./prefs";
import { useSessionMeta } from "./session-meta";
import { useSessions } from "./sessions";
import { useTabs } from "./tabs";
import { cancelAllViewReads, cancelViewReads, startViewReads, viewReadSignal } from "./view-reads";

export type Activity = { running: boolean };

export type TranscriptState = {
  bySession: Record<string, SessionView>;
  /** 未打开会话也要显示的运行标记（侧栏状态点）。 */
  activity: Record<string, Activity>;
  permissions: Record<string, PermissionRequestParams[]>;
  asks: Record<string, AskParams[]>;
  ensureView: (info: SessionInfo) => void;
  mutate: (sessionId: string, recipe: (view: SessionView) => void) => void;
  drop: (sessionId: string) => void;
  localUser: (
    sessionId: string,
    text: string,
    images: ImageSource[],
    attachments?: UserAttachment[],
  ) => void;
  localStatus: (sessionId: string, tone: "info" | "warn" | "error", text: string) => void;
  failPrompt: (sessionId: string) => void;
  /** 断线：收尾全部直播轮，清掉待处理的反向请求（core 已按拒绝处理）。 */
  interruptAll: () => void;
};

enablePatches();

function withBlockChanges(recipe: (state: TranscriptState) => void) {
  return (state: TranscriptState): TranscriptState => {
    const [next, patches] = produceWithPatches(state, recipe);
    const changed = new Map<string, number>();
    for (const { path } of patches) {
      if (path[0] !== "bySession" || typeof path[1] !== "string") continue;
      if (path.length > 2 && path[2] !== "blocks") continue;
      const from = typeof path[3] === "number" ? path[3] : 0;
      changed.set(path[1], Math.min(changed.get(path[1]) ?? from, from));
    }
    for (const [sessionId, from] of changed) {
      const previous = state.bySession[sessionId]?.blocks;
      const blocks = next.bySession[sessionId]?.blocks;
      if (previous && blocks) recordBlockChange(previous, blocks, from);
    }
    return next;
  };
}

const permissionResolvers = new Map<string, (result: PermissionResult) => void>();
const askResolvers = new Map<string, (result: AskResult) => void>();

export const useTranscript = create<TranscriptState>()((set) => ({
  bySession: {},
  activity: {},
  permissions: {},
  asks: {},
  ensureView: (info) => {
    startViewReads(info.id);
    set(
      withBlockChanges((state: TranscriptState) => {
        const existing = state.bySession[info.id];
        if (existing) existing.info = info;
        else state.bySession[info.id] = createView(info);
      }),
    );
  },
  mutate: (sessionId, recipe) => {
    flushDeltas();
    set(
      withBlockChanges((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) recipe(view);
      }),
    );
  },
  drop: (sessionId) => {
    cancelViewReads(sessionId);
    flushDeltas();
    set(
      withBlockChanges((state: TranscriptState) => {
        delete state.bySession[sessionId];
      }),
    );
  },
  localUser: (sessionId, text, images, attachments) => {
    flushDeltas();
    set(
      withBlockChanges((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) appendLocalUser(view, text, images, attachments);
      }),
    );
  },
  localStatus: (sessionId, tone, text) => {
    flushDeltas();
    set(
      withBlockChanges((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) appendLocalStatus(view, tone, text);
      }),
    );
  },
  failPrompt: (sessionId) => {
    flushDeltas();
    const prev = useTranscript.getState();
    const wasRunning = Boolean(
      prev.activity[sessionId]?.running || prev.bySession[sessionId]?.running,
    );
    set(
      withBlockChanges((state: TranscriptState) => {
        delete state.activity[sessionId];
        const view = state.bySession[sessionId];
        if (!view) return;
        if (view.live) {
          applyUpdate(view, {
            type: "prompt_finished",
            prompt_id: view.live.promptId,
            message: { role: "Assistant", content: [] },
            finish_reason: { Other: "error" },
            usage: { input_tokens: 0, output_tokens: 0 },
          });
        } else {
          view.running = false;
        }
      }),
    );
    if (wasRunning && shouldMarkUnread(sessionId)) {
      useSessionMeta.getState().markUnread(sessionId);
    }
  },
  interruptAll: () => {
    cancelAllViewReads();
    flushDeltas();
    for (const resolve of permissionResolvers.values()) resolve({ option_id: "" });
    permissionResolvers.clear();
    for (const resolve of askResolvers.values()) resolve({ answers: [] });
    askResolvers.clear();
    set(
      withBlockChanges((state: TranscriptState) => {
        for (const view of Object.values(state.bySession)) interruptLive(view);
        state.activity = {};
        state.permissions = {};
        state.asks = {};
      }),
    );
  },
}));

// ---------- session/update 路由 ----------

type PendingDelta = { params: SessionUpdateParams; at: number };
const pendingDeltas = new Map<string, PendingDelta>();
let deltaTimer: ReturnType<typeof setTimeout> | undefined;
let pendingChars = 0;

/** 只合并相邻同通道增量；工具、结束、回退等控制事件之前必须落地。 */
function flushDeltas(): void {
  if (deltaTimer !== undefined) clearTimeout(deltaTimer);
  deltaTimer = undefined;
  if (!pendingDeltas.size) return;
  const pending = [...pendingDeltas.values()];
  pendingDeltas.clear();
  pendingChars = 0;
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      for (const { params, at } of pending) {
        const view = state.bySession[params.session_id];
        if (view) applyUpdate(view, params.update, at);
      }
    }),
  );
}

/** 不会改写仓库 HEAD 的纯只读或会话控制工具；其余工具（bash、execute、子代理、插件/MCP）结束立即刷新分支。 */
const GIT_SAFE_READONLY_TOOLS = new Set([
  "read",
  "grep",
  "find",
  "todo",
  "plan",
  "ask",
  "webfetch",
  "job",
]);

function mayChangeGitBranch(update: SessionUpdate): boolean {
  if (
    update.type === "job_finished" ||
    update.type === "prompt_finished" ||
    update.type === "agent_result"
  ) {
    return true;
  }
  if (update.type === "tool_finished") {
    return !GIT_SAFE_READONLY_TOOLS.has(update.name);
  }
  if (update.type === "agent_status_changed") {
    return (
      update.status === "completed" ||
      update.status === "stopped" ||
      update.status === "failed" ||
      update.status === "idle"
    );
  }
  return false;
}

function sessionCwd(sessionId: string): string | undefined {
  const open = useTranscript.getState().bySession[sessionId]?.info.cwd;
  if (open) return open;
  const sessions = useSessions.getState();
  return sessions.extra[sessionId]?.cwd ?? sessions.list.find((item) => item.id === sessionId)?.cwd;
}

/** 仅对顶层会话且用户当前未停留在其会话视图时标未读（子代理不进侧栏）。 */
function shouldMarkUnread(sessionId: string): boolean {
  if (!sessionId || sessionId === "draft-session") return false;
  const openParent = useTranscript.getState().bySession[sessionId]?.info.parent_session_id;
  if (openParent) return false;
  const sessions = useSessions.getState();
  const known = sessions.extra[sessionId] ?? sessions.list.find((item) => item.id === sessionId);
  if (known?.parent_session_id) return false;
  const inAgentView =
    typeof window === "undefined" || routeOf(window.location.pathname) === "agent";
  return !(inAgentView && useTabs.getState().active === sessionId);
}

rpc.on("session/update", (params: SessionUpdateParams) => {
  const { session_id: sessionId, update } = params;
  // Agent 或手动命令执行 bash/execute/子代理/后台任务后可能已切换分支，立即失效并重查。
  if (mayChangeGitBranch(update)) {
    invalidateGitBranch(sessionCwd(sessionId));
  }
  if (update.type === "assistant_delta" && update.delta.type !== "usage") {
    if (!useTranscript.getState().bySession[sessionId]) return;
    const previous = pendingDeltas.get(sessionId);
    const prior = previous?.params.update;
    if (
      prior?.type === "assistant_delta" &&
      prior.prompt_id === update.prompt_id &&
      prior.delta.type === update.delta.type
    ) {
      prior.delta.delta += update.delta.delta;
    } else {
      // 通道切换决定思考耗时和正文顺序，不得与下一段串接。
      if (previous) flushDeltas();
      pendingDeltas.set(sessionId, {
        params: { ...params, update: { ...update, delta: { ...update.delta } } },
        at: Date.now(),
      });
    }
    pendingChars += update.delta.delta.length;
    if (pendingChars >= 64 * 1024) flushDeltas();
    else if (deltaTimer === undefined) deltaTimer = setTimeout(flushDeltas, 16);
    return;
  }
  flushDeltas();
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      if (update.type === "prompt_started") state.activity[sessionId] = { running: true };
      if (update.type === "prompt_finished") delete state.activity[sessionId];
      const view = state.bySession[sessionId];
      if (view) applyUpdate(view, update);
    }),
  );
  if (update.type === "prompt_started") {
    useSessionMeta.getState().markRead(sessionId);
  } else if (update.type === "prompt_finished" && shouldMarkUnread(sessionId)) {
    useSessionMeta.getState().markUnread(sessionId);
  }
  if (
    CONTEXT_EVENTS.has(update.type) ||
    (update.type === "assistant_delta" && update.delta.type === "usage")
  ) {
    scheduleContextRefresh(sessionId);
  }
});

/** 改变上下文组成或窗口的会话事件：每次模型请求结束（usage）、消息与工具结果写入、压缩、回退、模式与模型切换。 */
const CONTEXT_EVENTS = new Set<SessionUpdate["type"]>([
  "prompt_started",
  "steering_applied",
  "tool_finished",
  "prompt_finished",
  "prompt_rewound",
  "context_compacted",
  "model_changed",
  "engine_changed",
  "plan_mode_changed",
  "permission_changed",
  "autonomy_changed",
]);

/** 每个会话最多一个在途读取；期间再有事件只记一次，读完补读最新值，连续工具结果不会堆积请求。 */
const contextReads = new Map<string, { again: boolean }>();

function scheduleContextRefresh(sessionId: string): void {
  const pending = contextReads.get(sessionId);
  if (pending) {
    pending.again = true;
    return;
  }
  const state = { again: false };
  contextReads.set(sessionId, state);
  void refreshContext(sessionId)
    .catch(() => undefined)
    .finally(() => {
      contextReads.delete(sessionId);
      if (state.again) scheduleContextRefresh(sessionId);
    });
}

/** 只重读上下文占用（圆环与简略上下文窗口）；视图已关闭时不发请求，读不到保留旧值。 */
export async function refreshContext(sessionId: string): Promise<void> {
  const signal = viewReadSignal(sessionId);
  if (!signal) return;
  const context = await rpc.call<ContextUsageResult>("session/context", { session_id: sessionId });
  if (signal.aborted) return;
  useTranscript.getState().mutate(sessionId, (view) => {
    view.context = context;
  });
}

export async function refreshJobs(sessionId: string): Promise<void> {
  const signal = viewReadSignal(sessionId);
  if (!signal) return;
  // tail_lines 让服务端顺带回传未结束任务的输出，省掉逐 job 的 job/output。
  const result = await rpc.call<JobListResult>("job/list", {
    session_id: sessionId,
    tail_lines: 20,
  });
  if (signal.aborted) return;
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      const view = state.bySession[sessionId];
      if (!view) return;
      view.jobs = Object.fromEntries(result.jobs.map((job) => [job.job_id, job]));
      for (const job of result.jobs) {
        applyUpdate(view, { type: "job_updated", job });
        const tail = result.outputs?.[job.job_id];
        if (tail !== undefined) {
          applyUpdate(view, {
            type: "job_output",
            job_id: job.job_id,
            tool_call_id: job.tool_call_id,
            tail,
          });
        }
      }
    }),
  );
}

// ---------- 反向请求 ----------

/** 页面在后台且用户开了通知时，提醒有待处理的请求。 */
function notifyBackground(title: string, body: string): void {
  if (!usePrefs.getState().notify || !document.hidden) return;
  if (typeof Notification === "undefined" || Notification.permission !== "granted") return;
  const notification = new Notification(title, { body, tag: "moc-request" });
  notification.onclick = () => {
    window.focus();
    notification.close();
  };
}

rpc.onRequest("session/request_permission", (params) => {
  flushDeltas();
  const { promise, resolve } = Promise.withResolvers<PermissionResult>();
  permissionResolvers.set(params.request_id, resolve);
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      state.permissions[params.session_id] = [
        ...(state.permissions[params.session_id] ?? []),
        params,
      ];
    }),
  );
  notifyBackground(t("notify.permission"), params.title);
  return promise;
});

rpc.onRequest("session/request_ask", (params) => {
  flushDeltas();
  const { promise, resolve } = Promise.withResolvers<AskResult>();
  askResolvers.set(params.request_id, resolve);
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      state.asks[params.session_id] = [...(state.asks[params.session_id] ?? []), params];
    }),
  );
  notifyBackground(t("notify.ask"), params.questions[0]?.question ?? "");
  return promise;
});

/** 回选一个权限选项并出队。 */
export function resolvePermission(sessionId: string, requestId: string, optionId: string): void {
  permissionResolvers.get(requestId)?.({ option_id: optionId });
  permissionResolvers.delete(requestId);
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      const list = state.permissions[sessionId];
      if (list) state.permissions[sessionId] = list.filter((item) => item.request_id !== requestId);
    }),
  );
}

/** 回答提问并出队。 */
export function resolveAsk(sessionId: string, requestId: string, result: AskResult): void {
  askResolvers.get(requestId)?.(result);
  askResolvers.delete(requestId);
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      const list = state.asks[sessionId];
      if (list) state.asks[sessionId] = list.filter((item) => item.request_id !== requestId);
    }),
  );
}

/** 需要用户处理的会话数（权限 + 提问 + 待审计划），供收件箱与标签标记。 */
export function attentionCount(state: TranscriptState, sessionId: string): number {
  const plans = state.bySession[sessionId]?.plans.filter((plan) => plan.status === "submitted");
  return (
    (state.permissions[sessionId]?.length ?? 0) +
    (state.asks[sessionId]?.length ?? 0) +
    (plans?.length ?? 0)
  );
}
