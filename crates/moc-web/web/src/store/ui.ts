// 全局浮层状态：对话框、命令面板、模型选择器。只在内存里，不持久化。

import { create } from "zustand";
import type { SessionInfo } from "@/protocol/types";

export type SettingsTarget = { tab?: string };

export type UiState = {
  newSession: { open: boolean; cwd?: string };
  folderPicker: { open: boolean; start?: string; onPick?: (path: string) => void };
  settings: { open: boolean; tab?: string };
  usage: string | null;
  plugins: string | null;
  contextContent: string | null;
  planReview: { sessionId: string; planId: string } | null;
  palette: boolean;
  help: boolean;
  /** 打开模型选择器的会话；由 composer 渲染。 */
  modelPicker: string | null;
  /** 重命名对话框目标会话。 */
  rename: SessionInfo | null;
  /** 删除确认对话框目标会话。 */
  confirmDelete: SessionInfo | null;
  /** 关闭运行中会话的确认。 */
  confirmClose: string | null;
  /** composer 聚焦请求计数，变化即聚焦当前会话的输入框。 */
  focusComposer: number;
  /** 旁路输入框聚焦请求计数（Ctrl+;）。 */
  focusBtw: number;
  set: (patch: Partial<Omit<UiState, "set">>) => void;
};

export const useUi = create<UiState>()((set) => ({
  newSession: { open: false },
  folderPicker: { open: false },
  settings: { open: false },
  usage: null,
  plugins: null,
  contextContent: null,
  planReview: null,
  palette: false,
  help: false,
  modelPicker: null,
  rename: null,
  confirmDelete: null,
  confirmClose: null,
  focusComposer: 0,
  focusBtw: 0,
  set: (patch) => set(patch),
}));

/** 有任何模态浮层打开时暂停队列派发（计划审阅、设置）。 */
export function dispatchPaused(state: UiState): boolean {
  return state.settings.open || state.planReview !== null;
}

export function requestComposerFocus(): void {
  useUi.setState((state) => ({ focusComposer: state.focusComposer + 1 }));
}
