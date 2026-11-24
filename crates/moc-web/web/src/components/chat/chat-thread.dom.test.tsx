import { act, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n } from "@/lib/i18n";
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
const locale = useI18n.getState().locale;

it("已完成的回复以静态 Markdown 渲染并保留语义内容", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  const view = createView(info);
  view.blocks = [
    { kind: "user", id: "question", text: "解释这段代码", images: [], atMs: 0 },
    {
      kind: "text",
      id: "answer",
      text: "**完整答案**与 `代码`。",
    },
    { kind: "footer", id: "footer", finishReason: "Stop", atMs: 1 },
  ];
  useTranscript.setState({ bySession: { [info.id]: view } });
  const thread = render(
    <TooltipProvider>
      <ChatThread sessionId={info.id} />
    </TooltipProvider>,
  );

  await waitFor(() => {
    const prose = thread.container.querySelector(".moc-prose");
    expect(prose?.querySelector('[data-streamdown="strong"]')?.textContent).toBe("完整答案");
    expect(prose?.querySelector('[data-streamdown="inline-code"]')?.textContent).toBe("代码");
  });
  thread.unmount();
});
it("连接退避在首个生成块之前显示重试原因及等待时长", async () => {
  vi.spyOn(Date, "now").mockReturnValue(1000);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  const view = createView(info);
  view.blocks = [{ kind: "user", id: "question", text: "你好", images: [], atMs: 0 }];
  applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m", at_ms: 0 });
  const message =
    "连接或请求失败，60.0 秒后自动重试（第 2/10 次尝试）。\n网络传输失败: http://127.0.0.1:1";
  applyUpdate(view, { type: "generation_retry", prompt_id: "p", message, retry_in_ms: 60000 });
  useTranscript.setState({ bySession: { [info.id]: view } });
  const thread = render(
    <TooltipProvider>
      <ChatThread sessionId={info.id} />
    </TooltipProvider>,
  );
  await act(async () => {});
  expect(thread.getByText(message.replace(/\s+/g, " "))).toBeTruthy();
  expect(thread.container.textContent).toContain("2/10");
  expect(view.running).toBe(true);
  expect(view.blocks.filter((block) => block.kind === "status")).toMatchObject([{ tone: "warn" }]);
  thread.unmount();
});

