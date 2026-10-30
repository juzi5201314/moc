import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MessageResponse } from "./message";

const MD = `# 标题一

段落里有 \`inline\` 代码。

| 参数 | 覆盖情况 |
| ---- | -------- |
| a    | 1        |
| b    | 2        |

\`\`\`bash
$ python3 hello.py --name Mia
hello Mia
\`\`\`

> 引用
`;

describe("MessageResponse GitHub 风格渲染", () => {
  it("输出 data-streamdown 标记供 moc-prose 命中", async () => {
    const { container } = render(<MessageResponse>{MD}</MessageResponse>);
    await new Promise((r) => setTimeout(r, 50));

    for (const marker of [
      "heading-1",
      "table",
      "table-body",
      "table-row",
      "table-header-cell",
      "table-cell",
      "code-block",
      "code-block-body",
      "inline-code",
      "blockquote",
    ]) {
      expect(
        container.querySelector(`[data-streamdown="${marker}"]`),
        marker,
      ).not.toBeNull();
    }
  });

  it("代码块只保留复制按钮，表格无工具条", async () => {
    const { container } = render(<MessageResponse>{MD}</MessageResponse>);
    await new Promise((r) => setTimeout(r, 50));

    // 复制保留、下载移除
    expect(container.querySelector('[data-streamdown="code-block-copy-button"]')).not.toBeNull();
    expect(container.querySelector('[data-streamdown="code-block-download-button"]')).toBeNull();
    // 表格工具条整体禁用
    expect(container.querySelector('[data-streamdown="table-wrapper"] button')).toBeNull();
    // 无行号 gutter 类
    expect(container.querySelector('[class*="counter(line)"]')).toBeNull();
  });
});
