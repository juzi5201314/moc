// 会话列表与工作区。工作区 = cwd 分组：固定的工作区 ∪ 会话里出现过的 cwd。

import { create } from "zustand";
import { rpc } from "@/protocol/client";
import type { SessionInfo, SessionListResult } from "@/protocol/types";

const PINNED_KEY = "moc-web-workspaces";

function loadPinned(): string[] {
  try {
    const raw = localStorage.getItem(PINNED_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.filter((item) => typeof item === "string") : [];
  } catch {
    return [];
  }
}

export type Workspace = {
  cwd: string;
  pinned: boolean;
  /** 顶层会话（子代理会话挂在父会话下）。 */
  sessions: SessionInfo[];
  updatedAt: number;
};

export type SessionsState = {
  list: SessionInfo[];
  /** 已打开但还不在 session/list 里的会话（首条消息落盘前）。 */
  extra: Record<string, SessionInfo>;
  pinned: string[];
  refresh: () => Promise<void>;
  upsert: (info: SessionInfo) => void;
  remove: (id: string) => void;
  pin: (cwd: string) => void;
  unpin: (cwd: string) => void;
};

export const useSessions = create<SessionsState>()((set, get) => ({
  list: [],
  extra: {},
  pinned: loadPinned(),
  refresh: async () => {
    const result = await rpc.call<SessionListResult>("session/list", {});
    const listed = new Set(result.sessions.map((session) => session.id));
    const extra = Object.fromEntries(Object.entries(get().extra).filter(([id]) => !listed.has(id)));
    set({ list: result.sessions, extra });
  },
  upsert: (info) => {
    const { list, extra } = get();
    if (list.some((session) => session.id === info.id)) {
      set({ list: list.map((session) => (session.id === info.id ? info : session)) });
    } else {
      set({ extra: { ...extra, [info.id]: info } });
    }
  },
  remove: (id) => {
    const { list, extra } = get();
    const { [id]: _removed, ...rest } = extra;
    set({ list: list.filter((session) => session.id !== id), extra: rest });
  },
  pin: (cwd) => {
    const pinned = get().pinned.includes(cwd) ? get().pinned : [...get().pinned, cwd];
    localStorage.setItem(PINNED_KEY, JSON.stringify(pinned));
    set({ pinned });
  },
  unpin: (cwd) => {
    const pinned = get().pinned.filter((item) => item !== cwd);
    localStorage.setItem(PINNED_KEY, JSON.stringify(pinned));
    set({ pinned });
  },
}));

/** 全部会话（list ∪ extra），按更新时间降序。 */
export function allSessions(state: Pick<SessionsState, "list" | "extra">): SessionInfo[] {
  return [...Object.values(state.extra), ...state.list].sort(
    (a, b) => b.updated_at_ms - a.updated_at_ms,
  );
}

/** 父会话 ID → 子代理会话。 */
export function childrenByParent(sessions: SessionInfo[]): Record<string, SessionInfo[]> {
  const result: Record<string, SessionInfo[]> = {};
  for (const session of sessions) {
    if (!session.parent_session_id) continue;
    result[session.parent_session_id] ??= [];
    result[session.parent_session_id].push(session);
  }
  return result;
}

export function groupWorkspaces(
  sessions: SessionInfo[],
  pinned: string[],
  query = "",
): Workspace[] {
  const needle = query.trim().toLowerCase();
  const groups = new Map<string, Workspace>();
  for (const cwd of pinned) groups.set(cwd, { cwd, pinned: true, sessions: [], updatedAt: 0 });
  for (const session of sessions) {
    if (session.parent_session_id) continue;
    if (
      needle &&
      !session.title.toLowerCase().includes(needle) &&
      !session.cwd.toLowerCase().includes(needle)
    ) {
      continue;
    }
    let group = groups.get(session.cwd);
    if (!group) {
      group = { cwd: session.cwd, pinned: false, sessions: [], updatedAt: 0 };
      groups.set(session.cwd, group);
    }
    group.sessions.push(session);
    group.updatedAt = Math.max(group.updatedAt, session.updated_at_ms);
  }
  return [...groups.values()]
    .filter(
      (group) => !needle || group.sessions.length > 0 || group.cwd.toLowerCase().includes(needle),
    )
    .sort((a, b) => b.updatedAt - a.updatedAt);
}

export function workspaceName(cwd: string): string {
  const parts = cwd.split(/[\\/]/).filter(Boolean);
  return parts.at(-1) ?? cwd;
}
