// 三栏布局：左侧工作区栏宽度/折叠，右侧检查面板开关/宽度/当前页，持久化到 localStorage。

import { create } from "zustand";

export const SIDEBAR_MIN = 232;
export const SIDEBAR_MAX = 400;
export const SIDEBAR_DEFAULT = 264;
/** 窗口窄于此值时侧栏改为浮层，主舞台不被挤压。 */
export const NARROW_VIEWPORT = 960;

export const RIGHTBAR_MIN = 320;
export const RIGHTBAR_DEFAULT = 420;
export const RIGHTBAR_MAX_RATIO = 0.6;

export type InspectorTab = "changes" | "plan" | "todo" | "agents" | "context" | "usage" | "btw";

export const INSPECTOR_TABS: InspectorTab[] = [
  "changes",
  "plan",
  "todo",
  "agents",
  "context",
  "usage",
  "btw",
];

type Persisted = {
  sidebarCollapsed: boolean;
  sidebarWidth: number;
  rightbarWidth: number;
  inspectorTab: InspectorTab;
};

const STORAGE_KEY = "moc-web-layout-v2";

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, Math.round(value)));
}

function load(): Persisted {
  const fallback: Persisted = {
    sidebarCollapsed: false,
    sidebarWidth: SIDEBAR_DEFAULT,
    rightbarWidth: RIGHTBAR_DEFAULT,
    inspectorTab: "changes",
  };
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
    if (!parsed || typeof parsed !== "object") return fallback;
    const raw = parsed as Partial<Persisted>;
    return {
      sidebarCollapsed: Boolean(raw.sidebarCollapsed),
      sidebarWidth: clamp(raw.sidebarWidth ?? SIDEBAR_DEFAULT, SIDEBAR_MIN, SIDEBAR_MAX),
      rightbarWidth: Math.max(RIGHTBAR_MIN, raw.rightbarWidth ?? RIGHTBAR_DEFAULT),
      inspectorTab: INSPECTOR_TABS.includes(raw.inspectorTab as InspectorTab)
        ? (raw.inspectorTab as InspectorTab)
        : "changes",
    };
  } catch {
    return fallback;
  }
}

export type LayoutState = Persisted & {
  rightbarOpen: boolean;
  /** 窄窗口下侧栏以浮层展开（不持久化）。 */
  sidebarOverlay: boolean;
  toggleSidebar: () => void;
  setSidebarOverlay: (open: boolean) => void;
  setSidebarWidth: (width: number) => void;
  toggleRightbar: (open?: boolean) => void;
  setRightbarWidth: (width: number) => void;
  /** 打开右侧面板并切到指定页。 */
  openInspector: (tab: InspectorTab) => void;
};

export const useLayout = create<LayoutState>()((set, get) => {
  const persist = () => {
    const { sidebarCollapsed, sidebarWidth, rightbarWidth, inspectorTab } = get();
    try {
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({ sidebarCollapsed, sidebarWidth, rightbarWidth, inspectorTab }),
      );
    } catch {
      // 隐私模式等写不进去时只影响下次打开的布局
    }
  };
  return {
    ...load(),
    rightbarOpen: false,
    sidebarOverlay: false,
    toggleSidebar: () => {
      if (window.innerWidth < NARROW_VIEWPORT) {
        set((state) => ({ sidebarOverlay: !state.sidebarOverlay }));
        return;
      }
      set((state) => ({ sidebarCollapsed: !state.sidebarCollapsed }));
      persist();
    },
    setSidebarOverlay: (open) => set({ sidebarOverlay: open }),
    setSidebarWidth: (width) => {
      set({ sidebarWidth: clamp(width, SIDEBAR_MIN, SIDEBAR_MAX) });
      persist();
    },
    toggleRightbar: (open) => set((state) => ({ rightbarOpen: open ?? !state.rightbarOpen })),
    setRightbarWidth: (width) => {
      set({
        rightbarWidth: clamp(width, RIGHTBAR_MIN, window.innerWidth * RIGHTBAR_MAX_RATIO),
      });
      persist();
    },
    openInspector: (tab) => {
      set({ inspectorTab: tab, rightbarOpen: true });
      persist();
    },
  };
});
