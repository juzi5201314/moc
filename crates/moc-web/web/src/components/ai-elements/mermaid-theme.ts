import type { MermaidConfig } from "@streamdown/mermaid";

import type { FigureTokens } from "./svg-figure";

/** mermaid 的 base 主题，颜色跟当前 prose 令牌走，背景保持透明。 */
export function mermaidConfig(tokens: FigureTokens, dark: boolean): MermaidConfig {
  return {
    startOnLoad: false,
    theme: "base",
    securityLevel: "strict",
    suppressErrorRendering: true,
    fontFamily: "Geist, sans-serif",
    themeVariables: {
      darkMode: dark,
      background: "transparent",
      fontFamily: "Geist, sans-serif",
      primaryColor: tokens.surface,
      primaryTextColor: tokens.fg,
      primaryBorderColor: tokens.border,
      secondaryColor: tokens.surface,
      secondaryTextColor: tokens.fg,
      secondaryBorderColor: tokens.border,
      tertiaryColor: tokens.surface,
      tertiaryTextColor: tokens.muted,
      tertiaryBorderColor: tokens.border,
      lineColor: tokens.muted,
      textColor: tokens.fg,
      mainBkg: tokens.surface,
      nodeBorder: tokens.border,
      clusterBkg: tokens.surface,
      clusterBorder: tokens.border,
      titleColor: tokens.fg,
      edgeLabelBackground: tokens.surface,
      actorBkg: tokens.surface,
      actorBorder: tokens.accent,
      actorTextColor: tokens.fg,
      signalColor: tokens.fg,
      signalTextColor: tokens.fg,
      noteBkgColor: tokens.surface,
      noteTextColor: tokens.fg,
      noteBorderColor: tokens.border,
      labelColor: tokens.fg,
    },
  };
}
