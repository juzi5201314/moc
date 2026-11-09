import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ContextContentResult, ContextNode } from "@/protocol/types";

const { call } = vi.hoisted(() => ({ call: vi.fn() }));
vi.mock("@/protocol/client", () => ({ rpc: { call } }));
// jsdom 没有布局；用固定视口呈现前12行，验证组件只请求展开节点。
vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: (options: { count: number; getItemKey: (index: number) => string }) => ({
    getTotalSize: () => options.count * 54,
    measureElement: () => {},
    getVirtualItems: () =>
      Array.from({ length: Math.min(12, options.count) }, (_, index) => ({
        index,
        key: options.getItemKey(index),
        start: index * 54,
      })),
  }),
}));

import { ContextViewer } from "./context-viewer";

const node = (index: number): ContextNode => ({
  id: String(index),
  path: `/messages/${index}`,
  name: `messages[${index}]`,
  kind: "text",
  role: index ? "assistant" : "system",
  source: null,
  bytes: 1000000,
  tokens: 100,
  children: 0,
});
const view = (id = "v1"): ContextContentResult => ({
  view_id: id,
  model: "test/model",
  protocol: "openai_chat",
  context_window: 100000,
  total_tokens: 1000,
  sections: Array.from({ length: 1000 }, (_, index) => node(index)),
  next_cursor: null,
  error: null,
});
beforeEach(() => {
  call.mockReset();
  call.mockImplementation((method: string) =>
    Promise.resolve(method === "session/context_content" ? view() : {}),
  );
});
afterEach(cleanup);

it("千条消息默认折叠，只读取展开正文，关闭释放视图", async () => {
  const ui = render(<ContextViewer sessionId="s1" />);
  await waitFor(() => expect(ui.getByText("messages[0]")).toBeTruthy());
  expect(ui.queryByText("messages[99]")).toBeNull();
  expect(call.mock.calls.filter((args) => args[0] === "session/context_read")).toHaveLength(0);
  call.mockImplementation((method: string) =>
    Promise.resolve(
      method === "session/context_read"
        ? {
            view_id: "v1",
            node_id: "0",
            children: [],
            text: "默认提示词的真实原文",
            next_cursor: 65536,
            sources: [],
          }
        : {},
    ),
  );
  fireEvent.click(ui.getByText("messages[0]"));
  await waitFor(() => expect(ui.getByText("默认提示词的真实原文")).toBeTruthy());
  expect(call.mock.calls.filter((args) => args[0] === "session/context_read")).toHaveLength(1);
  fireEvent.click(ui.getByText("下一页"));
  await waitFor(() =>
    expect(call).toHaveBeenCalledWith(
      "session/context_read",
      expect.objectContaining({ cursor: 65536 }),
    ),
  );
  ui.unmount();
  expect(call).toHaveBeenCalledWith("session/context_close", { session_id: "s1", view_id: "v1" });
});

it("按role消息、content和来源子块逐级独立展开", async () => {
  const system = { ...node(0), name: "#1", kind: "object", children: 1 };
  const content = {
    ...node(1),
    id: "content",
    name: "content",
    role: null,
    path: "/messages/0/content",
    children: 2,
  };
  const intro = {
    ...content,
    id: "intro",
    name: "intro",
    children: 0,
    text_range: [0, 6] as [number, number],
  };
  const policy = {
    ...intro,
    id: "policy",
    name: "tool policy",
    text_range: [6, 12] as [number, number],
  };
  call.mockImplementation((method: string, params: { node_id?: string }) => {
    if (method === "session/context_content")
      return Promise.resolve({
        ...view(),
        sections: [
          system,
          { ...node(2), name: "#2", role: "user" },
          { ...node(3), name: "#3", role: "user" },
        ],
      });
    if (method !== "session/context_read") return Promise.resolve({});
    const children =
      params.node_id === "0" ? [content] : params.node_id === "content" ? [intro, policy] : [];
    return Promise.resolve({
      view_id: "v1",
      node_id: params.node_id,
      children,
      text: params.node_id === "intro" ? "身份原文" : null,
      next_cursor: null,
      sources: [],
    });
  });
  const ui = render(<ContextViewer sessionId="s1" />);
  await waitFor(() => expect(ui.getByRole("button", { name: /system.*#1/ })).toBeTruthy());
  fireEvent.click(ui.getByRole("button", { name: /system.*#1/ }));
  await waitFor(() => expect(ui.getByText("content")).toBeTruthy());
  expect(ui.queryByText("intro")).toBeNull();
  fireEvent.click(ui.getByText("content"));
  await waitFor(() => expect(ui.getByText("tool policy")).toBeTruthy());
  expect(ui.queryByText("身份原文")).toBeNull();
  fireEvent.click(ui.getByText("intro"));
  await waitFor(() => expect(ui.getByText("身份原文")).toBeTruthy());
  expect(
    call.mock.calls
      .filter((args) => args[0] === "session/context_read")
      .map((args) => args[1].node_id),
  ).toEqual(["0", "content", "intro"]);
  expect(ui.getByRole("button", { name: /tool policy/ }).getAttribute("aria-expanded")).toBe(
    "false",
  );
  fireEvent.click(ui.getByRole("button", { name: /system.*#1/ }));
  expect(ui.queryByText("intro")).toBeNull();
  expect(ui.getByRole("button", { name: /user.*#2/ })).toBeTruthy();
  expect(ui.getByRole("button", { name: /user.*#3/ })).toBeTruthy();
});

it("刷新后丢弃旧目录回执并释放旧视图", async () => {
  let resolveOld: (value: ContextContentResult) => void = () => {};
  call.mockImplementation((method: string) =>
    method === "session/context_content"
      ? new Promise<ContextContentResult>((resolve) => {
          resolveOld = resolve;
        })
      : Promise.resolve({}),
  );
  const ui = render(<ContextViewer sessionId="s1" />);
  call.mockImplementation((method: string) =>
    Promise.resolve(
      method === "session/context_content" ? { ...view("new"), model: "new-model" } : {},
    ),
  );
  fireEvent.click(ui.getByLabelText("刷新上下文"));
  await waitFor(() => expect(ui.getByText(/new-model/)).toBeTruthy());
  resolveOld({ ...view("old"), model: "old-model" });
  await waitFor(() =>
    expect(call).toHaveBeenCalledWith("session/context_close", {
      session_id: "s1",
      view_id: "old",
    }),
  );
  expect(ui.queryByText(/old-model/)).toBeNull();
});
