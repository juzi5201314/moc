// 每个已打开会话的视图（SessionView），以及 session/update 路由与反向请求队列。
//
// 反向请求（权限、提问）的 resolver 放在模块级 Map，store 里只存可序列化的参数，
// 避免 immer 草稿里混入函数。

import { produce } from "immer";
import { create } from "zustand";
import {
  appendLocalStatus,
  appendLocalUser,
  applyUpdate,
  createView,
  interruptLive,
  type SessionView,
} from "@/lib/transcript-model";
import { rpc } from "@/protocol/client";
import type {
  AskParams,
  AskResult,
  ImageSource,
  PermissionRequestParams,
  PermissionResult,
  SessionInfo,
  SessionUpdateParams,
} from "@/protocol/types";
import { usePrefs } from "./prefs";

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

const permissionResolvers = new Map<string, (result: PermissionResult) => void>();
const askResolvers = new Map<string, (result: AskResult) => void>();

export const useTranscript = create<TranscriptState>()((set) => ({
  bySession: {},
  activity: {},
  permissions: {},
  asks: {},
  ensureView: (info) =>
    set(
      produce((state: TranscriptState) => {
        const existing = state.bySession[info.id];
        if (existing) existing.info = info;
        else state.bySession[info.id] = createView(info);
      }),
    ),
  mutate: (sessionId, recipe) =>
    set(
      produce((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) recipe(view);
      }),
    ),
  drop: (sessionId) =>
    set(
      produce((state: TranscriptState) => {
        delete state.bySession[sessionId];
      }),
    ),
  localUser: (sessionId, text, images) =>
    set(
      produce((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) appendLocalUser(view, text, images);
      }),
    ),
  localStatus: (sessionId, tone, text) =>
    set(
      produce((state: TranscriptState) => {
        const view = state.bySession[sessionId];
        if (view) appendLocalStatus(view, tone, text);
      }),
    ),
  interruptAll: () => {
    for (const resolve of permissionResolvers.values()) resolve({ option_id: "" });
    permissionResolvers.clear();
    for (const resolve of askResolvers.values()) resolve({ answers: [] });
    askResolvers.clear();
    set(
      produce((state: TranscriptState) => {
        for (const view of Object.values(state.bySession)) interruptLive(view);
        state.activity = {};
        state.permissions = {};
        state.asks = {};
      }),
    );
  },
}));

// ---------- session/update 路由 ----------

rpc.on("session/update", (params: SessionUpdateParams) => {
  const { session_id: sessionId, update } = params;
  useTranscript.setState(
    produce((state: TranscriptState) => {
      if (update.type === "prompt_started") state.activity[sessionId] = { running: true };
      if (update.type === "prompt_finished") delete state.activity[sessionId];
      const view = state.bySession[sessionId];
      if (view) applyUpdate(view, update);
    }),
  );
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
  const { promise, resolve } = Promise.withResolvers<PermissionResult>();
  permissionResolvers.set(params.request_id, resolve);
  useTranscript.setState(
    produce((state: TranscriptState) => {
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
  const { promise, resolve } = Promise.withResolvers<AskResult>();
  askResolvers.set(params.request_id, resolve);
  useTranscript.setState(
    produce((state: TranscriptState) => {
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
    produce((state: TranscriptState) => {
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
    produce((state: TranscriptState) => {
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
