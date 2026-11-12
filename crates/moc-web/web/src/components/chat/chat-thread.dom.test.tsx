import { act, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { BRIEF_BODY_ROWS } from "@/lib/tool-view";
import { applyUpdate, createView, entriesToBlocks } from "@/lib/transcript-model";
import type { SessionInfo } from "@/protocol/types";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { ChatThread } from "./chat-thread";

const info: SessionInfo = {
  id: "copy-reply",
  title: "回复复制",
  model: "test/model",
  permission: "workspace",
  engine: "standard",
  autonomy: "default",
  plan_active: false,
  fast_mode: false,
  engine_locked: false,
  cwd: "/tmp",
  created_at_ms: 0,
  updated_at_ms: 0,
};

const clipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
const toolDetail = usePrefs.getState().toolDetail;
afterEach(() => {
  useTranscript.getState().drop(info.id);
  usePrefs.setState({ toolDetail });
  if (clipboard) Object.defineProperty(navigator, "clipboard", clipboard);
  else Reflect.deleteProperty(navigator, "clipboard");
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("复制回复按需拼接全部正文，保留空段、顺序和流式入口", async () => {
  // jsdom 不做布局；只补观察器接口，使用真实对话、轮次订阅与复制按钮。
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  for (const texts of [["前段\n中文", "尾段\n\n"], ["", ""], ["", "尾段"], [""], []]) {
    const expected = texts.join("\n\n");
    const view = createView(info);
    view.blocks = [
      ...texts.map((text, index) => ({ kind: "text" as const, id: `t${index}`, text })),
      { kind: "footer", id: "footer", atMs: 1, finishReason: "Stop" },
    ];
    useTranscript.setState({ bySession: { [info.id]: view } });
    const join = Array.prototype.join;
    let replyJoins = 0;
    const spy = vi.spyOn(Array.prototype, "join").mockImplementation(function (
      this: unknown[],
      separator,
    ) {
      if (
        separator === "\n\n" &&
        this.length === texts.length &&
        this.every((text, index) => text === texts[index])
      ) {
        replyJoins++;
      }
      return join.call(this, separator);
    });
    const thread = render(
      <TooltipProvider>
        <ChatThread sessionId={info.id} />
      </TooltipProvider>,
    );
    await act(async () => {});
    expect(replyJoins).toBe(0);
    const button = thread.queryByRole("button", { name: "复制回复" });
    expect(Boolean(button)).toBe(Boolean(expected));
    if (button) {
      fireEvent.click(button);
      expect(writeText).toHaveBeenLastCalledWith(expected);
      expect(replyJoins).toBe(1);
    }
    act(() =>
      useTranscript.getState().mutate(info.id, (draft) => {
        draft.running = true;
      }),
    );
    expect(thread.queryByRole("button", { name: "复制回复" })).toBeNull();
    act(() =>
      useTranscript.getState().mutate(info.id, (draft) => {
        draft.running = false;
      }),
    );
    expect(Boolean(thread.queryByRole("button", { name: "复制回复" }))).toBe(Boolean(expected));
    thread.unmount();
    spy.mockRestore();
  }
});

it("实时与恢复的子代理结果独立显示，展开后保留尾部并响应全局折叠", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  const meta = { agent_id: "child", title: "探查者", status: "completed" as const };
  const body = `第一段\n\n${"中间段\n\n".repeat(BRIEF_BODY_ROWS)}报告末尾`;
  for (const restored of [false, true]) {
    usePrefs.setState({ toolDetail: "brief" });
    const view = createView(info);
    view.blocks = [
      { kind: "reasoning", id: "thinking", text: "已折叠的处理过程" },
      { kind: "text", id: "reply", text: "父代理回复" },
    ];
    if (restored) {
      view.blocks.push(
        ...entriesToBlocks([
          {
            id: "result",
            at_ms: 1,
            message: { role: "Custom", content: [{ Text: body }] },
            agent_result: meta,
          },
        ]),
      );
    } else {
      applyUpdate(view, { type: "agent_result", entry_id: "result", at_ms: 1, meta, body });
    }
    view.blocks.push({ kind: "footer", id: "footer", atMs: 2, finishReason: "Stop" });
    useTranscript.setState({ bySession: { [info.id]: view } });
    const thread = render(
      <TooltipProvider>
        <ChatThread sessionId={info.id} />
      </TooltipProvider>,
    );
    await act(async () => {});
    expect(thread.queryByText("已折叠的处理过程")).toBeNull();
    expect(thread.getByText("第一段")).toBeTruthy();
    expect(thread.queryByText("报告末尾")).toBeNull();
    const header = thread.getByRole("button", { name: /探查者/ });
    fireEvent.click(header);
    expect(thread.getByText("报告末尾")).toBeTruthy();
    act(() => usePrefs.setState({ toolDetail: "hidden" }));
    expect(thread.queryByText("第一段")).toBeNull();
    act(() => usePrefs.setState({ toolDetail: "brief" }));
    expect(thread.queryByText("报告末尾")).toBeNull();
    thread.unmount();
  }
});
