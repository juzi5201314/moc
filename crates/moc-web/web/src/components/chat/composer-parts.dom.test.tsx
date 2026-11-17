import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { useI18n } from "@/lib/i18n";
import { ShellModeNotice } from "./composer-parts";

afterEach(cleanup);

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