afterEach(() => {
  vi.useRealTimers();
  useI18n.setState({ locale });
  useTranscript.getState().drop(info.id);
  usePrefs.setState({ toolDetail });
  if (clipboard) Object.defineProperty(navigator, "clipboard", clipboard);
  else Reflect.deleteProperty(navigator, "clipboard");
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("连续重试复用同一卡片，中英文倒计时更新并在恢复生成后移除", async () => {
  vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  for (const language of ["zh", "en"] as const) {
    useI18n.setState({ locale: language });
    const view = createView(info);
    view.blocks = [{ kind: "user", id: "question", text: "你好", images: [], atMs: 0 }];
    applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m", at_ms: 0 });
    const message = (attempt: number) =>
      language === "zh"
        ? `连接或请求失败，99.0 秒后自动重试（第 ${attempt}/10 次尝试）。\n原因 ${attempt}`
        : `Connection or request failed; retrying in 99.0s (attempt ${attempt}/10).\nReason ${attempt}`;
    const expected = (attempt: number, seconds: string) =>
      message(attempt).replace("99.0", seconds).replace(/\s+/g, " ");
    applyUpdate(view, {
      type: "generation_retry",
      prompt_id: "p",
      message: message(2),
      retry_in_ms: 2500,
    });
    useTranscript.setState({ bySession: { [info.id]: view } });
    const thread = render(
      <TooltipProvider>
        <ChatThread sessionId={info.id} />
      </TooltipProvider>,
    );
    await act(async () => {});
    const notice = thread.getByText(expected(2, "2.5"));
    await act(async () => {
      vi.advanceTimersByTime(1000);
    });
    expect(thread.getByText(expected(2, "1.5"))).toBe(notice);
    for (let attempt = 3; attempt <= 8; attempt++) {
      await act(async () => {
        useTranscript.getState().mutate(info.id, (current) => {
          applyUpdate(current, {
            type: "generation_retry",
            prompt_id: "p",
            message: message(attempt),
            retry_in_ms: 2500,
          });
        });
      });
      expect(thread.getByText(expected(attempt, "2.5"))).toBe(notice);
    }
    expect(
      useTranscript.getState().bySession[info.id].blocks.filter((block) => block.kind === "status"),
    ).toHaveLength(1);
    await act(async () => {
      vi.advanceTimersByTime(5000);
    });
    expect(thread.getByText(expected(8, "0.0"))).toBe(notice);
    await act(async () => {
      useTranscript
        .getState()
        .mutate(info.id, (current) =>
          applyUpdate(current, { type: "generation_started", prompt_id: "p" }),
        );
    });
    expect(notice.isConnected).toBe(false);
    thread.unmount();
  }
});

it("空原因及零等待也显示倒计时，取消或 RPC 失败会移除重试卡片", async () => {
  vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  useI18n.setState({ locale: "zh" });
  for (const failed of [false, true]) {
    const view = createView(info);
    applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m", at_ms: 0 });
    applyUpdate(view, { type: "generation_retry", prompt_id: "p", message: "", retry_in_ms: 0 });
    useTranscript.setState({ bySession: { [info.id]: view } });
    const thread = render(
      <TooltipProvider>
        <ChatThread sessionId={info.id} />
      </TooltipProvider>,
    );
    await act(async () => {});
    expect(thread.getByText("Retry in 0.0s")).toBeTruthy();
    await act(async () => {
      if (failed) useTranscript.getState().failPrompt(info.id);
      else
        useTranscript.getState().mutate(info.id, (current) =>
          applyUpdate(current, {
            type: "prompt_finished",
            prompt_id: "p",
            message: { role: "Assistant", content: [] },
            finish_reason: "Cancelled",
            usage: { input_tokens: 0, output_tokens: 0 },
          }),
        );
    });
    expect(thread.queryByText("Retry in 0.0s")).toBeNull();
    expect(useTranscript.getState().bySession[info.id].running).toBe(false);
    thread.unmount();
  }
});

it("处理摘要在中英文中插入耗时，缺少耗时或异常结束时仍正常显示", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  for (const language of ["zh", "en"] as const) {
    useI18n.setState({ locale: language });
    for (const durationMs of [23_000, 0, undefined, null]) {
      const view = createView(info);
      view.blocks = [
        { kind: "reasoning", id: "thinking", text: "处理过程" },
        { kind: "text", id: "reply", text: "回复" },
      ];
      if (durationMs !== null) {
        view.blocks.push({
          kind: "footer",
          id: "footer",
          atMs: 1,
          durationMs,
          finishReason: "Stop",
        });
      }
      useTranscript.setState({ bySession: { [info.id]: view } });
      const thread = render(
        <TooltipProvider>
          <ChatThread sessionId={info.id} />
        </TooltipProvider>,
      );
      await act(async () => {});
      const expected = durationMs == null ? "Process" : `Worked ${durationMs / 1000}s`;
      expect(thread.getByText(expected)).toBeTruthy();
      expect(thread.container.textContent).not.toContain("{n}");
      thread.unmount();
    }
  }
});

it("用户 shell 独立展示且不折叠上一轮最终回复", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  usePrefs.setState({ toolDetail: "brief" });
  const view = createView(info);
  view.blocks = [
    { kind: "text", id: "answer", text: "上一轮最终回复" },
    { kind: "footer", id: "footer", atMs: 2, finishReason: "Stop" },
    { kind: "bash", id: "shell", command: "printf BASH-CHECK", output: "BASH-CHECK", atMs: 3 },
  ];
  useTranscript.setState({ bySession: { [info.id]: view } });
  const thread = render(
    <TooltipProvider>
      <ChatThread sessionId={info.id} />
    </TooltipProvider>,
  );
  await act(async () => {});
  expect(thread.getByText("上一轮最终回复")).toBeTruthy();
  expect(thread.getByText("BASH-CHECK")).toBeTruthy();
  expect(thread.queryByRole("button", { name: "Retry" })).toBeNull();
  thread.unmount();
});

