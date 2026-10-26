// 界面偏好：只影响前端展示，持久化到 localStorage，不发送给 core。

import { create } from "zustand";
import { persist } from "zustand/middleware";

export type Theme = "system" | "light" | "dark";
export type DetailTier = "hidden" | "brief" | "full";
export type RunningEnter = "steer" | "interrupt";
export type DiffMode = "unified" | "split";
export type RightTab = "plan" | "todo" | "changes" | "agents" | "context" | "btw";

export type Prefs = {
  theme: Theme;
  thinkingTier: DetailTier;
  toolTier: DetailTier;
  runningEnter: RunningEnter;
  notify: boolean;
  diffMode: DiffMode;
  rightPanel: { open: boolean; tab: RightTab; width: number };
  sidebarWidth: number;
  sidebarCollapsed: boolean;
};

type PrefsStore = Prefs & {
  set: (patch: Partial<Prefs>) => void;
  setRightPanel: (patch: Partial<Prefs["rightPanel"]>) => void;
};

export const TIER_ORDER: DetailTier[] = ["hidden", "brief", "full"];

export const usePrefs = create<PrefsStore>()(
  persist(
    (set) => ({
      theme: "system",
      thinkingTier: "brief",
      toolTier: "brief",
      runningEnter: "steer",
      notify: false,
      diffMode: "unified",
      rightPanel: { open: false, tab: "plan", width: 360 },
      sidebarWidth: 280,
      sidebarCollapsed: false,
      set: (patch) => set(patch),
      setRightPanel: (patch) => set((state) => ({ rightPanel: { ...state.rightPanel, ...patch } })),
    }),
    { name: "moc-web-prefs" },
  ),
);
