// 代码高亮：shiki 核心 + JS 正则引擎（不加载 wasm），语言与主题按需动态导入。
// 只打包常用语言；未知语言原样显示为纯文本。

import type { HighlighterCore, LanguageInput } from "shiki/core";

// 动态导入是有意为之：语言包按代码块语言在运行时选择，静态导入会把全部语法打进首屏包。
const LANG_LOADERS: Record<string, LanguageInput> = {
  bash: () => import("shiki/langs/bash.mjs"),
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  java: () => import("shiki/langs/java.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  typescript: () => import("shiki/langs/typescript.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
};

const LANG_ALIAS: Record<string, string> = {
  sh: "bash",
  shell: "bash",
  shellscript: "bash",
  zsh: "bash",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  ts: "typescript",
  py: "python",
  rs: "rust",
  md: "markdown",
  yml: "yaml",
  "c++": "cpp",
  golang: "go",
};

export const LIGHT_THEME = "github-light";
export const DARK_THEME = "github-dark";

let highlighter: Promise<HighlighterCore> | null = null;
/** 语言名 → 加载结果；运行时按需插入，同一语言只加载一次。 */
const loadedLangs = new Map<string, Promise<boolean>>();

function getHighlighter(): Promise<HighlighterCore> {
  highlighter ??= (async () => {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }] = await Promise.all([
      import("shiki/core"),
      import("shiki/engine/javascript"),
    ]);
    return createHighlighterCore({
      themes: [import("shiki/themes/github-light.mjs"), import("shiki/themes/github-dark.mjs")],
      langs: [],
      engine: createJavaScriptRegexEngine(),
    });
  })();
  return highlighter;
}

/** 归一化语言名；不支持的返回 null。 */
export function resolveLang(lang: string | undefined): string | null {
  if (!lang) return null;
  const lower = lang.toLowerCase();
  const name = LANG_ALIAS[lower] ?? lower;
  return Object.hasOwn(LANG_LOADERS, name) ? name : null;
}

/** 高亮成 HTML（shiki 输出已转义）；语言不支持或加载失败返回 null。 */
export async function highlight(code: string, lang: string, dark: boolean): Promise<string | null> {
  const name = resolveLang(lang);
  if (!name) return null;
  const core = await getHighlighter();
  let loaded = loadedLangs.get(name);
  if (!loaded) {
    loaded = core
      .loadLanguage(LANG_LOADERS[name])
      .then(() => true)
      .catch(() => false);
    loadedLangs.set(name, loaded);
  }
  if (!(await loaded)) return null;
  return core.codeToHtml(code, { lang: name, theme: dark ? DARK_THEME : LIGHT_THEME });
}
