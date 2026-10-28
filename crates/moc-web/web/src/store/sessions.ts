// 会话列表与工作区管理。工作区 = 文件夹路径分组，对齐 deepseek-harness 交互规范。

import { create } from "zustand";
import { rpc } from "@/protocol/client";
import type { SessionInfo, SessionListResult, SessionMode } from "@/protocol/types";

const WORKSPACES_STORAGE_KEY = "dsh-workspaces-paths";
const LEGACY_PINNED_KEY = "moc-web-workspaces";

function loadWorkspaces(): string[] {
  try {
    const raw =
      localStorage.getItem(WORKSPACES_STORAGE_KEY) || localStorage.getItem(LEGACY_PINNED_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed)
      ? parsed.filter((item): item is string => typeof item === "string")
      : [];
  } catch {
    return [];
  }
}

function saveWorkspaces(list: string[]): void {
  try {
    localStorage.setItem(WORKSPACES_STORAGE_KEY, JSON.stringify(list));
  } catch {
    // ignore
  }
}

export type DraftSession = {
  id: string;
  cwd: string;
  title: string;
  isDraft: true;
  model?: string;
  mode?: SessionMode;
};

export type Workspace = {
  cwd: string;
  name: string;
  pinned: boolean;
  /** 顶层会话（子代理会话挂在父会话下） */
  sessions: SessionInfo[];
  updatedAt: number;
};

export type SessionsState = {
  list: SessionInfo[];
  /** 已打开但还不在 session/list 里的会话（首条消息落盘前） */
  extra: Record<string, SessionInfo>;
  pinned: string[];
  /** 用户添加或置顶的工作区文件夹路径列表 */
  workspaces: string[];
  /** 本地临时空白新会话（未发送首条提示词前不写后端，离开后自动丢弃） */
  draftSession: DraftSession | null;

  refresh: () => Promise<void>;
  upsert: (info: SessionInfo) => void;
  remove: (id: string) => void;
  pin: (cwd: string) => void;
  unpin: (cwd: string) => void;
  addWorkspace: (cwd: string) => void;
  removeWorkspace: (cwd: string) => void;
  setDraftSession: (draft: DraftSession | null) => void;
};

export const useSessions = create<SessionsState>()((set, get) => ({
  list: [],
  extra: {},
  pinned: loadWorkspaces(),
  workspaces: loadWorkspaces(),
  draftSession: null,

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
    const clean = cwd.trim().replace(/[\\/]+$/, "");
    if (!clean) return;
    const workspaces = get().workspaces.includes(clean)
      ? get().workspaces
      : [...get().workspaces, clean];
    saveWorkspaces(workspaces);
    set({ workspaces, pinned: workspaces });
  },

  unpin: (cwd) => {
    const clean = cwd.trim().replace(/[\\/]+$/, "");
    const workspaces = get().workspaces.filter((item) => item !== clean);
    saveWorkspaces(workspaces);
    set({ workspaces, pinned: workspaces });
  },

  addWorkspace: (cwd) => {
    const clean = cwd.trim().replace(/[\\/]+$/, "");
    if (!clean) return;
    const workspaces = get().workspaces.includes(clean)
      ? get().workspaces
      : [clean, ...get().workspaces];
    saveWorkspaces(workspaces);
    set({ workspaces, pinned: workspaces });
  },

  removeWorkspace: (cwd) => {
    const clean = cwd.trim().replace(/[\\/]+$/, "");
    const workspaces = get().workspaces.filter((item) => item !== clean);
    saveWorkspaces(workspaces);
    set({ workspaces, pinned: workspaces });
  },

  setDraftSession: (draft) => {
    set({ draftSession: draft });
  },
}));

/** 全部会话（list ∪ extra）。默认按更新时间降序；手动模式保留当前列表顺序。 */
export function allSessions(
  state: Pick<SessionsState, "list" | "extra">,
  order: "updated" | "manual" = "updated",
): SessionInfo[] {
  const sessions = [...Object.values(state.extra), ...state.list];
  return order === "manual" ? sessions : sessions.sort((a, b) => b.updated_at_ms - a.updated_at_ms);
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

/**
 * 将会话按照工作区（cwd 文件夹）进行分组，对齐 dsh 规则。
 * 保留用户添加的工作区，即使用户刚添加的工作区还没有会话也会列出。
 */
export function groupWorkspaces(
  sessions: SessionInfo[],
  workspaces: string[],
  query = "",
  order: "updated" | "manual" = "updated",
): Workspace[] {
  const needle = query.trim().toLowerCase();
  const groups = new Map<string, Workspace>();

  // 1. 先置入已添加的工作区
  for (const cwd of workspaces) {
    if (!cwd) continue;
    groups.set(cwd, {
      cwd,
      name: workspaceName(cwd),
      pinned: true,
      sessions: [],
      updatedAt: 0,
    });
  }

  // 2. 将会话归类到各自的 cwd 下
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
      group = {
        cwd: session.cwd,
        name: workspaceName(session.cwd),
        pinned: false,
        sessions: [],
        updatedAt: 0,
      };
      groups.set(session.cwd, group);
    }
    group.sessions.push(session);
    group.updatedAt = Math.max(group.updatedAt, session.updated_at_ms);
  }

  // 3. 对各个工作区内部的会话按更新时间降序排列
  for (const group of groups.values()) {
    if (order === "updated") group.sessions.sort((a, b) => b.updated_at_ms - a.updated_at_ms);
  }

  // 4. 返回符合搜索条件的工作区列表
  const result = [...groups.values()].filter(
    (group) => !needle || group.sessions.length > 0 || group.cwd.toLowerCase().includes(needle),
  );
  return order === "updated" ? result.sort((a, b) => b.updatedAt - a.updatedAt) : result;
}

export function workspaceName(cwd: string): string {
  const parts = cwd.split(/[\\/]/).filter(Boolean);
  return parts.at(-1) ?? cwd;
}
