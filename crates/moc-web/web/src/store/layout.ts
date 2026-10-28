// 布局与多栏面板状态：三列尺寸、折叠、右侧标签页抽屉（完全对齐 deepseek-harness 的 ui-layout 与 ui-sidebar-right）。

import { create } from "zustand";

export const SIDEBAR_MIN = 264;
export const SIDEBAR_MAX = 420;
export const SIDEBAR_DEFAULT = 280;
export const SIDEBAR_COLLAPSED = 56;
export const SIDEBAR_AUTO_COLLAPSE = 1024;

export const RIGHTBAR_MIN = 300;
export const RIGHTBAR_DEFAULT = 450;
export const RIGHTBAR_MAX_RATIO = 0.7;

export interface RightTab {
  id: string;
  kind:
    | "guide"
    | "files"
    | "doc"
    | "changes"
    | "plan"
    | "terminal"
    | "subagents"
    | "context"
    | "usage";
  title: string;
  path?: string;
  data?: unknown;
}

interface LayoutStorage {
  sidebarCollapsed: boolean;
  sidebarWidth: number;
  rightbarWidth: number;
  pinnedSessions: string[];
}

const STORAGE_KEY = "moc-web-layout-v1";

function loadStorage(): LayoutStorage {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      return {
        sidebarCollapsed: Boolean(parsed.sidebarCollapsed),
        sidebarWidth:
          typeof parsed.sidebarWidth === "number" ? parsed.sidebarWidth : SIDEBAR_DEFAULT,
        rightbarWidth:
          typeof parsed.rightbarWidth === "number" ? parsed.rightbarWidth : RIGHTBAR_DEFAULT,
        pinnedSessions: Array.isArray(parsed.pinnedSessions) ? parsed.pinnedSessions : [],
      };
    }
  } catch {
    // 损坏数据忽略
  }
  return {
    sidebarCollapsed: typeof window !== "undefined" && window.innerWidth < SIDEBAR_AUTO_COLLAPSE,
    sidebarWidth: SIDEBAR_DEFAULT,
    rightbarWidth: RIGHTBAR_DEFAULT,
    pinnedSessions: [],
  };
}

function saveStorage(state: LayoutStorage) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // 忽略存储失败
  }
}

const DEFAULT_TABS: RightTab[] = [
  { id: "guide", kind: "guide", title: "🧭 开始" },
  { id: "files", kind: "files", title: "工作区文件" },
  { id: "terminal", kind: "terminal", title: "新建终端" },
];

export interface LayoutState {
  sidebarCollapsed: boolean;
  sidebarWidth: number;
  rightbarOpen: boolean;
  rightbarWidth: number;
  rightbarFullscreen: boolean;
  openRightTabs: RightTab[];
  activeRightTabId: string;
  pinnedSessions: string[];

  toggleSidebar: () => void;
  setSidebarCollapsed: (collapsed: boolean) => void;
  setSidebarWidth: (width: number) => void;

  toggleRightbar: (open?: boolean) => void;
  setRightbarWidth: (width: number) => void;
  toggleRightbarFullscreen: () => void;

  openRightTab: (tab: {
    kind: RightTab["kind"];
    title?: string;
    path?: string;
    data?: unknown;
  }) => void;
  closeRightTab: (id: string) => void;
  activateRightTab: (id: string) => void;

  togglePinSession: (sessionId: string) => void;
  isSessionPinned: (sessionId: string) => boolean;
}

export const useLayout = create<LayoutState>()((set, get) => {
  const initial = loadStorage();

  const sync = () => {
    const { sidebarCollapsed, sidebarWidth, rightbarWidth, pinnedSessions } = get();
    saveStorage({ sidebarCollapsed, sidebarWidth, rightbarWidth, pinnedSessions });
  };

  return {
    sidebarCollapsed: initial.sidebarCollapsed,
    sidebarWidth: Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, initial.sidebarWidth)),
    // 与 DSH 一致：右栏默认收起，由顶栏按钮或具体入口按需打开
    rightbarOpen: false,
    rightbarWidth: Math.max(RIGHTBAR_MIN, initial.rightbarWidth),
    rightbarFullscreen: false,
    openRightTabs: DEFAULT_TABS,
    activeRightTabId: "guide",
    pinnedSessions: initial.pinnedSessions,

    toggleSidebar: () => {
      set((state) => ({ sidebarCollapsed: !state.sidebarCollapsed }));
      sync();
    },

    setSidebarCollapsed: (collapsed) => {
      set({ sidebarCollapsed: collapsed });
      sync();
    },

    setSidebarWidth: (width) => {
      const clamped = Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, Math.round(width)));
      set({ sidebarWidth: clamped });
      sync();
    },

    toggleRightbar: (open) => {
      set((state) => ({ rightbarOpen: open ?? !state.rightbarOpen }));
    },

    setRightbarWidth: (width) => {
      const max = typeof window !== "undefined" ? window.innerWidth * RIGHTBAR_MAX_RATIO : 800;
      const clamped = Math.max(RIGHTBAR_MIN, Math.min(max, Math.round(width)));
      set({ rightbarWidth: clamped });
      sync();
    },

    toggleRightbarFullscreen: () => {
      set((state) => ({ rightbarFullscreen: !state.rightbarFullscreen }));
    },

    openRightTab: ({ kind, title, path, data }) => {
      const { openRightTabs } = get();
      const id = kind === "doc" && path ? `doc:${path}` : kind;
      const existing = openRightTabs.find((t) => t.id === id);

      const resolvedTitle =
        title ??
        (kind === "doc" && path
          ? path.split("/").pop() || path
          : kind === "files"
            ? "文件"
            : kind === "changes"
              ? "变更"
              : kind === "plan"
                ? "计划"
                : kind === "terminal"
                  ? "终端"
                  : kind === "subagents"
                    ? "子代理"
                    : kind === "context"
                      ? "上下文"
                      : kind === "usage"
                        ? "用量"
                        : "指南");

      if (existing) {
        set({
          activeRightTabId: id,
          rightbarOpen: true,
        });
      } else {
        const newTab: RightTab = {
          id,
          kind,
          title: resolvedTitle,
          path,
          data,
        };
        set({
          openRightTabs: [...openRightTabs, newTab],
          activeRightTabId: id,
          rightbarOpen: true,
        });
      }
    },

    closeRightTab: (id) => {
      const { openRightTabs, activeRightTabId } = get();
      const index = openRightTabs.findIndex((t) => t.id === id);
      if (index < 0) return;

      const remaining = openRightTabs.filter((t) => t.id !== id);
      let nextActive = activeRightTabId;

      if (activeRightTabId === id) {
        if (remaining.length > 0) {
          const nextIndex = Math.min(index, remaining.length - 1);
          nextActive = remaining[nextIndex]?.id ?? "guide";
        } else {
          nextActive = "guide";
          remaining.push({ id: "guide", kind: "guide", title: "开始指南" });
        }
      }

      set({
        openRightTabs: remaining,
        activeRightTabId: nextActive,
      });
    },

    activateRightTab: (id) => {
      set({ activeRightTabId: id, rightbarOpen: true });
    },

    togglePinSession: (sessionId) => {
      set((state) => {
        const isPinned = state.pinnedSessions.includes(sessionId);
        const next = isPinned
          ? state.pinnedSessions.filter((id) => id !== sessionId)
          : [...state.pinnedSessions, sessionId];
        return { pinnedSessions: next };
      });
      sync();
    },

    isSessionPinned: (sessionId) => {
      return get().pinnedSessions.includes(sessionId);
    },
  };
});
