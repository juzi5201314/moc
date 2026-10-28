// 会话生命周期：打开（写租约 → 只读降级）、分页、关闭、新建、删除、重命名、回退、重连。
//
// 只有 `session/load` 会取跨进程写租约，所以打开时先 load（max_entries=1，顺带拿
// todos / plans），再用 `session/load_page` 取展示用的转写。

import { toast } from "sonner";
import { attachEntryIds, entriesToBlocks } from "@/lib/transcript-model";
import { clientListeners, rpc } from "@/protocol/client";
import { ErrorCode, RpcError } from "@/protocol/errors";
import type {
  ContextUsageResult,
  PluginListResult,
  SessionInfo,
  SessionLoadPageResult,
  SessionLoadResult,
  SessionMode,
} from "@/protocol/types";
import { useComposer } from "./composer";
import { useConnection } from "./connection";
import { useSessions } from "./sessions";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";
import { requestComposerFocus } from "./ui";

const PAGE_SIZE = 80;
const MAX_MESSAGE_BYTES = 65536;
const CLOSE_WAIT_MS = 5000;

/** 已在加载中的会话：防止重复点击并发两次 load。 */
const opening = new Map<string, Promise<void>>();

/** `readOnly`：强制只读打开（子代理会话不抢父进程的写租约）。 */
export type OpenOptions = { activate?: boolean; readOnly?: boolean };

/** 打开会话：加入标签并加载；已加载的只切换。若此前有未提交的空白新会话，自动丢弃。 */
export function openSession(sessionId: string, options: OpenOptions = {}): Promise<void> {
  if (sessionId !== "draft-session") {
    useSessions.getState().setDraftSession(null);
    useTranscript.getState().drop("draft-session");
    useTabs.getState().close("draft-session");
  }
  useTabs.getState().add(sessionId, options.activate ?? true);
  if (useTranscript.getState().bySession[sessionId]) return Promise.resolve();
  const existing = opening.get(sessionId);
  if (existing) return existing;
  const task = loadSession(sessionId, options.readOnly ?? false).finally(() =>
    opening.delete(sessionId),
  );
  opening.set(sessionId, task);
  return task;
}

async function loadSession(sessionId: string, requestedReadOnly: boolean): Promise<void> {
  const known = useSessions.getState().list.find((item) => item.id === sessionId);
  // 子代理会话只读查看，不抢父进程的写租约。
  const forceReadOnly = requestedReadOnly || Boolean(known?.parent_session_id);
  let loaded: SessionLoadResult;
  let readOnly = forceReadOnly;
  try {
    loaded = await rpc.call<SessionLoadResult>("session/load", {
      session_id: sessionId,
      max_entries: 1,
      read_only: forceReadOnly,
    });
  } catch (error) {
    if (!(error instanceof RpcError) || error.code !== ErrorCode.SessionLocked) {
      if (error instanceof RpcError && error.code === ErrorCode.SessionNotFound) {
        useTabs.getState().close(sessionId);
      }
      throw error;
    }
    readOnly = true;
    loaded = await rpc.call<SessionLoadResult>("session/load", {
      session_id: sessionId,
      max_entries: 1,
      read_only: true,
    });
  }

  const page = await rpc.call<SessionLoadPageResult>("session/load_page", {
    session_id: sessionId,
    max_entries: PAGE_SIZE,
    max_message_bytes: MAX_MESSAGE_BYTES,
  });

  const store = useTranscript.getState();
  store.ensureView(page.session);
  store.mutate(sessionId, (view) => {
    view.info = page.session;
    view.readOnly = readOnly;
    view.holder = readOnly ? (loaded.session.lock ?? null) : null;
    view.blocks = entriesToBlocks(page.entries, page.child_calls ?? [], "p0-");
    view.hasMore = page.has_more;
    view.beforeEntryId = page.entries[0]?.id;
    view.todos = loaded.todos ?? page.todos ?? [];
    view.plans = loaded.plans ?? page.plans ?? [];
  });
  useSessions.getState().upsert(page.session);
  void refreshSideData(sessionId);
}

/** 上下文占用与插件故障提示；读不到只影响对应的小部件。 */
export async function refreshSideData(sessionId: string): Promise<void> {
  const store = useTranscript.getState();
  const [context, plugins] = await Promise.allSettled([
    rpc.call<ContextUsageResult>("session/context", { session_id: sessionId }),
    rpc.call<PluginListResult>("plugin/list", { session_id: sessionId }),
  ]);
  store.mutate(sessionId, (view) => {
    if (context.status === "fulfilled") view.context = context.value;
    if (plugins.status === "fulfilled") {
      const failed = plugins.value.plugins.find((plugin) => plugin.error);
      view.pluginNotice = failed
        ? { plugin: failed.name, code: "E_PLUGIN_DISABLED", message: failed.error ?? "" }
        : null;
    }
  });
}

