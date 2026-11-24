import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n } from "@/lib/i18n";
import { commandToken } from "@/lib/slash";
import { useConnection } from "@/store/connection";
import { ComposerStatus, ShellModeNotice } from "./composer-parts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  useConnection.setState({ server: null });
});

it("命令范围只认开头的已知命令、别名和插件命令", () => {
  const commands = [
    { name: "help", description: "帮助", aliases: ["h"] },
    { name: "deploy", description: "插件", plugin: "demo" },
  ];
  for (const [text, token] of [
    ["  /help 参数 /help", "/help"],
    ["/h 参数", "/h"],
    ["/deploy 参数", "/deploy"],
    ["/unknown 参数", null],
    ["//foo", null],
    ["普通文本 /help", null],
    ["", null],
  ] as const) {
    const range = commandToken(text, commands);
    expect(range ? text.slice(range.start, range.end) : null).toBe(token);
  }
});

it("手动命令在非 full 权限下提示例外，普通文本和 full 模式不显示额外警告", () => {
  const original = useI18n.getState().locale;
  const view = render(<ShellModeNotice text="" />);
  try {
    for (const locale of ["zh", "en"] as const) {
      useI18n.setState({ locale });
      const hint =
        locale === "zh"
          ? "手动 command 不受 sandbox 限制，将以 full 权限运行。"
          : "Manual commands bypass the sandbox and run with full access.";
      for (const permission of [undefined, "full", "workspace", "workspace_only"] as const) {
        for (const text of ["!echo ok", "!!echo ok", "ordinary", ""]) {
          view.rerender(<ShellModeNotice text={text} permission={permission} />);
          expect(Boolean(view.queryByText(hint))).toBe(
            Boolean(text.startsWith("!") && permission && permission !== "full"),
          );
          if (!text.startsWith("!")) expect(view.container.textContent).toBe("");
        }
      }
    }
  } finally {
    useI18n.setState({ locale: original });
  }
});

it("空闲态 tips 末尾提供刷新按钮，点击随机切到不同条目；仅 1 条时不显示按钮", () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  const tips = ["Tip alpha", "Tip beta", "Tip gamma"];
  useConnection.setState({
    server: {
      protocol_version: "1.0",
      server: { name: "moc-core", version: "0.0.0" },
      tips,
    },
  });

  const view = render(
    <TooltipProvider>
      <ComposerStatus sessionId="draft-session" todos={[]} />
    </TooltipProvider>,
  );

  const button = view.getByRole("button", { name: "Next tip" });
  const currentTip = () => tips.find((item) => view.queryByText(item) !== null);
  let prev = currentTip();
  expect(prev).toBeDefined();

  // 连续点击多次，每次都必须切到不同于当前的一条。
  for (let i = 0; i < 6; i += 1) {
    fireEvent.click(button);
    const next = currentTip();
    expect(next).toBeDefined();
    expect(next).not.toBe(prev);
    prev = next;
  }

  // 只有一条 tip 时不显示无意义的刷新按钮。
  act(() => {
    useConnection.setState({
      server: {
        protocol_version: "1.0",
        server: { name: "moc-core", version: "0.0.0" },
        tips: ["Only tip"],
      },
    });
  });
  expect(view.queryByRole("button", { name: "Next tip" })).toBeNull();
});