it("虚拟轮次以布局偏移定位，展开过程中的工具标题在正文之前", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  usePrefs.setState({ toolDetail: "brief" });
  for (const rows of [0, 2, 80]) {
    const view = createView(info);
    view.blocks = [
      {
        kind: "tool",
        id: "tool",
        toolCallId: "tool",
        name: "bash",
        arguments: '{"command":"printf CHECK"}',
        status: "completed",
        result: Array.from({ length: rows }, (_, i) => `CHECK-${i}`).join("\n"),
        isError: rows === 0,
        images: [],
        children: [],
        awaitingPermission: false,
      },
      { kind: "text", id: "answer", text: "最终回复" },
      { kind: "footer", id: "footer", atMs: 2, finishReason: "Stop" },
    ];
    useTranscript.setState({ bySession: { [info.id]: view } });
    const thread = render(
      <TooltipProvider>
        <ChatThread sessionId={info.id} />
      </TooltipProvider>,
    );
    await act(async () => {});
    fireEvent.click(thread.getByRole("button", { name: /Process/ }));
    const toggle = thread.getByRole("button", { name: /printf CHECK/ });
    const header = toggle.parentElement;
    if (!header) throw new Error("缺少工具标题行");
    const row = header.closest<HTMLElement>("[data-index]");
    expect(row?.style.top).toBe("32px");
    expect(row?.style.transform).toBe("");
    expect(header.classList.contains("sticky")).toBe(true);
    expect(header.parentElement?.firstElementChild).toBe(header);
    if (rows > 0) {
      fireEvent.click(toggle);
      expect(thread.container.textContent).toContain("CHECK-0");
      expect(header.parentElement?.firstElementChild).toBe(header);
    }
    thread.unmount();
  }
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
    const button = thread.queryByRole("button", { name: "Copy reply" });
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
    expect(thread.queryByRole("button", { name: "Copy reply" })).toBeNull();
    act(() =>
      useTranscript.getState().mutate(info.id, (draft) => {
        draft.running = false;
      }),
    );
    expect(Boolean(thread.queryByRole("button", { name: "Copy reply" }))).toBe(Boolean(expected));
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

it("正文把过程切成「工作了」段，完成后整体再折叠一层", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  useI18n.setState({ locale: "zh" });
  const call = (id: string, command: string) => ({
    ToolCall: { id, name: "bash", arguments: JSON.stringify({ command }) },
  });
  const result = (id: string, at_ms: number) => ({
    id: `${id}-result`,
    at_ms,
    message: {
      role: "Tool" as const,
      content: [{ ToolResult: { tool_call_id: id, content: "ok", is_error: false } }],
    },
  });
  const entries = [
    { id: "user", at_ms: 0, message: { role: "User" as const, content: [{ Text: "问题" }] } },
    {
      id: "a1",
      at_ms: 1000,
      message: { role: "Assistant" as const, content: [call("c1", "printf ONE")] },
    },
    result("c1", 2000),
    {
      id: "a2",
      at_ms: 6500,
      message: {
        role: "Assistant" as const,
        content: [{ Text: "中途说明" }, call("c2", "printf TWO")],
      },
      response: { model: "test/gpt-test", text_spans: [{ start_ms: 5000, end_ms: 6000 }] },
    },
    result("c2", 8000),
    {
      id: "a3",
      at_ms: 10_000,
      message: { role: "Assistant" as const, content: [{ Text: "最终回复" }] },
      response: { model: "test/gpt-test", text_spans: [{ start_ms: 9000, end_ms: 9500 }] },
    },
  ];
  for (const running of [true, false]) {
    const view = createView(info);
    view.blocks = entriesToBlocks(running ? entries.slice(0, -1) : entries);
    view.running = running;
    useTranscript.setState({ bySession: { [info.id]: view } });
    const thread = render(
      <TooltipProvider>
        <ChatThread sessionId={info.id} />
      </TooltipProvider>,
    );
    await act(async () => {});
    if (running) {
      expect(thread.getByRole("button", { name: "Worked 5s" })).toBeTruthy();
      expect(thread.queryByRole("button", { name: /printf ONE/ })).toBeNull();
      expect(thread.getByText("中途说明")).toBeTruthy();
      expect(thread.getByRole("button", { name: /printf TWO/ })).toBeTruthy();
    } else {
      expect(thread.queryByText("中途说明")).toBeNull();
      expect(thread.getByText("最终回复")).toBeTruthy();
      expect(thread.getByText("gpt-test")).toBeTruthy();
      fireEvent.click(thread.getByRole("button", { name: "Worked 9s" }));
      expect(thread.getByText("中途说明")).toBeTruthy();
      expect(thread.queryByRole("button", { name: /printf/ })).toBeNull();
      fireEvent.click(thread.getByRole("button", { name: "Worked 3s" }));
      expect(thread.getByRole("button", { name: /printf TWO/ })).toBeTruthy();
      expect(thread.getByRole("button", { name: "Worked 5s" })).toBeTruthy();
    }
    thread.unmount();
  }

  // 最终回复之后存在常显告警时，不产生空工作段导致外层内再套一层折叠。
  const withTrailingNotice = createView(info);
  withTrailingNotice.blocks = [
    ...entriesToBlocks([entries[0], entries[1], entries[2], entries[5]]),
    { kind: "status", id: "warn-1", tone: "warn", text: "末尾告警" },
  ];
  withTrailingNotice.running = false;
  useTranscript.setState({ bySession: { [info.id]: withTrailingNotice } });
  const trailing = render(
    <TooltipProvider>
      <ChatThread sessionId={info.id} />
    </TooltipProvider>,
  );
  await act(async () => {});
  expect(trailing.getByText("末尾告警")).toBeTruthy();
  fireEvent.click(trailing.getByRole("button", { name: "Worked 9s" }));
  expect(trailing.getByRole("button", { name: /printf ONE/ })).toBeTruthy();
  expect(trailing.getAllByRole("button", { name: /Worked/ })).toHaveLength(1);
  trailing.unmount();
});
