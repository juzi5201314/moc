import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { useRef } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { Popover, PopoverAnchor } from "@/components/ui/popover";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n } from "@/lib/i18n";
import { commandToken } from "@/lib/slash";
import { useConnection } from "@/store/connection";
import { ComposerStatus, ShellModeNotice, type Suggestion, SuggestionMenu } from "./composer-parts";

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

const SAMPLE_SUGGESTIONS: Suggestion[] = [
  {
    key: "/debuginfo",
    title: "/debuginfo",
    detail: "查看工具调用统计",
  },
  {
    key: "/goal",
    title: "/goal",
    hint: "[<objective>|edit <objective>|pause|resume|clear]",
    detail: "查看或设置 session 目标，开启后自动续跑直至完成或停止",
    source: "builtin-demo",
  },
];

function SuggestionMenuHarness({
  side = "top",
  active = 0,
  items = SAMPLE_SUGGESTIONS,
  onOpenChange,
}: {
  side?: "top" | "bottom";
  active?: number;
  items?: Suggestion[];
  onOpenChange?: (open: boolean) => void;
}) {
  const inputRef = useRef<HTMLDivElement>(null);
  return (
    <Popover open onOpenChange={onOpenChange}>
      <PopoverAnchor asChild>
        <div ref={inputRef}>input</div>
      </PopoverAnchor>
      <SuggestionMenu
        id="test-suggestions"
        side={side}
        heading="Commands"
        items={items}
        active={active}
        onPick={vi.fn()}
        onHover={vi.fn()}
        inputRef={inputRef}
      />
    </Popover>
  );
}

it("补全详情在宽屏下以独立侧边浮层展示当前高亮项，不叠放其余项撑高主列表", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  const originalScrollIntoView = HTMLElement.prototype.scrollIntoView;
  HTMLElement.prototype.scrollIntoView = vi.fn();
  const originalWidth = window.innerWidth;
  Object.defineProperty(window, "innerWidth", { configurable: true, value: 1280 });

  try {
    const onOpenChange = vi.fn();
    const view = render(<SuggestionMenuHarness active={0} onOpenChange={onOpenChange} />);
    const listbox = view.getByRole("listbox", { name: "Commands" });
    const detail = await view.findByRole("note");

    // 详情卡在主浮层之外，且仅渲染当前高亮项，不含其余项的长参数/说明。
    expect(listbox.parentElement?.contains(detail)).toBe(false);
    expect(detail.dataset.placement).toBe("side");
    expect(detail.textContent).toContain("/debuginfo");
    expect(detail.textContent).toContain("查看工具调用统计");
    expect(detail.textContent).not.toContain("pause|resume|clear");

    // 切换到长参数项后，完整参数语法、说明与来源都在侧边卡展示。
    view.rerender(<SuggestionMenuHarness active={1} onOpenChange={onOpenChange} />);
    const updatedDetail = await view.findByRole("note");
    expect(updatedDetail.textContent).toContain("/goal");
    expect(updatedDetail.textContent).toContain(
      "[<objective>|edit <objective>|pause|resume|clear]",
    );
    expect(updatedDetail.textContent).toContain("builtin-demo");
    expect(updatedDetail.textContent).not.toContain("查看工具调用统计");

    // 在侧边详情卡内按下指针不算外部点击，不关闭主菜单。
    fireEvent.pointerDown(updatedDetail);
    expect(onOpenChange).not.toHaveBeenCalledWith(false);
  } finally {
    HTMLElement.prototype.scrollIntoView = originalScrollIntoView;
    Object.defineProperty(window, "innerWidth", { configurable: true, value: originalWidth });
  }
});

it("补全详情在窄视口自动降级为远离输入框一端的内嵌栏，避免列表位移", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  const originalScrollIntoView = HTMLElement.prototype.scrollIntoView;
  HTMLElement.prototype.scrollIntoView = vi.fn();
  const originalWidth = window.innerWidth;
  const originalHeight = window.innerHeight;
  Object.defineProperty(window, "innerWidth", { configurable: true, value: 380 });
  Object.defineProperty(window, "innerHeight", { configurable: true, value: 900 });
  Object.defineProperty(document.documentElement, "clientWidth", {
    configurable: true,
    value: 380,
  });
  Object.defineProperty(document.documentElement, "clientHeight", {
    configurable: true,
    value: 900,
  });
  const originalRect = HTMLElement.prototype.getBoundingClientRect;
  let anchorTop = 620;
  const rectSpy = vi
    .spyOn(HTMLElement.prototype, "getBoundingClientRect")
    .mockImplementation(function (this: HTMLElement) {
      const slot = this.getAttribute("data-slot");
      if (slot === "popover-anchor") {
        return new DOMRect(14, anchorTop, 352, 40);
      }
      if (slot === "popover-content") {
        return new DOMRect(14, anchorTop === 620 ? 396 : 124, 352, 220);
      }
      return originalRect.call(this);
    });

  try {
    const view = render(<SuggestionMenuHarness side="top" active={1} />);
    const listbox = view.getByRole("listbox", { name: "Commands" });
    const topDetail = await view.findByRole("note");
    expect(topDetail.dataset.placement).toBe("inline-top");
    // 向上弹出时详情在列表之前（顶部），避免高度变化推挤列表。
    expect(topDetail.compareDocumentPosition(listbox) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(
      0,
    );

    anchorTop = 80;
    view.rerender(<SuggestionMenuHarness side="bottom" active={1} />);
    await waitFor(() => {
      const bottomDetail = view.getByRole("note");
      expect(bottomDetail.dataset.placement).toBe("inline-bottom");
      // 向下弹出时详情在列表之后（底部）。
      expect(
        listbox.compareDocumentPosition(bottomDetail) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).not.toBe(0);
    });
  } finally {
    rectSpy.mockRestore();
    HTMLElement.prototype.scrollIntoView = originalScrollIntoView;
    Object.defineProperty(window, "innerWidth", { configurable: true, value: originalWidth });
    Object.defineProperty(window, "innerHeight", { configurable: true, value: originalHeight });
    Reflect.deleteProperty(document.documentElement, "clientWidth");
    Reflect.deleteProperty(document.documentElement, "clientHeight");
  }
});
