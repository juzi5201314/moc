import { render, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MessageResponse } from "./message";

class ImmediateIntersectionObserver {
  readonly callback: IntersectionObserverCallback;
  constructor(callback: IntersectionObserverCallback) {
    this.callback = callback;
  }
  observe() {
    this.callback(
      [{ isIntersecting: true } as IntersectionObserverEntry],
      this as unknown as IntersectionObserver,
    );
  }
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
  root = null;
  rootMargin = "";
  thresholds = [];
}

if (!("IntersectionObserver" in globalThis)) {
  globalThis.IntersectionObserver =
    ImmediateIntersectionObserver as unknown as typeof IntersectionObserver;
}

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

  it("闭合的 svg 围栏内联成图，坏围栏留在代码块", async () => {
    const ok = [
      "```svg",
      '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="var(--fg)"/><script>alert(1)</script></svg>',
      "```",
    ].join("\n");
    const { container } = render(<MessageResponse>{ok}</MessageResponse>);
    await waitFor(() => {
      expect(container.querySelector("svg.moc-svg-figure")).not.toBeNull();
    });

    const svg = container.querySelector("svg.moc-svg-figure");
    expect(svg?.querySelector("script")).toBeNull();
    expect(container.textContent).not.toContain("alert");
    expect(svg?.querySelector("rect")?.getAttribute("fill")).toBe("#171717");

    const linked = [
      "```svg",
      '<svg xmlns="http://www.w3.org/2000/svg"><use href="#mark"/><image href="data:image/png;base64,aaaa"/></svg>',
      "```",
    ].join("\n");
    const linkedView = render(<MessageResponse>{linked}</MessageResponse>);
    await waitFor(() => {
      expect(linkedView.container.querySelector("svg.moc-svg-figure")).not.toBeNull();
    });
    const figure = linkedView.container.querySelector("svg.moc-svg-figure");
    expect(figure?.querySelector("use")?.getAttribute("href")).toBe("#mark");
    const image = figure?.querySelector("image");
    const dataHref = image?.getAttribute("href") ?? image?.getAttribute("xlink:href");
    expect(dataHref).toContain("data:image/png;base64,aaaa");
    expect(figure?.innerHTML).not.toContain("https://");

    const bad = render(<MessageResponse>{"```svg\nnot svg\n```"}</MessageResponse>);
    await waitFor(() => {
      expect(bad.container.querySelector('[data-streamdown="code-block"]')).not.toBeNull();
    });
    expect(bad.container.querySelector("svg.moc-svg-figure")).toBeNull();
  });

  it("mermaid 解析失败时显示源码，不画红框", async () => {
    const { container } = render(
      <MessageResponse>{"```mermaid\nflowchart TD\n  A-->B\n```"}</MessageResponse>,
    );
    await waitFor(
      () => {
        expect(
          container.querySelector('[data-streamdown="mermaid"], .moc-mermaid-fallback'),
        ).not.toBeNull();
      },
      { timeout: 8000 },
    );
    expect(container.querySelector(".bg-red-50")).toBeNull();
  }, 15000);
});
