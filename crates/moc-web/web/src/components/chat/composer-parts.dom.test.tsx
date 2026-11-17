import { cleanup, fireEvent, render } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { useI18n } from "@/lib/i18n";
import { commandToken } from "@/lib/slash";
import { CommandTextarea } from "./command-textarea";
import { ShellModeNotice } from "./composer-parts";

afterEach(cleanup);

it("镜像只突出已知命令、别名和插件命令，保留原生输入与滚动", () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  const ref = createRef<HTMLTextAreaElement>();
  const onChange = vi.fn();
  const commands = [
    { name: "help", description: "帮助", aliases: ["h"] },
    { name: "deploy", description: "插件", plugin: "demo" },
  ];
  const view = render(<CommandTextarea command={null} value="" ref={ref} onChange={onChange} />);
  try {
    for (const [text, token] of [
      ["  /help 参数 /help", "/help"],
      ["/h 参数", "/h"],
      ["/deploy 参数", "/deploy"],
      ["/unknown 参数", null],
      ["//foo", null],
      ["普通文本 /help", null],
      ["", null],
    ] as const) {
      view.rerender(
        <CommandTextarea
          command={commandToken(text, commands)}
          value={text}
          ref={ref}
          onChange={onChange}
          className="text-foreground"
        />,
      );
      const input = view.getByRole("textbox");
      expect(ref.current).toBe(input);
      expect(ref.current?.value).toBe(text);
      const mirror = view.container.querySelector<HTMLElement>('[aria-hidden="true"]');
      if (token) {
        expect(mirror?.querySelector("span")?.textContent).toBe(token);
        expect(mirror?.querySelector("span")?.classList.contains("text-brand")).toBe(true);
        expect(mirror?.textContent).toBe(`${text}\u200b`);
        expect(input.classList.contains("text-transparent")).toBe(true);
        Object.defineProperty(input, "clientWidth", { value: 180, configurable: true });
        Object.defineProperty(input, "clientHeight", { value: 72, configurable: true });
        fireEvent.scroll(input, { target: { scrollTop: 24 } });
        expect(mirror?.scrollTop).toBe(24);
        expect(mirror?.style.width).toBe("180px");
        expect(mirror?.style.height).toBe("72px");
      } else {
        expect(mirror).toBeNull();
        expect(input.classList.contains("text-foreground")).toBe(true);
        expect(input.classList.contains("text-transparent")).toBe(false);
      }
    }
    fireEvent.change(view.getByRole("textbox"), { target: { value: "/typo" } });
    expect(onChange).toHaveBeenCalledOnce();
  } finally {
    cleanup();
    vi.unstubAllGlobals();
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
          ? "手动命令不受沙箱限制，以完整权限运行"
          : "Manual commands bypass the sandbox and run with full access";
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
