import { describe, expect, it } from "vitest";
import { mermaidConfig } from "./mermaid-theme";
import type { FigureTokens } from "./svg-figure";

const tokens: FigureTokens = {
  fg: "#111111",
  muted: "#222222",
  border: "#333333",
  surface: "#444444",
  accent: "#555555",
  success: "#666666",
  warning: "#777777",
  error: "#888888",
};

describe("mermaid 主题", () => {
  it("用 base 主题，颜色读图元令牌", () => {
    const config = mermaidConfig(tokens, true);
    expect(config.theme).toBe("base");
    expect(config.securityLevel).toBe("strict");
    expect(config.suppressErrorRendering).toBe(true);
    expect(config.fontFamily).toContain("Geist");
    const theme = config.themeVariables;
    expect(theme?.background).toBe("transparent");
    expect(theme?.darkMode).toBe(true);
    expect(theme?.primaryTextColor).toBe(tokens.fg);
    expect(theme?.primaryBorderColor).toBe(tokens.border);
    expect(theme?.lineColor).toBe(tokens.muted);
    expect(theme?.actorBorder).toBe(tokens.accent);
  });
});