// 组装注入主题令牌与双向通信桥的组件 HTML 文档，及用于导出的干净独立 HTML。

export type WidgetTokens = {
  bg: string;
  surface: string;
  fg: string;
  muted: string;
  border: string;
  accent: string;
  success: string;
  warn: string;
  error: string;
  fontSans: string;
  fontMono: string;
};

const DEFAULT_FONT_SANS =
  '"Geist Variable", -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", "Noto Sans SC", sans-serif';
const DEFAULT_FONT_MONO =
  '"Geist Mono Variable", ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace';

const LIGHT_FALLBACK: WidgetTokens = {
  bg: "#ffffff",
  surface: "#f9f9f9",
  fg: "#171717",
  muted: "#676767",
  border: "rgba(23, 23, 23, 0.14)",
  accent: "#3b6ff5",
  success: "#16a34a",
  warn: "#d97706",
  error: "#dc2626",
  fontSans: DEFAULT_FONT_SANS,
  fontMono: DEFAULT_FONT_MONO,
};

const DARK_FALLBACK: WidgetTokens = {
  bg: "#1c1c1f",
  surface: "#161618",
  fg: "#ededef",
  muted: "#a0a0a8",
  border: "rgba(255, 255, 255, 0.15)",
  accent: "#6d97ff",
  success: "#3fc56f",
  warn: "#f0a33a",
  error: "#f26161",
  fontSans: DEFAULT_FONT_SANS,
  fontMono: DEFAULT_FONT_MONO,
};

/** 从当前宿主根元素读取实时主题令牌，保证与 index.css 浅/深色一致。 */
export function readWidgetTokens(dark: boolean): WidgetTokens {
  const fallback = dark ? DARK_FALLBACK : LIGHT_FALLBACK;
  const style =
    typeof document === "undefined" ? null : getComputedStyle(document.documentElement);
  const pick = (cssVar: string, fb: string) => {
    const value = style?.getPropertyValue(cssVar).trim();
    return value ? value : fb;
  };
  return {
    bg: pick("--card", fallback.bg),
    surface: pick("--background", fallback.surface),
    fg: pick("--foreground", fallback.fg),
    muted: pick("--muted-foreground", fallback.muted),
    border: pick("--border-strong", fallback.border),
    accent: pick("--brand", fallback.accent),
    success: pick("--success", fallback.success),
    warn: pick("--warn", fallback.warn),
    error: pick("--destructive", fallback.error),
    fontSans: pick("--font-sans", fallback.fontSans),
    fontMono: pick("--font-mono", fallback.fontMono),
  };
}

function escapeHtmlAttr(value: string): string {
  return value.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}

