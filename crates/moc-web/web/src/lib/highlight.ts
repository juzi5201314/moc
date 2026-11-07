// 代码高亮：shiki 核心 + JS 正则引擎（不加载 wasm），语言与主题按需动态导入。
// 只打包常用语言；未知语言原样显示为纯文本。

import type { HighlighterCore, LanguageInput, TokensResult } from "shiki/core";

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
/** 已 resolve 的 core；同步 token 化路径用，未就绪时为 null。 */
let core: HighlighterCore | null = null;
/** 语言名 → 加载结果；运行时按需插入，同一语言只加载一次。 */
const loadedLangs = new Map<string, Promise<boolean>>();
/** 加载成功的语言；tokenize 的同步命中判据。 */
const readyLangs = new Set<string>();

function getHighlighter(): Promise<HighlighterCore> {
  highlighter ??= (async () => {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }] = await Promise.all([
      import("shiki/core"),
      import("shiki/engine/javascript"),
    ]);
    core = await createHighlighterCore({
      themes: [import("shiki/themes/github-light.mjs"), import("shiki/themes/github-dark.mjs")],
      langs: [],
      engine: createJavaScriptRegexEngine(),
    });
    return core;
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

/** 确保 core 与指定语言就绪；语言未知或加载失败返回 false。 */
export async function ensureLang(name: string): Promise<boolean> {
  const h = await getHighlighter();
  const loader = LANG_LOADERS[name];
  if (!loader) return false;
  let loaded = loadedLangs.get(name);
  if (!loaded) {
    loaded = h
      .loadLanguage(loader)
      .then(() => {
        readyLangs.add(name);
        return true;
      })
      .catch(() => false);
    loadedLangs.set(name, loaded);
  }
  return loaded;
}

/** 同步 token 化：core 与语言均已就绪才返回，否则 null（经 ensureLang 加载后用 callback 回补）。 */
export function tokenize(code: string, name: string): TokensResult | null {
  if (!core || !readyLangs.has(name)) return null;
  return core.codeToTokens(code, {
    lang: name,
    themes: { light: LIGHT_THEME, dark: DARK_THEME },
  });
}

/** 支持的语言名清单（白名单内，别名见 resolveLang）。 */
export function supportedLanguages(): string[] {
  return Object.keys(LANG_LOADERS);
}

/** 高亮成 HTML（shiki 输出已转义）；语言不支持或加载失败返回 null。 */
export async function highlight(code: string, lang: string, dark: boolean): Promise<string | null> {
  const name = resolveLang(lang);
  if (!name || !(await ensureLang(name)) || !core) return null;
  return core.codeToHtml(code, { lang: name, theme: dark ? DARK_THEME : LIGHT_THEME });
}
