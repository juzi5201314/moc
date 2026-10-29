// 全局快捷键。Alt 组合用 event.code 判断：macOS 上 Option 会改写 event.key。
// 避开浏览器保留的 Ctrl+N/T/W/Tab；Esc 由 composer 和各浮层自己处理。

import { useEffect } from "react";
import { toast } from "sonner";
import { rpc } from "@/protocol/client";
import type { SessionMode } from "@/protocol/types";
import { useLayout } from "@/store/layout";
import { closeSession, createQuickSession, togglePlanMode } from "@/store/lifecycle";
import { type DetailTier, TIER_ORDER, usePrefs } from "@/store/prefs";
import { useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

async function setMode(sessionId: string, mode: SessionMode): Promise<void> {
  if (sessionId === "draft-session") {
    useTranscript.getState().mutate(sessionId, (view) => {
      view.info.mode = mode;
    });
    const draft = useSessions.getState().draftSession;
    if (draft) useSessions.getState().setDraftSession({ ...draft, mode });
    return;
  }
  await rpc.call("session/set_mode", { session_id: sessionId, mode });
}

async function runAction(fn: () => Promise<unknown>, label?: string): Promise<void> {
  try {
    await fn();
    if (label) toast(label);
  } catch (err) {
    toast.error(err instanceof Error ? err.message : String(err));
  }
}

const MODE_CYCLE: SessionMode[] = ["normal", "ptc", "clm_experimental"];
const MODE_LABEL: Record<string, string> = {
  normal: "标准",
  ptc: "PTC",
  clm_experimental: "CLM 实验",
};
const TIER_LABEL: Record<DetailTier, string> = { hidden: "隐藏", brief: "简要", full: "完整" };

function nextTier(tier: DetailTier): DetailTier {
  return TIER_ORDER[(TIER_ORDER.indexOf(tier) + 1) % TIER_ORDER.length] ?? "brief";
}

function cycleMode(sessionId: string): void {
  const view = useTranscript.getState().bySession[sessionId];
  if (!view || view.readOnly) return;
  if (view.info.mode_locked) {
    toast("会话方言已锁定；换方言请新建会话");
    return;
  }
  const index = MODE_CYCLE.indexOf(view.info.mode);
  const next = MODE_CYCLE[(index + 1) % MODE_CYCLE.length] ?? "normal";
  void runAction(() => setMode(sessionId, next), `方言：${MODE_LABEL[next]}`);
}

function cyclePlan(sessionId: string): void {
  const view = useTranscript.getState().bySession[sessionId];
  if (!view || view.readOnly) return;
  const next = !view.info.plan_active;
  void runAction(() => togglePlanMode(sessionId, next), next ? "已进入计划状态" : "已退出计划状态");
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
      void createQuickSession(cwd);
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
    case "KeyB":
      useLayout.getState().toggleSidebar();
      return true;
    case "KeyR":
      useLayout.getState().toggleRightbar();
      return true;
    case "KeyP":
      if (active) ui.set({ modelPicker: active });
      return true;
    case "KeyM":
      if (active) cycleMode(active);
      return true;
    case "KeyJ":
      if (active) cyclePlan(active);
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
    case "KeyB":
      useLayout.getState().toggleSidebar();
      return true;
    case "KeyN": {
      const active = useTabs.getState().active;
      const cwd = active ? useTranscript.getState().bySession[active]?.info.cwd : undefined;
      void createQuickSession(cwd);
      return true;
    }
    case "Slash":
      ui.set({ help: !ui.help });
      return true;
    case "Semicolon":
      useLayout.getState().openRightTab({ kind: "terminal", title: "终端" });
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
