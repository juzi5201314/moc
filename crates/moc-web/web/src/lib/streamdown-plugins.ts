// 自实现 Streamdown 插件：
// - @streamdown/code 顶层 `import "shiki"`（全量语言注册表），vite 只能给 300+ 种
//   grammar 各发懒 chunk——全部压进二进制；这里转发 lib/highlight 的 19 语言懒加载 core。
// - @streamdown/mermaid 顶层 `import mermaid`，把 mermaid core 拉进首屏；
//   这里做成同接口懒壳，首个 mermaid 块渲染时才动态导入。

import type { TokensResult } from "shiki/core";
import type {
  CodeHighlighterPlugin,
  DiagramPlugin,
  HighlightOptions,
} from "streamdown";
import {
  DARK_THEME,
  ensureLang,
  LIGHT_THEME,
  resolveLang,
  supportedLanguages,
  tokenize,
} from "./highlight";

// streamdown 不直接导出这两个类型，从 DiagramPlugin 反推。
type MermaidConfig = NonNullable<Parameters<DiagramPlugin["getMermaid"]>[0]>;
type MermaidInstance = ReturnType<DiagramPlugin["getMermaid"]>;

export const codePlugin = {
  name: "shiki",
  type: "code-highlighter",
  getSupportedLanguages: supportedLanguages,
  getThemes: () => [LIGHT_THEME, DARK_THEME] as [string, string],
  supportsLanguage: (language: string) => resolveLang(language) !== null,
  // 命中已加载语言同步返回；未就绪返回 null 并异步加载后经 callback 回补（streamdown 既有协议）。
  highlight(
    { code, language }: HighlightOptions,
    callback?: (result: TokensResult) => void,
  ): TokensResult | null {
    const name = resolveLang(language);
    if (!name) return null;
    const hit = tokenize(code, name);
    if (hit) return hit;
    if (callback) {
      void ensureLang(name).then((ok) => {
        if (ok) {
          const result = tokenize(code, name);
          if (result) callback(result);
        }
      });
    }
    return null;
  },
} satisfies CodeHighlighterPlugin;

// 同 @streamdown/mermaid 的默认配置。
const MERMAID_DEFAULTS: MermaidConfig = {
  fontFamily: "monospace",
  securityLevel: "strict",
  startOnLoad: false,
  suppressErrorRendering: true,
  theme: "default",
};

let mermaidModule: Promise<typeof import("mermaid").default> | null = null;
const loadMermaid = () => (mermaidModule ??= import("mermaid").then((module) => module.default));

export const mermaidPlugin: DiagramPlugin = (() => {
  let initialized = false;
  let config = { ...MERMAID_DEFAULTS };
  const controller: MermaidInstance = {
    initialize(extra: MermaidConfig) {
      config = { ...MERMAID_DEFAULTS, ...extra };
      initialized = false;
    },
    async render(id: string, source: string) {
      const mermaid = await loadMermaid();
      if (!initialized) {
        // streamdown 的 MermaidConfig 刻意放宽为 Record<string, any>，透传即可。
        mermaid.initialize(config as import("mermaid").MermaidConfig);
        initialized = true;
      }
      return mermaid.render(id, source);
    },
  };
  return {
    language: "mermaid",
    name: "mermaid",
    type: "diagram",
    getMermaid(extra?: MermaidConfig) {
      if (extra) controller.initialize(extra);
      return controller;
    },
  };
})();
