// 会话侧栏视图元数据：按工作区保存的手动排序与未读会话标记。
// 属于浏览器本地视图态（与 tabs / layout 同层），持久化在 localStorage，不写 config.toml / web.toml
// 与工作目录；随 session/list 刷新与删除会话自动 GC 裁剪，空态直接移除 key 不留垃圾数据。

import { create } from "zustand";
import type { SessionInfo } from "@/protocol/types";

export const SESSION_ORDER_KEY = "moc-web-session-order";
export const SESSION_UNREAD_KEY = "moc-web-session-unread";

export type DropEdge = "before" | "after";

type OrderByCwd = Record<string, string[]>;
type UnreadMap = Record<string, true>;

function sanitizeIds(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const item of raw) {
    if (typeof item !== "string" || !item || item === "draft-session" || seen.has(item)) continue;
    seen.add(item);
    out.push(item);
  }
  return out;
}

function loadOrder(): OrderByCwd {
  try {
    const raw = localStorage.getItem(SESSION_ORDER_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : null;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    const out: OrderByCwd = {};
    for (const [cwd, value] of Object.entries(parsed as Record<string, unknown>)) {
      if (!cwd) continue;
      const ids = sanitizeIds(value);
      if (ids.length > 1) out[cwd] = ids;
    }
    return out;
  } catch {
    return {};
  }
}

function loadUnread(): UnreadMap {
  try {
    const raw = localStorage.getItem(SESSION_UNREAD_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : null;
    const ids = sanitizeIds(parsed);
    const out: UnreadMap = {};
    for (const id of ids) out[id] = true;
    return out;
  } catch {
    return {};
  }
}

function persistOrder(orderByCwd: OrderByCwd): void {
  try {
    if (Object.keys(orderByCwd).length === 0) {
      localStorage.removeItem(SESSION_ORDER_KEY);
    } else {
      localStorage.setItem(SESSION_ORDER_KEY, JSON.stringify(orderByCwd));
    }
  } catch {
    // 隐私模式或配额受限时忽略持久化失败，仅影响刷新后的恢复
  }
}

function persistUnread(unread: UnreadMap): void {
  try {
    const ids = Object.keys(unread);
    if (ids.length === 0) {
      localStorage.removeItem(SESSION_UNREAD_KEY);
    } else {
      localStorage.setItem(SESSION_UNREAD_KEY, JSON.stringify(ids));
    }
  } catch {
    // 隐私模式或配额受限时忽略持久化失败，仅影响刷新后的恢复
  }
}

/**
 * 按工作区自定义顺序排布会话：
 * 未记录的新会话自动置顶（保持原有按更新时间降序），已记录会话严格保持用户手动位置。
 */
export function applySessionOrder(
  sessions: SessionInfo[],
  order: readonly string[] | undefined,
): SessionInfo[] {
  if (!order || order.length === 0 || sessions.length <= 1) return sessions;
  const rank = new Map<string, number>();
  for (let i = 0; i < order.length; i++) rank.set(order[i], i);

  const unranked: SessionInfo[] = [];
  const ranked: SessionInfo[] = [];
  for (const session of sessions) {
    if (rank.has(session.id)) ranked.push(session);
    else unranked.push(session);
  }
  if (ranked.length === 0) return sessions;
  ranked.sort((a, b) => (rank.get(a.id) ?? 0) - (rank.get(b.id) ?? 0));
  return unranked.length === 0 ? ranked : [...unranked, ...ranked];
}

/**
 * 将 `fromId` 移至 `targetId` 的前/后侧；若结果位置无变化则返回 `null`。
 */
export function moveSessionId(
  ids: readonly string[],
  fromId: string,
  targetId: string,
  edge: DropEdge,
): string[] | null {
  if (fromId === targetId) return null;
  const fromIndex = ids.indexOf(fromId);
  const targetIndex = ids.indexOf(targetId);
  if (fromIndex < 0 || targetIndex < 0) return null;

  const rawInsert = edge === "before" ? targetIndex : targetIndex + 1;
  const insertAt = fromIndex < rawInsert ? rawInsert - 1 : rawInsert;
  if (insertAt === fromIndex) return null;

  const next = [...ids];
  next.splice(fromIndex, 1);
  next.splice(insertAt, 0, fromId);
  return next;
}

function sameList(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

export type SessionMetaState = {
  /** 仅保存用户手动排过序、且至少有 2 条会话的工作区顺序。 */
  orderByCwd: OrderByCwd;
  /** 工作停止后用户尚未查看过的会话 ID 集合（对象形态便于按单条做 O(1) 订阅）。 */
  unread: UnreadMap;

  reorder: (
    cwd: string,
    currentIds: readonly string[],
    fromId: string,
    targetId: string,
    edge: DropEdge,
  ) => void;
  resetOrder: (cwd: string) => void;
  markUnread: (sessionId: string) => void;
  markRead: (sessionId: string) => void;
  removeSession: (sessionId: string) => void;
  /** 用服务端权威存活会话集做 GC 裁剪，清理已删除或迁移目录的陈旧记录。 */
  prune: (liveIds: ReadonlySet<string>, liveCwdById: ReadonlyMap<string, string>) => void;
};

export const useSessionMeta = create<SessionMetaState>()((set, get) => ({
  orderByCwd: loadOrder(),
  unread: loadUnread(),

  reorder: (cwd, currentIds, fromId, targetId, edge) => {
    if (!cwd) return;
    const moved = moveSessionId(currentIds, fromId, targetId, edge);
    if (!moved || moved.length <= 1) return;
    const orderByCwd = { ...get().orderByCwd, [cwd]: moved };
    persistOrder(orderByCwd);
    set({ orderByCwd });
  },

  resetOrder: (cwd) => {
    const current = get().orderByCwd;
    if (!(cwd in current)) return;
    const { [cwd]: _removed, ...orderByCwd } = current;
    persistOrder(orderByCwd);
    set({ orderByCwd });
  },

  markUnread: (sessionId) => {
    if (!sessionId || sessionId === "draft-session") return;
    const current = get().unread;
    if (current[sessionId]) return;
    const unread: UnreadMap = { ...current, [sessionId]: true };
    persistUnread(unread);
    set({ unread });
  },

  markRead: (sessionId) => {
    if (!sessionId) return;
    const current = get().unread;
    if (!current[sessionId]) return;
    const { [sessionId]: _removed, ...unread } = current;
    persistUnread(unread);
    set({ unread });
  },

  removeSession: (sessionId) => {
    if (!sessionId) return;
    const { orderByCwd, unread } = get();
    let orderChanged = false;
    const nextOrder: OrderByCwd = {};
    for (const [cwd, ids] of Object.entries(orderByCwd)) {
      if (!ids.includes(sessionId)) {
        nextOrder[cwd] = ids;
        continue;
      }
      orderChanged = true;
      const filtered = ids.filter((id) => id !== sessionId);
      if (filtered.length > 1) nextOrder[cwd] = filtered;
    }

    const unreadChanged = Boolean(unread[sessionId]);
    let nextUnread = unread;
    if (unreadChanged) {
      const { [sessionId]: _removed, ...rest } = unread;
      nextUnread = rest;
    }

    if (!orderChanged && !unreadChanged) return;
    if (orderChanged) persistOrder(nextOrder);
    if (unreadChanged) persistUnread(nextUnread);
    set({
      ...(orderChanged ? { orderByCwd: nextOrder } : {}),
      ...(unreadChanged ? { unread: nextUnread } : {}),
    });
  },

  prune: (liveIds, liveCwdById) => {
    const { orderByCwd, unread } = get();

    let orderChanged = false;
    const nextOrder: OrderByCwd = {};
    for (const [cwd, ids] of Object.entries(orderByCwd)) {
      const filtered = ids.filter((id) => liveIds.has(id) && liveCwdById.get(id) === cwd);
      if (filtered.length <= 1) {
        orderChanged = true;
        continue;
      }
      if (!sameList(ids, filtered)) orderChanged = true;
      nextOrder[cwd] = filtered;
    }

    let unreadChanged = false;
    const nextUnread: UnreadMap = {};
    for (const id of Object.keys(unread)) {
      if (liveIds.has(id)) nextUnread[id] = true;
      else unreadChanged = true;
    }

    if (!orderChanged && !unreadChanged) return;
    if (orderChanged) persistOrder(nextOrder);
    if (unreadChanged) persistUnread(nextUnread);
    set({
      ...(orderChanged ? { orderByCwd: nextOrder } : {}),
      ...(unreadChanged ? { unread: nextUnread } : {}),
    });
  },
}));

// 同浏览器多标签页同步：其他标签改动排序或未读标记时立即跟进。
if (typeof window !== "undefined") {
  window.addEventListener("storage", (event) => {
    if (event.key === SESSION_ORDER_KEY) {
      useSessionMeta.setState({ orderByCwd: loadOrder() });
    } else if (event.key === SESSION_UNREAD_KEY) {
      useSessionMeta.setState({ unread: loadUnread() });
    }
  });
}