/** 转写滚到顶时往前翻一页。 */
export async function loadOlder(sessionId: string): Promise<void> {
  const view = useTranscript.getState().bySession[sessionId];
  if (!view?.hasMore || view.loadingOlder || !view.beforeEntryId) return;
  const store = useTranscript.getState();
  store.mutate(sessionId, (draft) => {
    draft.loadingOlder = true;
  });
  try {
    const page = await rpc.call<SessionLoadPageResult>("session/load_page", {
      session_id: sessionId,
      before_entry_id: view.beforeEntryId,
      max_entries: PAGE_SIZE,
      max_message_bytes: MAX_MESSAGE_BYTES,
    });
    store.mutate(sessionId, (draft) => {
      const prefix = `p${page.entries[0]?.id ?? draft.seq}-`;
      draft.blocks = [
        ...entriesToBlocks(page.entries, page.child_calls ?? [], prefix),
        ...draft.blocks,
      ];
      draft.hasMore = page.has_more;
      draft.beforeEntryId = page.entries[0]?.id;
    });
  } finally {
    store.mutate(sessionId, (draft) => {
      draft.loadingOlder = false;
    });
  }
}

/** 提示结束后用最新一页补上直播用户块的 entry id（回退依赖它）。 */
export async function syncEntryIds(sessionId: string): Promise<void> {
  const page = await rpc.call<SessionLoadPageResult>("session/load_page", {
    session_id: sessionId,
    max_entries: 20,
    max_message_bytes: 1024,
  });
  useTranscript.getState().mutate(sessionId, (view) => attachEntryIds(view.blocks, page.entries));
}

/** 子代理会话：状态变化时重拉最新一页（只读）。 */
export async function reloadReadOnly(sessionId: string): Promise<void> {
  const page = await rpc.call<SessionLoadPageResult>("session/load_page", {
    session_id: sessionId,
    max_entries: PAGE_SIZE,
    max_message_bytes: MAX_MESSAGE_BYTES,
  });
  useTranscript.getState().mutate(sessionId, (view) => {
    view.info = page.session;
    view.blocks = entriesToBlocks(page.entries, page.child_calls ?? [], "p0-");
    view.hasMore = page.has_more;
    view.beforeEntryId = page.entries[0]?.id;
  });
}

/** 只读横幅「重新获取写权限」：丢掉视图后按写模式重开。 */
export async function reacquire(sessionId: string): Promise<void> {
  useTranscript.getState().drop(sessionId);
  await openSession(sessionId);
}

function waitForIdle(sessionId: string): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  const timer = window.setTimeout(done, CLOSE_WAIT_MS);
  const unsubscribe = useTranscript.subscribe((state) => {
    if (!state.bySession[sessionId]?.running) done();
  });
  function done() {
    window.clearTimeout(timer);
    unsubscribe();
    resolve();
  }
  return promise;
}

/**
 * 关闭标签。如果是空白临时新会话，直接丢弃且不触发后端 RPC。
 * 运行中的会话由调用方先确认；这里负责 cancel → 等收尾 → unload。
 * 只读会话不持有租约，直接移除。
 */
export async function closeSession(sessionId: string): Promise<void> {
  if (sessionId === "draft-session") {
    useSessions.getState().setDraftSession(null);
    useTabs.getState().close(sessionId);
    useTranscript.getState().drop(sessionId);
    return;
  }
  const view = useTranscript.getState().bySession[sessionId];
  if (view && !view.readOnly) {
    if (view.running) {
      await rpc.call("session/cancel", { session_id: sessionId }).catch(() => undefined);
      await waitForIdle(sessionId);
    }
    await rpc.call("session/unload", { session_id: sessionId }).catch(() => undefined);
  }
  useTabs.getState().close(sessionId);
  useTranscript.getState().drop(sessionId);
}

/** 释放写租约但保留标签（只读重开）。 */
export async function releaseSession(sessionId: string): Promise<void> {
  await rpc.call("session/unload", { session_id: sessionId });
  useTranscript.getState().mutate(sessionId, (view) => {
    view.readOnly = true;
    view.holder = null;
  });
}

export type NewSessionOptions = { cwd?: string; model?: string; mode?: SessionMode };

export async function newSession(options: NewSessionOptions = {}): Promise<SessionInfo> {
  const info = await rpc.call<SessionInfo>("session/new", {
    cwd: options.cwd,
    model: options.model,
    mode: options.mode ?? "normal",
  });
  // session/new 已取写租约；第一条消息落盘前 session/list 看不到它。
  useSessions.getState().upsert(info);
  const store = useTranscript.getState();
  store.ensureView(info);
  useTabs.getState().add(info.id);
  void refreshSideData(info.id);
  return info;
}

