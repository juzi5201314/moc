// 全局浮层状态：对话框、命令面板、模型选择器。只在内存里，不持久化。

import { create } from "zustand";

export type SettingsSection =
  | "general"
  | "providers"
  | "roles"
  | "instructions"
  | "skills"
  | "tools"
  | "sandbox"
  | "context"
  | "session"
  | "mcp"
  | "plugins"
  | "advanced"
  | "shortcuts";

export type UiState = {
  folderPicker: { open: boolean; start?: string; onPick?: (path: string) => void };
  settings: { open: boolean; section?: SettingsSection };
  palette: boolean;
  /** 打开模型选择器的会话；由 composer 渲染。 */
  modelPicker: string | null;
  /** 删除确认对话框的目标会话。 */
  confirmDelete: { id: string; title: string } | null;
  /** composer 聚焦请求计数，变化即聚焦当前会话的输入框。 */
  focusComposer: number;
  /** 旁路输入框聚焦请求计数。 */
  focusBtw: number;
  set: (patch: Partial<Omit<UiState, "set">>) => void;
};

export const useUi = create<UiState>()((set) => ({
  folderPicker: { open: false },
  settings: { open: false },
  palette: false,
  modelPicker: null,
  confirmDelete: null,
  focusComposer: 0,
  focusBtw: 0,
  set: (patch) => set(patch),
}));

/** 有模态浮层打开时暂停队列派发（设置）。 */
export function dispatchPaused(state: UiState): boolean {
  return state.settings.open;
}

export function requestComposerFocus(): void {
  useUi.setState((state) => ({ focusComposer: state.focusComposer + 1 }));
}

export function openSettings(section?: SettingsSection): void {
  useUi.setState({ settings: { open: true, section } });
}
