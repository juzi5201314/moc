import { act, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { createView } from "@/lib/transcript-model";
import type { SessionInfo } from "@/protocol/types";
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
afterEach(() => {
  useTranscript.getState().drop(info.id);
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