/**
 * 极速新建会话：对齐 deepseek-harness 交互。
 * 不立即写入后端数据库，而是创建本地空白临时会话（draft）；
 * 若用户离开或关闭，直接丢弃；仅在输入首条提示词后才向后端写入会话。
 */
export function createQuickSession(targetCwd?: string): SessionInfo {
  let cwd = targetCwd;
  if (!cwd) {
    const activeId = useTabs.getState().active;
    if (activeId && activeId !== "draft-session") {
      cwd = useTranscript.getState().bySession[activeId]?.info?.cwd;
    }
  }
  if (!cwd) {
    const workspaces = useSessions.getState().workspaces;
    const list = useSessions.getState().list;
    cwd = workspaces[0] ?? list[0]?.cwd ?? "/home/soeur/project/moc";
  }

  const activeId = useTabs.getState().active;
  const currentModel =
    activeId && activeId !== "draft-session"
      ? useTranscript.getState().bySession[activeId]?.info?.model
      : undefined;
  const models = useConnection.getState().config?.models;
  const defaultModel = currentModel || models?.[0]?.id;

  const draftId = "draft-session";
  const draftInfo: SessionInfo = {
    id: draftId,
    title: "新会话",
    cwd,
    model: defaultModel ?? "",
    mode: "normal",
    permission: "full",
    created_at_ms: Date.now(),
    updated_at_ms: Date.now(),
  };

  useSessions.getState().setDraftSession({
    id: draftId,
    cwd,
    title: "新会话",
    isDraft: true,
    model: defaultModel,
    mode: "normal",
  });

  useTranscript.getState().ensureView(draftInfo);
  useTabs.getState().add(draftId);
  requestComposerFocus();
  return draftInfo;
}

export async function deleteSession(sessionId: string): Promise<void> {
  await rpc.call("session/delete", { session_id: sessionId });
  useTabs.getState().close(sessionId);
  useTranscript.getState().drop(sessionId);
  useSessions.getState().remove(sessionId);
  await useSessions.getState().refresh();
}

export async function renameSession(sessionId: string, title: string): Promise<void> {
  const trimmed = title.trim();
  if (!trimmed) return;
  const info = await rpc.call<SessionInfo>("session/rename", {
    session_id: sessionId,
    title: trimmed,
  });
  useSessions.getState().upsert(info);
  useTranscript.getState().mutate(sessionId, (view) => {
    view.info = info;
  });
}

/** 分叉/分支会话：继承当前会话的上下文工作区、模式与模型，开启独立新分支 */
export async function forkSession(sessionId: string): Promise<SessionInfo | undefined> {
  const current =
    useTranscript.getState().bySession[sessionId]?.info ??
    useSessions.getState().list.find((s) => s.id === sessionId);
  if (!current) return undefined;
  try {
    const info = await newSession({
      cwd: current.cwd,
      model: current.model,
      mode: current.mode,
    });
    const forkTitle = `${current.title || "会话"} (分支)`;
    await renameSession(info.id, forkTitle).catch(() => undefined);
    toast.success(`已分叉生成新会话: ${forkTitle}`);
    return info;
  } catch (error) {
    toast.error(`分叉会话失败: ${error instanceof Error ? error.message : String(error)}`);
    return undefined;
  }
}

/**
 * 从某条消息回退。core 返回分叉会话（id 不同）或原地回退（id 相同）；
 * 目标是用户消息时把原文还给输入框。
 */
export async function rewind(
  sessionId: string,
  entryId: string,
  restoreText?: string,
): Promise<void> {
  const result = await rpc.call<SessionLoadResult>("session/rewind", {
    session_id: sessionId,
    entry_id: entryId,
  });
  const targetId = result.session.id;
  if (targetId !== sessionId) useTabs.getState().replace(sessionId, targetId);
  useTranscript.getState().drop(targetId);
  useSessions.getState().upsert(result.session);
  await openSession(targetId);
  if (restoreText !== undefined) {
    useComposer.getState().restore(targetId, { text: restoreText, images: [] });
  }
  void useSessions.getState().refresh();
}

// ---------- 连接事件 ----------

clientListeners.status.add((status) => {
  if (status === "offline") useTranscript.getState().interruptAll();
});

clientListeners.ready.add(() => {
  void (async () => {
    await useSessions
      .getState()
      .refresh()
      .catch(() => undefined);
    // 重连后视图已失效：全部丢掉，按标签顺序重开（写租约随连接断开已释放）。
    const views = Object.keys(useTranscript.getState().bySession);
    for (const id of views) useTranscript.getState().drop(id);
    const { open, active } = useTabs.getState();
    for (const id of open) {
      await openSession(id, { activate: id === active }).catch(() => undefined);
    }
  })();
});
