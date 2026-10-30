// 把闭合的 ```svg 围栏换成一行内联 SVG。消毒在进 Streamdown 之前做完：
// 页面上还要过 rehype-sanitize，属性名用的是 hast 的写法。

export type FigureTokens = {
  fg: string;
  muted: string;
  border: string;
  surface: string;
  accent: string;
  success: string;
  warning: string;
  error: string;
};

const LIGHT: FigureTokens = {
  fg: "#171717",
  muted: "#6b6b70",
  border: "rgba(23, 23, 23, 0.16)",
  surface: "#f7f7f8",
  accent: "#3b6ff5",
  success: "#16a34a",
  warning: "#d97706",
  error: "#dc2626",
};

const DARK: FigureTokens = {
  fg: "#ededef",
  muted: "#a0a0a8",
  border: "rgba(255, 255, 255, 0.15)",
  surface: "#1b1b1e",
  accent: "#6d97ff",
  success: "#3fc56f",
  warning: "#f0a33a",
  error: "#f26161",
};

const CSS_VAR: Record<keyof FigureTokens, string> = {
  fg: "--foreground",
  muted: "--muted-foreground",
  border: "--border-strong",
  surface: "--code-bg",
  accent: "--brand",
  success: "--success",
  warning: "--warn",
  error: "--destructive",
};

const SVG_TAGS = [
  "svg",
  "g",
  "path",
  "rect",
  "circle",
  "ellipse",
  "line",
  "polyline",
  "polygon",
  "text",
  "tspan",
  "defs",
  "use",
  "image",
  "marker",
  "title",
] as const;

const ALLOWED = new Set<string>(SVG_TAGS);

/** 整棵子树丢掉，不把里面的文本留在图里。 */
const DROP = new Set([
  "script",
  "style",
  "foreignobject",
  "iframe",
  "animate",
  "animatetransform",
  "animatemotion",
  "set",
]);

const SVG_ATTRS = [
  "className",
  "id",
  "xmlns",
  "xmlnsXlink",
  "viewBox",
  "preserveAspectRatio",
  "width",
  "height",
  "x",
  "y",
  "x1",
  "x2",
  "y1",
  "y2",
  "cx",
  "cy",
  "r",
  "rx",
  "ry",
  "dx",
  "dy",
  "d",
  "points",
  "fill",
  "stroke",
  "strokeWidth",
  "strokeLinecap",
  "strokeLinejoin",
  "strokeDasharray",
  "strokeMiterlimit",
  "fillOpacity",
  "strokeOpacity",
  "opacity",
  "transform",
  "fillRule",
  "clipRule",
  "markerStart",
  "markerMid",
  "markerEnd",
  "markerWidth",
  "markerHeight",
  "orient",
  "refX",
  "refY",
  "markerUnits",
  "textAnchor",
  "fontSize",
  "fontFamily",
  "fontWeight",
  "dominantBaseline",
  "href",
  "xlinkHref",
  "xLinkHref",
];

export const SVG_ALLOWED_TAGS: Record<string, string[]> = Object.fromEntries(
  SVG_TAGS.map((tag) => [tag, SVG_ATTRS]),
);

export function readFigureTokens(dark: boolean): FigureTokens {
  const fallback = dark ? DARK : LIGHT;
  const style =
    typeof document === "undefined" ? null : getComputedStyle(document.documentElement);
  const read = (key: keyof FigureTokens) => {
    const value = style?.getPropertyValue(CSS_VAR[key]).trim();
    return value ? value : fallback[key];
  };
  return {
    fg: read("fg"),
    muted: read("muted"),
    border: read("border"),
    surface: read("surface"),
    accent: read("accent"),
    success: read("success"),
    warning: read("warning"),
    error: read("error"),
  };
}

type Fence = { char: string; len: number; info: string };

function openingFence(line: string): Fence | null {
  const match = /^( {0,3})(`{3,}|~{3,})([^`]*)$/.exec(line);
  if (!match) return null;
  const marker = match[2];
  if (marker.startsWith("~") && match[3].includes("~")) return null;
  const info = match[3].trim().split(/\s+/)[0] ?? "";
  return { char: marker[0], len: marker.length, info };
}

function findClose(lines: string[], from: number, fence: Fence): number | null {
  const pattern = new RegExp(`^ {0,3}\\${fence.char}{${fence.len},}[ \\t]*$`);
  for (let index = from; index < lines.length; index++) {
    if (pattern.test(lines[index])) return index;
  }
  return null;
}

/** 顶层闭合的 svg 围栏换成一行图；引号、列表和未闭合围栏不动。 */
export function liftSvgFences(markdown: string, tokens: FigureTokens): string {
  const lines = markdown.replace(/\r\n/g, "\n").split("\n");
  const out: string[] = [];
  const prose: string[] = [];
  const flush = () => {
    if (prose.length === 0) return;
    out.push(...sanitizeRawSvgRegions(prose.join("\n"), tokens).split("\n"));
    prose.length = 0;
  };

  let index = 0;
  while (index < lines.length) {
    const fence = openingFence(lines[index]);
    if (!fence) {
      prose.push(lines[index]);
      index++;
      continue;
    }
    flush();
    const close = findClose(lines, index + 1, fence);
    if (close == null) {
      out.push(...lines.slice(index));
      return out.join("\n");
    }
    if (fence.info.toLowerCase() === "svg") {
      const svg = sanitizeSvg(lines.slice(index + 1, close).join("\n"), tokens);
      if (svg) out.push(svg);
      else out.push(...lines.slice(index, close + 1));
    } else {
      out.push(...lines.slice(index, close + 1));
    }
    index = close + 1;
  }
  flush();
  return out.join("\n");
}

