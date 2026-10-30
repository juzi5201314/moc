// 会话列表与工作区：工作区 = 会话 cwd 分组，外加用户手动添加的目录。
// 手动目录属于 webui 前端状态，存在 core 侧 web.toml，由 store/prefs.ts 维护。

import { create } from "zustand";
import { rpc } from "@/protocol/client";
import type {
  SessionAutonomy,
  SessionEngine,
  SessionInfo,
  SessionListResult,
  SessionPermission,
} from "@/protocol/types";

/** 本地草稿会话上用户显式选过的参数；未选的项交给 core 默认值。 */
export type DraftSession = {
  cwd: string;
  model?: string;
  engine: SessionEngine;
  autonomy: SessionAutonomy;
  plan: boolean;
  permission?: SessionPermission;
};

export type Workspace = {
  cwd: string;
  name: string;
  /** 顶层会话（子代理会话挂在父会话的检查面板里） */
  sessions: SessionInfo[];
  updatedAt: number;
};

export type SessionsState = {
  list: SessionInfo[];
  /** 已打开但还不在 session/list 里的会话（首条消息落盘前） */
  extra: Record<string, SessionInfo>;
  /** 本地空白新会话（未发送首条提示前不写 core，离开即丢弃） */
  draftSession: DraftSession | null;

  refresh: () => Promise<void>;
  upsert: (info: SessionInfo) => void;
  remove: (id: string) => void;
  setDraftSession: (draft: DraftSession | null) => void;
};

export const useSessions = create<SessionsState>()((set, get) => ({
  list: [],
  extra: {},
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

  setDraftSession: (draft) => set({ draftSession: draft }),
}));

/** 全部会话（list ∪ extra），按更新时间降序。 */
export function allSessions(state: Pick<SessionsState, "list" | "extra">): SessionInfo[] {
  return [...Object.values(state.extra), ...state.list].sort(
    (a, b) => b.updated_at_ms - a.updated_at_ms,
  );
}

/**
 * 按 cwd 分组。手动添加的目录与草稿所在目录即使还没有会话也列出；
 * 搜索时只保留标题或路径命中的会话。工作区按最近活动降序。
 */
export function groupWorkspaces(
  sessions: SessionInfo[],
  extraDirs: string[],
  query = "",
): Workspace[] {
  const needle = query.trim().toLowerCase();
  const groups = new Map<string, Workspace>();
  const ensure = (cwd: string) => {
    let group = groups.get(cwd);
    if (!group) {
      group = { cwd, name: workspaceName(cwd), sessions: [], updatedAt: 0 };
      groups.set(cwd, group);
    }
    return group;
  };

  for (const cwd of extraDirs) if (cwd) ensure(cwd);
  for (const session of sessions) {
    if (session.parent_session_id) continue;
    if (
      needle &&
      !session.title.toLowerCase().includes(needle) &&
      !session.cwd.toLowerCase().includes(needle)
    ) {
      continue;
    }
    const group = ensure(session.cwd);
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
  return cwd.split(/[\\/]/).filter(Boolean).at(-1) ?? cwd;
}
