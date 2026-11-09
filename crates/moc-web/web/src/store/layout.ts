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

export type InspectorTab = "files" | "terminal" | "changes" | "btw";

export const INSPECTOR_TABS: InspectorTab[] = ["files", "terminal", "changes", "btw"];

type Persisted = {
  sidebarCollapsed: boolean;
  sidebarWidth: number;
  rightbarWidth: number;
  inspectorTab: InspectorTab;
  /** 文件树显示隐藏文件开关。 */
  filesShowHidden: boolean;
  /** 文件分页宽屏模式下的树宽（px）。 */
  filesTreeWidth: number;
};

const STORAGE_KEY = "moc-web-layout-v2";

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, Math.round(value)));
}

/** 右栏宽度上限：全局比例之外再给主舞台留 400px（扣掉当前可见侧栏）。 */
function rightbarMax(): number {
  const { sidebarCollapsed, sidebarWidth } = useLayout.getState();
  return Math.min(
    window.innerWidth * RIGHTBAR_MAX_RATIO,
    window.innerWidth - (sidebarCollapsed ? 0 : sidebarWidth) - 400,
  );
}

function load(): Persisted {
  const fallback: Persisted = {
    sidebarCollapsed: false,
    sidebarWidth: SIDEBAR_DEFAULT,
    rightbarWidth: RIGHTBAR_DEFAULT,
    inspectorTab: "files",
    filesShowHidden: false,
    filesTreeWidth: 240,
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
        : "files",
      filesShowHidden: Boolean(raw.filesShowHidden),
      filesTreeWidth: Math.min(360, Math.max(200, raw.filesTreeWidth ?? 240)),
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
  setFilesShowHidden: (show: boolean) => void;
  setFilesTreeWidth: (width: number) => void;
};

export const useLayout = create<LayoutState>()((set, get) => {
  const persist = () => {
    const {
      sidebarCollapsed,
      sidebarWidth,
      rightbarWidth,
      inspectorTab,
      filesShowHidden,
      filesTreeWidth,
    } = get();
    try {
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({
          sidebarCollapsed,
          sidebarWidth,
          rightbarWidth,
          inspectorTab,
          filesShowHidden,
          filesTreeWidth,
        }),
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
      set({ rightbarWidth: clamp(width, RIGHTBAR_MIN, rightbarMax()) });
      persist();
    },
    openInspector: (tab) => {
      set({ inspectorTab: tab, rightbarOpen: true });
      persist();
    },
    setFilesShowHidden: (show) => {
      set({ filesShowHidden: show });
      persist();
    },
    setFilesTreeWidth: (width) => {
      set({ filesTreeWidth: Math.min(360, Math.max(200, width)) });
      persist();
    },
  };
});

// 窗口变小后右栏重新钳上限（拖宽过又缩窗的场景）。
window.addEventListener("resize", () => {
  const state = useLayout.getState();
  if (state.rightbarWidth > rightbarMax()) state.setRightbarWidth(state.rightbarWidth);
});
