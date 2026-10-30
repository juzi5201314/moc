// 界面偏好：只影响前端展示，持久化到 localStorage，不发送给 core。

import { create } from "zustand";
import { persist } from "zustand/middleware";

export type Theme = "system" | "light" | "dark";
/** 运行中按 Enter：插队补充到当前提示（steer），或中断后作为下一条提示（interrupt）。 */
export type RunningEnter = "steer" | "interrupt";
export type DiffMode = "unified" | "split";

export type Prefs = {
  theme: Theme;
  runningEnter: RunningEnter;
  notify: boolean;
  diffMode: DiffMode;
  /** 思考过程默认展开。 */
  expandThinking: boolean;
};

type PrefsStore = Prefs & {
  set: (patch: Partial<Prefs>) => void;
};

export const usePrefs = create<PrefsStore>()(
  persist(
    (set) => ({
      theme: "system",
      runningEnter: "steer",
      notify: false,
      diffMode: "unified",
      expandThinking: false,
      set: (patch) => set(patch),
    }),
    {
      name: "moc-web-prefs",
      version: 2,
      migrate: (persisted) => {
        const old = (persisted ?? {}) as Partial<Prefs>;
        return {
          theme: old.theme ?? "system",
          runningEnter: old.runningEnter ?? "steer",
          notify: old.notify ?? false,
          diffMode: old.diffMode ?? "unified",
          expandThinking: false,
        } as PrefsStore;
      },
    },
  ),
);
