import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ContextContentResult, ContextNode } from "@/protocol/types";
import { useUi } from "@/store/ui";

const { call, on } = vi.hoisted(() => ({ call: vi.fn(), on: vi.fn() }));
vi.mock("@/protocol/client", () => ({ rpc: { call, on } }));
vi.mock("@/store/connection", () => ({ useConnection: { subscribe: () => () => {} } }));
vi.mock("@/store/lifecycle", () => ({ compactSession: vi.fn() }));
vi.mock("@/store/transcript", () => ({ useTranscript: () => false }));

import { ContextDialog } from "./context-dialog";
import { ContextViewer } from "./context-viewer";

const category: ContextNode = {
  id: "0",
  name: "messages",
  kind: "category",
  tokens: 100,
  children: 2,
};
const visible: ContextNode = {
  id: "1",
  name: "运行时提醒",
  kind: "markdown",
  tokens: 20,
  children: 0,
};
const hidden: ContextNode = {
  id: "2",
  name: "工具结果",
  kind: "markdown",
  tokens: 80,
  children: 0,
  hidden: "工具输出已隐藏",
};
const snapshot: ContextContentResult = {
  view_id: "v1",
  model: "test/model",
  context_window: 1000,
  total_tokens: 100,
  sections: [category],
};

beforeEach(() => {
  call.mockReset();
  on.mockReset().mockReturnValue(() => {});
  call.mockImplementation(async (method: string, params: { node_id?: string }) => {
    if (method === "session/context_content") return snapshot;
    if (method === "session/context_read")
      return {
        view_id: "v1",
        node_id: params.node_id,
        children: params.node_id === "0" ? [visible, hidden] : [],
        text: params.node_id === "1" ? "提醒正文" : null,
      };
    return {};
  });
});
afterEach(cleanup);

it("隐藏项无法进入，阅读后返回恢复目录焦点，更新不替换当前快照", async () => {
  const ui = render(<ContextViewer sessionId="s1" />);
  fireEvent.click(await ui.findByText("messages"));
  const hiddenButton = await ui.findByRole("button", { name: /工具结果/ });
  expect(hiddenButton.hasAttribute("disabled")).toBe(true);
  fireEvent.click(hiddenButton);
  expect(
    call.mock.calls.some(
      ([method, params]) => method === "session/context_read" && params.node_id === "2",
    ),
  ).toBe(false);
  fireEvent.click(ui.getByText("运行时提醒"));
  await ui.findByText("提醒正文");
  const notify = on.mock.calls[0][1];
  act(() => notify({ session_id: "s1", update: { type: "prompt_finished" } }));
  expect(ui.getByLabelText("有更新，刷新上下文")).toBeTruthy();
  expect(ui.getByText("提醒正文")).toBeTruthy();
  expect(call.mock.calls.filter(([method]) => method === "session/context_content")).toHaveLength(
    1,
  );
  fireEvent.click(ui.getByLabelText("返回上一级"));
  expect(ui.queryByText("提醒正文")).toBeNull();
  expect(document.activeElement).toBe(ui.getByRole("button", { name: /运行时提醒/ }));
  fireEvent.click(ui.getByLabelText("有更新，刷新上下文"));
  await ui.findByText("messages");
  expect(call).toHaveBeenCalledWith("session/context_close", { session_id: "s1", view_id: "v1" });
  ui.unmount();
});

it("关闭前尚未完成的快照返回后立即释放，不污染其它会话", async () => {
  const { promise, resolve } = Promise.withResolvers<ContextContentResult>();
  call.mockImplementation((method: string) =>
    method === "session/context_content" ? promise : Promise.resolve({}),
  );
  const ui = render(<ContextViewer sessionId="s1" />);
  ui.unmount();
  await act(async () => resolve(snapshot));
  await waitFor(() =>
    expect(call).toHaveBeenCalledWith("session/context_close", { session_id: "s1", view_id: "v1" }),
  );
});

it("Esc 逐层返回，只在总览层关闭统一面板", async () => {
  useUi.getState().set({ contextDialog: "s1" });
  const ui = render(<ContextDialog />);
  fireEvent.click(await ui.findByText("messages"));
  fireEvent.click(await ui.findByText("运行时提醒"));
  await ui.findByText("提醒正文");
  fireEvent.keyDown(document, { key: "Escape" });
  await waitFor(() => expect(ui.queryByText("提醒正文")).toBeNull());
  expect(ui.getByRole("dialog")).toBeTruthy();
  expect(ui.getByRole("button", { name: /工具结果/ })).toBeTruthy();
  fireEvent.keyDown(document, { key: "Escape" });
  await ui.findByText("messages");
  expect(ui.getByRole("dialog")).toBeTruthy();
  fireEvent.keyDown(document, { key: "Escape" });
  await waitFor(() => expect(ui.queryByRole("dialog")).toBeNull());
});