function sanitizeRawSvgRegions(text: string, tokens: FigureTokens): string {
  const pattern = /<svg\b[\s\S]*?<\/svg>/g;
  let last = 0;
  let out = "";
  for (const match of text.matchAll(pattern)) {
    const start = match.index ?? 0;
    out += stripBareSvgRefs(text.slice(last, start));
    // 缩进代码、引用和列表里的图保持源码。其余原始 svg 先消毒，解析失败就整段丢掉。
    out += keepSvgSource(text, start) ? match[0] : (sanitizeSvg(match[0], tokens) ?? "");
    last = start + match[0].length;
  }
  return out + stripBareSvgRefs(text.slice(last));
}

function keepSvgSource(text: string, start: number): boolean {
  const lineStart = text.lastIndexOf("\n", start - 1) + 1;
  const prefix = text.slice(lineStart, start);
  if (/^ {4,}$/.test(prefix)) return true;
  const head = prefix.trimStart();
  return head.startsWith(">") || /^(?:[-+*]|\d+[.)])\s/.test(head);
}

function stripBareSvgRefs(text: string): string {
  return text.replace(/<\/?(?:image|use)\b[^>]*>/gi, "");
}

export function sanitizeSvg(source: string, tokens: FigureTokens): string | null {
  if (!source.includes("<svg")) return null;
  const doc = new DOMParser().parseFromString(source, "image/svg+xml");
  if (doc.querySelector("parsererror")) return null;
  const root = doc.documentElement;
  if (root.tagName.toLowerCase() !== "svg") return null;
  clean(root, tokens);
  if (!root.getAttribute("xmlns")) {
    root.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  }
  root.setAttribute("class", "moc-svg-figure");
  return new XMLSerializer()
    .serializeToString(root)
    .replace(/\s*\n\s*/g, " ")
    .trim();
}

function clean(el: Element, tokens: FigureTokens): void {
  for (const child of [...el.children]) {
    const name = child.tagName.toLowerCase();
    if (DROP.has(name)) {
      child.remove();
      continue;
    }
    if (!ALLOWED.has(name)) {
      clean(child, tokens);
      while (child.firstChild) el.insertBefore(child.firstChild, child);
      child.remove();
      continue;
    }
    clean(child, tokens);
  }
  rewriteAttrs(el, tokens);
}

function rewriteAttrs(el: Element, tokens: FigureTokens): void {
  const tag = el.tagName.toLowerCase();
  for (const attr of [...el.attributes]) {
    const name = attr.name.toLowerCase();
    if (name === "style" || name.startsWith("on") || /url\(\s*['"]?(https?:|\/\/)/i.test(attr.value)) {
      el.removeAttribute(attr.name);
      continue;
    }
    if ((name === "href" || name === "xlink:href") && !safeHref(attr.value, tag)) {
      el.removeAttribute(attr.name);
      continue;
    }
    // `data:` 不在 rehype-sanitize 的 href 协议表里，换成 xlink:href 才能留到页面上。
    if ((name === "href" || name === "xlink:href") && /^data:image\//i.test(attr.value.trim())) {
      el.setAttribute("xlink:href", rewriteColors(attr.value, tokens));
      if (name === "href") el.removeAttribute(attr.name);
      continue;
    }
    const next = rewriteColors(attr.value, tokens);
    if (next !== attr.value) el.setAttribute(attr.name, next);
  }
}

function safeHref(value: string, tag: string): boolean {
  const href = value.trim();
  if (tag === "use" && /^#[A-Za-z_][\w.-]*$/.test(href)) return true;
  return /^data:image\/[a-zA-Z0-9.+-]+[;,]/i.test(href);
}

function rewriteColors(value: string, tokens: FigureTokens): string {
  let out = "";
  let index = 0;
  while (index < value.length) {
    if (value.startsWith("currentColor", index)) {
      out += tokens.fg;
      index += "currentColor".length;
      continue;
    }
    const variable = readVar(value, index);
    if (variable && variable.name in tokens) {
      out += tokens[variable.name as keyof FigureTokens];
      index = variable.end;
      continue;
    }
    out += value[index];
    index++;
  }
  return out;
}

function readVar(value: string, index: number): { name: string; end: number } | null {
  if (!value.startsWith("var(", index)) return null;
  let cursor = index + 4;
  while (cursor < value.length && /\s/.test(value[cursor])) cursor++;
  if (!value.startsWith("--", cursor)) return null;
  cursor += 2;
  const start = cursor;
  while (cursor < value.length && /[A-Za-z0-9_-]/.test(value[cursor])) cursor++;
  const name = value.slice(start, cursor);
  if (!name) return null;
  while (cursor < value.length && /\s/.test(value[cursor])) cursor++;
  if (value[cursor] === ",") {
    cursor++;
    let depth = 0;
    while (cursor < value.length) {
      const ch = value[cursor];
      if (ch === "(") depth++;
      else if (ch === ")" && depth === 0) break;
      else if (ch === ")") depth--;
      cursor++;
    }
  }
  if (value[cursor] !== ")") return null;
  return { name, end: cursor + 1 };
}