function escapeHtmlText(value: string): string {
  return value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function safeJson(value: unknown): string {
  return JSON.stringify(value).replace(/</g, "\\u003c");
}

function themeStyleBlock(dark: boolean, tokens: WidgetTokens): string {
  // 使用 :where(...) 保持零特异性，只提供合理默认值，不覆盖组件作者自身的样式规则。
  return `<style id="moc-widget-theme">
:root {
  color-scheme: ${dark ? "dark" : "light"};
  --bg: ${tokens.bg};
  --surface: ${tokens.surface};
  --fg: ${tokens.fg};
  --muted: ${tokens.muted};
  --border: ${tokens.border};
  --accent: ${tokens.accent};
  --success: ${tokens.success};
  --warn: ${tokens.warn};
  --error: ${tokens.error};
  --font-sans: ${tokens.fontSans};
  --font-mono: ${tokens.fontMono};
}
:where(*, *::before, *::after) { box-sizing: border-box; }
:where(html, body) {
  margin: 0;
  background: var(--bg);
  color: var(--fg);
  font-family: var(--font-sans);
  line-height: 1.5;
  -webkit-font-smoothing: antialiased;
}
:where(body) { padding: 16px; }
:where(button, input, select, textarea) { font: inherit; color: inherit; }
</style>`;
}

function bridgeScriptBlock(id: string, dark: boolean): string {
  const safeId = safeJson(id);
  return `<script id="moc-widget-bridge">
(function () {
  var ID = ${safeId};
  var INITIAL_DARK = ${dark ? "true" : "false"};
  var VAR_MAP = {
    bg: "--bg",
    surface: "--surface",
    fg: "--fg",
    muted: "--muted",
    border: "--border",
    accent: "--accent",
    success: "--success",
    warn: "--warn",
    error: "--error",
    fontSans: "--font-sans",
    fontMono: "--font-mono"
  };

  function post(payload) {
    if (window.parent && window.parent !== window) {
      window.parent.postMessage(Object.assign({ channel: "moc-widget", id: ID }, payload), "*");
    }
  }

  function syncThemeMarks(dark) {
    var root = document.documentElement;
    root.classList.toggle("dark", Boolean(dark));
    root.dataset.theme = dark ? "dark" : "light";
    root.style.colorScheme = dark ? "dark" : "light";
  }

  function applyTheme(dark, tokens) {
    var root = document.documentElement;
    syncThemeMarks(dark);
    if (tokens && typeof tokens === "object") {
      for (var k in VAR_MAP) {
        if (typeof tokens[k] === "string") {
          root.style.setProperty(VAR_MAP[k], tokens[k]);
        }
      }
    }
    try {
      window.dispatchEvent(new CustomEvent("moc-theme-change", { detail: { dark: Boolean(dark), tokens: tokens } }));
    } catch (_) {}
  }

  syncThemeMarks(INITIAL_DARK);

  window.addEventListener("message", function (event) {
    if (event.source !== window.parent) return;
    var data = event.data;
    if (!data || data.channel !== "moc-widget" || data.id !== ID) return;
    if (data.type === "theme") {
      applyTheme(data.dark, data.tokens);
    }
  });

  window.addEventListener("error", function (event) {
    var message = (event.error && event.error.message) ? event.error.message : (event.message || "Script error");
    post({
      type: "error",
      message: String(message),
      line: typeof event.lineno === "number" ? event.lineno : undefined,
      column: typeof event.colno === "number" ? event.colno : undefined
    });
  });

  window.addEventListener("unhandledrejection", function (event) {
    var reason = event.reason;
    var message = reason && reason.message ? reason.message : String(reason || "Unhandled promise rejection");
    post({ type: "error", message: String(message) });
  });

  function measureHeight() {
    var body = document.body;
    if (!body) return 0;
    var height = body.scrollHeight || 0;
    var viewH = window.innerHeight || 0;
    // body 自身 min-height: 100vh 时 scrollHeight 恒等于视口，改用子元素包络高度（顶到底），居中布局一步收敛。
    if (viewH > 0 && Math.abs(height - viewH) <= 2 && body.children.length > 0) {
      var style = getComputedStyle(body);
      var pad = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
      var minTop = Infinity, maxBottom = -Infinity;
      for (var i = 0; i < body.children.length; i++) {
        var rect = body.children[i].getBoundingClientRect();
        if (rect.height > 0) {
          if (rect.top < minTop) minTop = rect.top;
          if (rect.bottom > maxBottom) maxBottom = rect.bottom;
        }
      }
      if (maxBottom > minTop) height = Math.min(height, Math.ceil(maxBottom - minTop + pad));
    }
    return Math.ceil(height);
  }

  var lastHeight = 0, lastViewH = 0, rafId = 0;

  function reportHeight() {
    rafId = 0;
    var viewH = window.innerHeight || 0;
    var height = measureHeight();
    var dV = viewH - lastViewH;
    // 子元素用 100vh / h-screen 时内容随 iframe 同步变高：视口刚长了多少、内容就长了多少，停止追高。
    var followsViewport = lastViewH > 0 && dV > 2 && Math.abs((height - lastHeight) - dV) <= 2;
    lastViewH = viewH;
    if (followsViewport) return;
    if (height > 0 && Math.abs(height - lastHeight) > 2) {
      lastHeight = height;
      post({ type: "resize", height: height });
    }
  }

  function scheduleMeasure() {
    if (!rafId) rafId = requestAnimationFrame(reportHeight);
  }

  function attachObservers() {
    scheduleMeasure();
    if (typeof ResizeObserver !== "undefined" && document.body) {
      new ResizeObserver(scheduleMeasure).observe(document.body);
    }
    if (typeof MutationObserver !== "undefined" && document.body) {
      new MutationObserver(scheduleMeasure).observe(document.body, {
        childList: true,
        subtree: true,
        attributes: true
      });
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", attachObservers, { once: true });
  } else {
    attachObservers();
  }
  window.addEventListener("load", scheduleMeasure);
})();
</script>`;
}

function themeHtmlAttrs(attrs: string, dark: boolean): string {
  const theme = dark ? "dark" : "light";
  let next = attrs;
  const themeRe = /(^|\s)data-theme\s*=\s*(?:(["'])[\s\S]*?\2|[^\s>]+)/i;
  if (themeRe.test(next)) {
    next = next.replace(themeRe, (_match, lead: string) => `${lead}data-theme="${theme}"`);
  } else {
    next += ` data-theme="${theme}"`;
  }

  const classRe = /(^|\s)class\s*=\s*(["'])([\s\S]*?)\2/i;
  if (classRe.test(next)) {
    next = next.replace(classRe, (_match, lead: string, quote: string, value: string) => {
      const tokens = value.split(/\s+/).filter((token) => token && token !== "dark");
      if (dark) tokens.push("dark");
      return `${lead}class=${quote}${tokens.join(" ")}${quote}`;
    });
  } else if (dark) {
    next += ' class="dark"';
  }
  return next;
}

function injectIntoFullHtml(
  source: string,
  dark: boolean,
  injectedHead: string,
  titleFallback?: string,
): string {
  let html = source;
  // 统一覆写 <html> 主题标记；省略 <html> 时补齐根标签，保证初始深浅色一致。
  const htmlMatch = /<html\b([^>]*)>/i.exec(html);
  if (htmlMatch) {
    html = html.replace(htmlMatch[0], () => `<html${themeHtmlAttrs(htmlMatch[1], dark)}>`);
  } else {
    const root = `<html${themeHtmlAttrs("", dark)}>`;
    const doctypeRe = /^\s*<!doctype[^>]*>/i;
    html = doctypeRe.test(html)
      ? html.replace(doctypeRe, (doctype) => `${doctype}\n${root}`)
      : `${root}\n${html}`;
  }

  const titleTag =
    titleFallback && !/<title\b/i.test(html) ? `<title>${escapeHtmlText(titleFallback)}</title>` : "";

  const headMatch = /<head\b[^>]*>/i.exec(html);
  if (headMatch) {
    const pos = headMatch.index + headMatch[0].length;
    return `${html.slice(0, pos)}\n${titleTag}${injectedHead}\n${html.slice(pos)}`;
  }

  const updatedHtmlMatch = /<html\b[^>]*>/i.exec(html);
  if (updatedHtmlMatch) {
    const pos = updatedHtmlMatch.index + updatedHtmlMatch[0].length;
    return `${html.slice(0, pos)}\n<head>${titleTag}${injectedHead}</head>\n${html.slice(pos)}`;
  }

  return `${injectedHead}\n${html}`;
}

function isFullHtmlDocument(code: string): boolean {
  return /^\s*(?:<!doctype\s+html\b|<html\b)/i.test(code);
}

/** 构造送入 `/widget-host` iframe 执行的完整 HTML（含主题注入与双向通信桥）。 */
export function buildWidgetDocument(
  code: string,
  options: { id: string; dark: boolean; tokens: WidgetTokens; lang: string },
): string {
  const injected = `${themeStyleBlock(options.dark, options.tokens)}\n${bridgeScriptBlock(options.id, options.dark)}`;
  if (isFullHtmlDocument(code)) {
    return injectIntoFullHtml(code, options.dark, injected);
  }
  const htmlClass = options.dark ? ' class="dark"' : "";
  return `<!doctype html>
<html lang="${escapeHtmlAttr(options.lang)}" data-theme="${options.dark ? "dark" : "light"}"${htmlClass}>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
${injected}
</head>
<body>
${code}
</body>
</html>`;
}

/** 构造可离线双击打开的干净独立 `.html` 文件（含当前主题令牌，不含通信桥脚本）。 */
export function buildStandaloneHtml(
  code: string,
  options: { title: string; dark: boolean; tokens: WidgetTokens; lang: string },
): string {
  const style = themeStyleBlock(options.dark, options.tokens);
  if (isFullHtmlDocument(code)) {
    return injectIntoFullHtml(code, options.dark, style, options.title);
  }
  const htmlClass = options.dark ? ' class="dark"' : "";
  return `<!doctype html>
<html lang="${escapeHtmlAttr(options.lang)}" data-theme="${options.dark ? "dark" : "light"}"${htmlClass}>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>${escapeHtmlText(options.title)}</title>
${style}
</head>
<body>
${code}
</body>
</html>`;
}
