// 全局快捷键。Alt 组合用 event.code 判断：macOS 上 Option 会改写 event.key。
// 避开浏览器保留的 Ctrl+N/T/W/Tab；Esc 由 composer 和各浮层自己处理。

import { useEffect } from "react";
import { toast } from "sonner";
import { setMode } from "@/components/composer/commands";
import { runAction } from "@/components/layout/session-actions";
import type { SessionMode } from "@/protocol/types";
import { closeSession } from "@/store/lifecycle";
import { type DetailTier, TIER_ORDER, usePrefs } from "@/store/prefs";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

const MODE_CYCLE: SessionMode[] = ["normal", "ask", "plan", "ptc"];
const MODE_LABEL: Record<string, string> = {
  normal: "标准",
  ask: "问答",
  plan: "计划",
  ptc: "PTC",
};
const TIER_LABEL: Record<DetailTier, string> = { hidden: "隐藏", brief: "简要", full: "完整" };

function nextTier(tier: DetailTier): DetailTier {
  return TIER_ORDER[(TIER_ORDER.indexOf(tier) + 1) % TIER_ORDER.length] ?? "brief";
}

function cycleMode(sessionId: string): void {
  const view = useTranscript.getState().bySession[sessionId];
  if (!view || view.readOnly) return;
  if (view.info.mode === "clm_experimental") {
    toast("CLM 模式下不能切换模式");
    return;
  }
  const index = MODE_CYCLE.indexOf(view.info.mode);
  const next = MODE_CYCLE[(index + 1) % MODE_CYCLE.length] ?? "normal";
  void runAction(() => setMode(sessionId, next), `模式：${MODE_LABEL[next]}`);
}

function switchTab(offset: number): void {
  const { open, active, activate } = useTabs.getState();
  if (open.length === 0) return;
  const index = active ? open.indexOf(active) : 0;
  const next = open[(index + offset + open.length) % open.length];
  if (next) activate(next);
}

/** 返回 true 表示已处理（调用方 preventDefault）。 */
function handleAlt(code: string): boolean {
  const ui = useUi.getState();
  const prefs = usePrefs.getState();
  const tabs = useTabs.getState();
  const active = tabs.active;
  if (/^Digit[1-9]$/.test(code)) {
    const target = tabs.open[Number(code.slice(5)) - 1];
    if (target) tabs.activate(target);
    return true;
  }
  switch (code) {
    case "KeyN": {
      const cwd = active ? useTranscript.getState().bySession[active]?.info.cwd : undefined;
      ui.set({ newSession: { open: true, cwd } });
      return true;
    }
    case "KeyW": {
      if (!active) return true;
      const view = useTranscript.getState().bySession[active];
      if (view?.running && !view.readOnly) ui.set({ confirmClose: active });
      else void runAction(() => closeSession(active));
      return true;
    }
    case "BracketLeft":
      switchTab(-1);
      return true;
    case "BracketRight":
      switchTab(1);
      return true;
    case "KeyS":
      prefs.set({ sidebarCollapsed: !prefs.sidebarCollapsed });
      return true;
    case "KeyR":
      prefs.setRightPanel({ open: !prefs.rightPanel.open });
      return true;
    case "KeyP":
      if (active) ui.set({ modelPicker: active });
      return true;
    case "KeyM":
      if (active) cycleMode(active);
      return true;
    case "KeyT": {
      const thinkingTier = nextTier(prefs.thinkingTier);
      prefs.set({ thinkingTier });
      toast(`思考显示：${TIER_LABEL[thinkingTier]}`);
      return true;
    }
    case "KeyO": {
      const toolTier = nextTier(prefs.toolTier);
      prefs.set({ toolTier });
      toast(`工具显示：${TIER_LABEL[toolTier]}`);
      return true;
    }
    default:
      return false;
  }
}

function handleCtrl(code: string): boolean {
  const ui = useUi.getState();
  switch (code) {
    case "KeyK":
      ui.set({ palette: !ui.palette });
      return true;
    case "Slash":
      ui.set({ help: !ui.help });
      return true;
    case "Semicolon":
      usePrefs.getState().setRightPanel({ open: true, tab: "btw" });
      useUi.setState((state) => ({ focusBtw: state.focusBtw + 1 }));
      return true;
    default:
      return false;
  }
}

export function useGlobalShortcuts(): void {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.isComposing || event.repeat) return;
      const ctrl = event.ctrlKey || event.metaKey;
      let handled = false;
      if (event.altKey && !ctrl && !event.shiftKey) handled = handleAlt(event.code);
      else if (ctrl && !event.altKey && !event.shiftKey) handled = handleCtrl(event.code);
      if (handled) event.preventDefault();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);
}
