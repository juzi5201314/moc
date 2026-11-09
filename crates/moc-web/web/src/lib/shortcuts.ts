// 全局快捷键。Alt 组合用 event.code 判断：macOS 上 Option 会改写 event.key。
// 与 TUI 同义的功能必须用 TUI 的同一个键；浏览器保留、拦不住的键（Ctrl+N/T/W/Tab）
// 宁可不做，只走鼠标——例如 TUI 的 Ctrl+T（思考档位）在这里没有快捷键。
// Esc 由 composer 和各浮层自己处理。
// 键位表同时供设置页「快捷键」分区展示，改动只需改这一处。

import { useEffect } from "react";
import { toast } from "sonner";
import { LEVEL_LABEL, NEXT_LEVEL } from "@/lib/tool-view";
import type { SessionEngine } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useLayout } from "@/store/layout";
import { createQuickSession, setSessionEngine, togglePlanMode } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { openSettings, requestComposerFocus, useUi } from "@/store/ui";

export const SHORTCUTS: { keys: string; label: string }[] = [
  { keys: "Ctrl K", label: "命令面板" },
  { keys: "Alt N", label: "新会话" },
  { keys: "Ctrl B", label: "展开 / 收起侧栏" },
  { keys: "Alt R", label: "展开 / 收起检查面板" },
  { keys: "Ctrl `", label: "打开 / 收起终端分页" },
  { keys: "Alt P", label: "切换模型" },
  { keys: "Alt M", label: "切换会话引擎（首条消息前）" },
  { keys: "Alt J", label: "进入 / 退出计划状态" },
  { keys: "Ctrl O", label: "切换工具显示档位（隐藏 / 简略 / 完整）" },
  { keys: "Ctrl /", label: "查看快捷键" },
  { keys: "Enter", label: "发送；生成中按偏好补充或中断" },
  { keys: "Ctrl Enter", label: "生成中排队发送" },
  { keys: "Shift Enter", label: "换行" },
  { keys: "Esc", label: "生成中且输入为空时停止" },
];

const ENGINE_CYCLE: SessionEngine[] = ["standard", "ptc", "clm_experimental"];
const ENGINE_LABEL: Record<SessionEngine, string> = {
  standard: "标准",
  ptc: "PTC",
  clm_experimental: "CLM（实验）",
};
// 引擎与特性 flag 的对应（与 core 的 flag_for_engine 同一张事实）。
const ENGINE_FLAG: Partial<Record<SessionEngine, string>> = {
  ptc: "ptc",
  clm_experimental: "clm",
};

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function activeView() {
  const active = useTabs.getState().active;
  const view = active ? useTranscript.getState().bySession[active] : undefined;
  return active && view && !view.readOnly ? { id: active, view } : null;
}

/** 切换会话引擎；引擎已锁定时只提示。`/clm-experimental` 与循环切换共用。 */
export function applySessionEngine(sessionId: string, engine: SessionEngine): void {
  const view = useTranscript.getState().bySession[sessionId];
  if (view?.info.engine_locked) {
    toast("会话引擎已锁定；换引擎请新建会话");
    return;
  }
  void setSessionEngine(sessionId, engine)
    .then(() => toast(`引擎：${ENGINE_LABEL[engine]}`))
    .catch((error) => toast.error(`切换失败：${errorText(error)}`));
}

/** 循环到下一个特性可用的引擎；flag 关闭的引擎跳过（与 TUI `/engine` 一致）。 */
export function cycleSessionEngine(sessionId: string): void {
  const features = useConnection.getState().config?.features;
  const allowed = ENGINE_CYCLE.filter(
    (engine) => !ENGINE_FLAG[engine] || features?.[ENGINE_FLAG[engine]] !== false,
  );
  const engine = useTranscript.getState().bySession[sessionId]?.info.engine;
  const index = allowed.indexOf(engine ?? "standard");
  applySessionEngine(sessionId, allowed[(index + 1) % allowed.length] ?? "standard");
}

function cycleEngine(): void {
  const target = activeView();
  if (target) cycleSessionEngine(target.id);
}

export function cyclePlan(): void {
  const target = activeView();
  if (!target) return;
  const next = !target.view.info.plan_active;
  togglePlanMode(target.id, next)
    .then(() => toast(next ? "已进入计划状态" : "已退出计划状态"))
    .catch((error) => toast.error(`切换失败：${errorText(error)}`));
}

/** 返回 true 表示已处理（调用方 preventDefault）。 */
function handleAlt(code: string): boolean {
  switch (code) {
    case "KeyN": {
      const active = useTabs.getState().active;
      const cwd = active ? useTranscript.getState().bySession[active]?.info.cwd : undefined;
      createQuickSession(cwd);
      requestComposerFocus();
      return true;
    }
    case "KeyR":
      useLayout.getState().toggleRightbar();
      return true;
    case "KeyP": {
      const target = activeView();
      if (target) useUi.getState().set({ modelPicker: target.id });
      return true;
    }
    case "KeyM":
      cycleEngine();
      return true;
    case "KeyJ":
      cyclePlan();
      return true;
    default:
      return false;
  }
}

function handleCtrl(code: string): boolean {
  switch (code) {
    case "KeyK": {
      const ui = useUi.getState();
      ui.set({ palette: !ui.palette });
      return true;
    }
    case "KeyB":
      useLayout.getState().toggleSidebar();
      return true;
    case "Backquote": {
      const layout = useLayout.getState();
      if (layout.rightbarOpen && layout.inspectorTab === "terminal") {
        layout.toggleRightbar(false);
      } else {
        layout.openInspector("terminal");
      }
      return true;
    }
    case "Slash":
      openSettings("shortcuts");
      return true;
    case "KeyO": {
      const toolDetail = NEXT_LEVEL[usePrefs.getState().toolDetail];
      usePrefs.getState().set({ toolDetail });
      toast(`工具显示：${LEVEL_LABEL[toolDetail]}`);
      return true;
    }
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
