import { describe, expect, it } from "vitest";
import { buildStandaloneHtml, buildWidgetDocument, readWidgetTokens } from "./widget-document";
import { sanitizeFilename, splitWidgetParts } from "./widget-fence";

describe("widget-fence 与文档组装", () => {
  it("拆分顶层 widget 围栏并保留普通代码块与流式状态", () => {
    const md = [
      "前文说明。",
      "",
      "````markdown",
      "```widget",
      "<div>example in code</div>",
      "```",
      "````",
      "",
      '````widget title="统计\\"面板\\"" height=420',
      "<div>```inline```</div>",
      "<script>console.log('ok')</script>",
      "````",
      "",
      "> ```widget",
      "> <p>quoted</p>",
      "> ```",
      "",
      "```widget",
      "<title>流式计算器</title>",
      "<div>partial",
    ].join("\n");

    const parts = splitWidgetParts(md);
    expect(parts).toHaveLength(4);
    expect(parts[0]).toMatchObject({ kind: "markdown" });
    expect((parts[0] as { text: string }).text).toContain("<div>example in code</div>");

    expect(parts[1]).toEqual({
      kind: "widget",
      index: 0,
      title: '统计"面板"',
      height: 420,
      code: "<div>```inline```</div>\n<script>console.log('ok')</script>",
      complete: true,
    });

    expect(parts[2]).toMatchObject({ kind: "markdown" });
    expect((parts[2] as { text: string }).text).toContain("<p>quoted</p>");

    expect(parts[3]).toEqual({
      kind: "widget",
      index: 1,
      title: "流式计算器",
      height: undefined,
      code: "<title>流式计算器</title>\n<div>partial",
      complete: false,
    });
  });

  it("列表项内的 widget 围栏保留为 Markdown，顶层缩进围栏仍可识别", () => {
    const md = [
      "- 无序列表项",
      "  ```widget",
      "<div>无序列表示例</div>",
      "  ```",
      "1) 有序列表项",
      "   ```widget",
      "<div>有序列表示例</div>",
      "   ```",
      "",
      ' ```widget title="顶层组件"',
      "<div>顶层组件</div>",
      " ```",
    ].join("\n");

    const parts = splitWidgetParts(md);
    expect(parts).toHaveLength(2);
    const markdownPart = parts[0];
    expect(markdownPart?.kind).toBe("markdown");
    if (!markdownPart || markdownPart.kind !== "markdown") {
      throw new Error("列表项内的代码必须保留为 Markdown");
    }
    expect(markdownPart.text).toContain("<div>无序列表示例</div>");
    expect(markdownPart.text).toContain("<div>有序列表示例</div>");
    expect(parts[1]).toMatchObject({
      kind: "widget",
      title: "顶层组件",
      code: "<div>顶层组件</div>",
      complete: true,
    });
  });

  it("组装宿主运行文档与离线导出 HTML，并转义闭合标签", () => {
    const tokens = readWidgetTokens(true);
    const runtime = buildWidgetDocument("<div>hi</div>", {
      id: "w-1</script>",
      dark: true,
      tokens,
      lang: "zh-CN",
    });
    expect(runtime).toContain('data-theme="dark"');
    expect(runtime).toContain('class="dark"');
    expect(runtime).toContain("id=\"moc-widget-bridge\"");
    expect(runtime).toContain("\\u003c/script>");
    expect(runtime).not.toContain("w-1</script>");

    const standalone = buildStandaloneHtml("<!doctype html><html><head></head><body>ok</body></html>", {
      title: "Demo",
      dark: false,
      tokens: readWidgetTokens(false),
      lang: "en",
    });
    expect(standalone).toContain("<title>Demo</title>");
    expect(standalone).toContain("moc-widget-theme");
    expect(standalone).not.toContain("moc-widget-bridge");
    expect(sanitizeFilename("统计 / 面板?v1")).toBe("统计-面板-v1.html");
  });
});
