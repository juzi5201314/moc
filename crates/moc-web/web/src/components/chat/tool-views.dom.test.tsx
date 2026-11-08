import { render } from "@testing-library/react";
import { expect, it } from "vitest";
import type { ToolBlock } from "@/lib/transcript-model";
import { toolBody } from "./tool-views";

const preferBuiltinResult =
  "[E_PREFER_BUILTIN] This bash command only reads, searches, lists, or edits workspace files; use built-in tools instead:\n" +
  '- edit {"path":"src/a.rs"} (instead of sed -i: read the file, then edit the anchored lines)';

function bashTool(result: string): ToolBlock {
  return {
    kind: "tool",
    id: "b1",
    toolCallId: "b1",
    name: "bash",
    arguments: JSON.stringify({ command: "sed -i 's/a/b/' src/a.rs" }),
    status: "error",
    result,
    isError: true,
    images: [],
    children: [],
    awaitingPermission: false,
  };
}

it("「内置工具优先」拒绝按建议而非失败展示，说明与建议都不丢", () => {
  for (const level of ["full", "brief"] as const) {
    const { container, unmount } = render(
      <div>{toolBody(bashTool(preferBuiltinResult), level, () => {})}</div>,
    );
    const text = container.textContent ?? "";
    expect(text, `${level} 档要有说明句`).toContain("use built-in tools instead");
    expect(text, `${level} 档不展示错误码前缀`).not.toContain("[E_PREFER_BUILTIN]");
    expect(text, `${level} 档要有等价调用建议`).toContain('{"path":"src/a.rs"}');
    expect(container.querySelector(".text-destructive"), `${level} 档不能用失败红`).toBeNull();
    expect(container.querySelector(".text-warn"), `${level} 档说明句用警告色`).not.toBeNull();
    unmount();
  }
});

it("普通 bash 错误仍按失败渲染", () => {
  const { container } = render(<div>{toolBody(bashTool("sed: error"), "full", () => {})}</div>);
  expect(container.querySelector(".text-destructive")).not.toBeNull();
});
