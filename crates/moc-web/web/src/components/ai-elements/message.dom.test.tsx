import { fireEvent, render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { CodeBlock, highlightCode } from "./code-block";
import { MessageResponse } from "./message";

const openFile = vi.fn();
vi.mock("@/store/files", () => ({
  openFile: (...args: unknown[]) => openFile(...args),
}));

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
    const prefix = `{"pad":"${"a".repeat(110)}","value":"`;
    const suffix = `","end":"${"z".repeat(110)}"}`;
    const highlight = (code: string) =>
      new Promise<NonNullable<ReturnType<typeof highlightCode>>>((resolve) => {
        const cached = highlightCode(code, "json", resolve);
        if (cached) resolve(cached);
      });
    for (const middle of ["left", "rght"]) {
      const code = prefix + middle + suffix;
      const result = await highlight(code);
      expect(result.tokens.map((row) => row.map((token) => token.content).join("")).join("\n"))
        .toBe(code);
    }
    const source = '{"number": 1}\n\n{"number": 2}';
    const block = render(<CodeBlock code={source} language="json" showLineNumbers />);
    await waitFor(() => {
      const rows = block.container.querySelectorAll("pre > code > span");
      expect(rows).toHaveLength(3);
      expect(rows[0].textContent).toBe('{"number": 1}');
      expect(rows[1].textContent).toBe("\n");
      expect(rows[2].textContent).toBe('{"number": 2}');
      expect(block.container.querySelector('[class*="counter(line)"]')).not.toBeNull();
      expect(rows[0].querySelector('span[style*="color"]')).not.toBeNull();
    });
    block.rerender(<CodeBlock code="replacement" language="json" />);
    expect(block.container.querySelector("code")?.textContent).toBe("replacement");
    expect(block.container.querySelector('[class*="counter(line)"]')).toBeNull();
    block.unmount();
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

  it("本地路径链接点击在文件分页打开，URL 保持原样", async () => {
    openFile.mockClear();
    useTabs.setState({ active: "s1" });
    useTranscript.setState((state) => ({
      bySession: {
        ...state.bySession,
        s1: { info: { cwd: "/repo" } } as never,
      },
    }));
    const source =
      "改了 [ModelCompat](/repo/src/model.rs) 和 [lib](file:///repo/src/lib.rs)，" +
      "还有 [源码](src/a.ts:9) 与 [URI](local://plan_1.md)，" +
      "参考 [文档](https://example.com/spec)。";
    const { container } = render(<MessageResponse>{source}</MessageResponse>);
    await waitFor(() => {
      expect(container.querySelectorAll('[data-streamdown="link"]')).toHaveLength(5);
    });

    const [model, lib, rel, uri, doc] = [...container.querySelectorAll('[data-streamdown="link"]')];
    fireEvent.click(model);
    expect(openFile).toHaveBeenLastCalledWith("s1", { kind: "fs", path: "/repo/src/model.rs" }, {});
    fireEvent.click(lib);
    expect(openFile).toHaveBeenLastCalledWith("s1", { kind: "fs", path: "/repo/src/lib.rs" }, {});
    fireEvent.click(rel);
    expect(openFile).toHaveBeenLastCalledWith("s1", { kind: "fs", path: "/repo/src/a.ts" }, { line: 9 });
    fireEvent.click(uri);
    expect(openFile).toHaveBeenLastCalledWith("s1", { kind: "resource", uri: "local://plan_1.md" }, {});
    expect(openFile).toHaveBeenCalledTimes(4);
    // 真 URL 交给内建渲染（linkSafety 弹窗按钮），不进入复制路径
    expect(doc.tagName).toBe("BUTTON");
    expect(doc.textContent).toBe("文档");
    expect(container.textContent).not.toContain("[blocked]");
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
