// 每个已打开会话的视图（SessionView），以及 session/update 路由与反向请求队列。
//
// 反向请求（权限、提问）的 resolver 放在模块级 Map，store 里只存可序列化的参数，
// 避免 immer 草稿里混入函数。

import { enablePatches, produceWithPatches } from "immer";
import { create } from "zustand";
import { recordBlockChange } from "@/lib/block-changes";
import {
  appendLocalStatus,
  appendLocalUser,
  applyUpdate,
  createView,
  interruptLive,
  type SessionView,
} from "@/lib/transcript-model";
import { clientListeners, rpc } from "@/protocol/client";
import type {
  AskParams,
  AskResult,
  ImageSource,
  JobListResult,
  PermissionRequestParams,
  PermissionResult,
  SessionInfo,
  SessionUpdateParams,
} from "@/protocol/types";
import { usePrefs } from "./prefs";
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
  localUser: (sessionId: string, text: string, images: ImageSource[]) => void;
  localStatus: (sessionId: string, tone: "info" | "warn" | "error", text: string) => void;
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
  localUser: (sessionId, text, images) => {
    flushDeltas();
    set(
      withBlockChanges((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) appendLocalUser(view, text, images);
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

rpc.on("session/update", (params: SessionUpdateParams) => {
  const { session_id: sessionId, update } = params;
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
});

export async function refreshJobs(sessionId: string): Promise<void> {
  const signal = viewReadSignal(sessionId);
  if (!signal) return;
  const result = await rpc.call<JobListResult>("job/list", { session_id: sessionId });
  if (signal.aborted) return;
  useTranscript.setState(
    withBlockChanges((state: TranscriptState) => {
      const view = state.bySession[sessionId];
      if (!view) return;
      view.jobs = Object.fromEntries(result.jobs.map((job) => [job.job_id, job]));
      for (const job of result.jobs) applyUpdate(view, { type: "job_updated", job });
    }),
  );
  await Promise.all(
    result.jobs
      .filter((job) => job.status === "running" || job.status === "killing")
      .map(async (job) => {
        const output = await rpc.call<{ text: string }>("job/output", {
          session_id: sessionId,
          job_id: job.job_id,
          tail_lines: 20,
        });
        if (signal.aborted) return;
        useTranscript.setState(
          withBlockChanges((state: TranscriptState) => {
            const view = state.bySession[sessionId];
            if (view) {
              applyUpdate(view, {
                type: "job_output",
                job_id: job.job_id,
                tool_call_id: job.tool_call_id,
                tail: output.text,
              });
            }
          }),
        );
      }),
  );
}

clientListeners.ready.add(() => {
  for (const sessionId of Object.keys(useTranscript.getState().bySession)) {
    void refreshJobs(sessionId).catch(() => undefined);
  }
});

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
  notifyBackground("moc 需要授权", params.title);
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
  notifyBackground("moc 有问题需要回答", params.questions[0]?.question ?? "");
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
