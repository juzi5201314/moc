import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n } from "@/lib/i18n";
import { createView } from "@/lib/transcript-model";
import type { ContextUsageResult, SessionInfo } from "@/protocol/types";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

const { triggerSessionCompact, refreshContext } = vi.hoisted(() => ({
  triggerSessionCompact: vi.fn(() => Promise.resolve()),
  refreshContext: vi.fn(() => Promise.resolve()),
}));

vi.mock("@/store/lifecycle", () => ({ triggerSessionCompact }));
vi.mock("@/store/transcript", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/store/transcript")>()),
  refreshContext,
}));

import { ContextRing } from "./status-popovers";

const SESSION: SessionInfo = {
  id: "s1",
  title: "Test",
  cwd: "/repo",
  model: "p/m",
  permission: "full",
  autonomy: "default",
  engine: "standard",
  fast_mode: false,
  plan_active: false,
  created_at_ms: 0,
  updated_at_ms: 0,
};

const CONTEXT: ContextUsageResult = {
  context_window: 100_000,
  total_tokens: 80_000,
  sections: [
    { name: "system", tokens: 10_000 },
    { name: "messages", tokens: 70_000 },
  ],
};

const initialUi = useUi.getState();
const initialLocale = useI18n.getState().locale;

function renderRing(sessionId = "s1") {
  return render(
    <TooltipProvider>
      <ContextRing sessionId={sessionId} context={CONTEXT} />
    </TooltipProvider>,
  );
}

beforeEach(() => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  useI18n.setState({ locale: "zh" });
  useUi.setState(initialUi);
  useTranscript.setState({ bySession: { s1: createView(SESSION) } });
  triggerSessionCompact.mockClear();
  refreshContext.mockClear();
});

afterEach(() => {
  cleanup();
  useUi.setState(initialUi);
  useI18n.setState({ locale: initialLocale });
  useTranscript.setState({ bySession: {} });
  vi.unstubAllGlobals();
});

it("点击 Compact 在浮层原地切确认态，取消返回总览，确认带聚焦说明触发并关闭", async () => {
  const ui = renderRing();
  fireEvent.click(ui.getByRole("button", { name: "Context 80%" }));

  // 点击底部 Compact 不直接发起 RPC，而是原地切换到确认视图。
  fireEvent.click(ui.getByRole("button", { name: "Compact" }));
  expect(triggerSessionCompact).not.toHaveBeenCalled();
  expect(ui.getByText("Compact context")).toBeTruthy();

  // 取消回到总览态，不关闭浮层。
  fireEvent.click(ui.getByRole("button", { name: "Cancel" }));
  expect(ui.getByRole("button", { name: "Details" })).toBeTruthy();
  expect(triggerSessionCompact).not.toHaveBeenCalled();

  // 再次进入确认态、输入聚焦说明后确认。
  fireEvent.click(ui.getByRole("button", { name: "Compact" }));
  const input = ui.getByRole("textbox", { name: "压缩聚焦说明" });
  fireEvent.change(input, { target: { value: "保留接口约定" } });
  fireEvent.click(ui.getByRole("button", { name: "Compact" }));

  expect(triggerSessionCompact).toHaveBeenCalledTimes(1);
  expect(triggerSessionCompact).toHaveBeenCalledWith("s1", "保留接口约定");
  await waitFor(() => expect(ui.queryByText("Compact context")).toBeNull());
});

it("运行中、只读与压缩中时禁用 Compact 入口", () => {
  const view = createView(SESSION);
  view.running = true;
  useTranscript.setState({ bySession: { s1: view } });

  const ui = renderRing();
  fireEvent.click(ui.getByRole("button", { name: "Context 80%" }));
  expect(ui.getByRole("button", { name: "Compact" }).hasAttribute("disabled")).toBe(true);

  const readOnlyView = createView(SESSION);
  readOnlyView.readOnly = true;
  act(() => {
    useTranscript.setState({ bySession: { s1: readOnlyView } });
  });
  expect(ui.getByRole("button", { name: "Compact" }).hasAttribute("disabled")).toBe(true);

  act(() => {
    useTranscript.setState({ bySession: { s1: createView(SESSION) } });
    useUi.setState({ compactingSessions: { s1: true } });
  });
  expect(ui.getByRole("button", { name: "Compacting…" }).hasAttribute("disabled")).toBe(true);
});
