import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { useI18n } from "@/lib/i18n";
import { commandToken } from "@/lib/slash";
import { ShellModeNotice } from "./composer-parts";

afterEach(cleanup);

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
