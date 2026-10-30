import { describe, expect, it } from "vitest";
import { liftSvgFences, readFigureTokens, sanitizeSvg } from "./svg-figure";

const tokens = {
  fg: "#111111",
  muted: "#222222",
  border: "#333333",
  surface: "#444444",
  accent: "#555555",
  success: "#666666",
  warning: "#777777",
  error: "#888888",
};

const svg = (body: string) =>
  ["```svg", `<svg xmlns="http://www.w3.org/2000/svg">${body}</svg>`, "```", ""].join("\n");

describe("svg 围栏", () => {
  it("去掉 script，并把 var(--fg) 换成前景色", () => {
    const out = liftSvgFences(svg('<rect fill="var(--fg)"/><script>alert(1)</script>'), tokens);
    expect(out).toContain("moc-svg-figure");
    expect(out).toContain(tokens.fg);
    expect(out).not.toContain("<script");
    expect(out).not.toContain("alert");
    expect(out).not.toContain("```");
  });

  it("currentColor 用前景色，未知变量保留", () => {
    const out = liftSvgFences(
      svg('<rect fill="currentColor" stroke="var(--nope)"/>'),
      tokens,
    );
    expect(out).toContain(`fill="${tokens.fg}"`);
    expect(out).toContain("var(--nope)");
  });

  it("只留 use 的片段引用和 data 图片", () => {
    const out = sanitizeSvg(
      '<svg xmlns="http://www.w3.org/2000/svg"><use href="#mark"/><use href="https://evil.test/x"/><use href="data:image/png;base64,aaaa"/></svg>',
      tokens,
    );
    expect(out).toContain('href="#mark"');
    expect(out).toContain("data:image/png;base64,aaaa");
    expect(out).not.toContain("evil.test");
  });

  it("未闭合、坏掉、别的围栏和引用都不内联", () => {
    const open = "```svg\n<svg></svg>\n";
    expect(liftSvgFences(open, tokens)).toBe(open);
    const broken = "```svg\nnot svg\n```\n";
    expect(liftSvgFences(broken, tokens)).toBe(broken);
    const html = "```html\n<svg></svg>\n```\n";
    expect(liftSvgFences(html, tokens)).toBe(html);
    const quoted = "> ```svg\n> <svg></svg>\n> ```\n";
    expect(liftSvgFences(quoted, tokens)).toBe(quoted);
  });

  it("读不到 CSS 变量时用主题回退色", () => {
    expect(readFigureTokens(false).fg).toBe("#171717");
    expect(readFigureTokens(true).fg).toBe("#ededef");
  });
});
