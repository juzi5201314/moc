// 顶层 ```widget 围栏拆分器：把 Markdown 拆为普通文本段与嵌入式交互组件段。
// 遵循 CommonMark 顶层围栏规则，跳过缩进/引用/列表以及其它语言代码块内部示例，避免误切。

export type MessagePart =
  | { kind: "markdown"; text: string }
  | {
      kind: "widget";
      index: number;
      title?: string;
      height?: number;
      code: string;
      complete: boolean;
    };

type Fence = {
  char: string;
  len: number;
  lang: string;
  rest: string;
};

const MIN_WIDGET_HEIGHT = 120;
const MAX_WIDGET_HEIGHT = 900;

function parseOpeningFence(line: string): Fence | null {
  const match = /^( {0,3})(`{3,}|~{3,})(.*)$/.exec(line);
  if (!match) return null;
  const marker = match[2];
  const info = match[3];
  if (marker[0] === "`" && info.includes("`")) return null;
  if (marker[0] === "~" && info.includes("~")) return null;
  const trimmed = info.trim();
  const space = trimmed.search(/\s/);
  const lang = (space < 0 ? trimmed : trimmed.slice(0, space)).toLowerCase();
  const rest = space < 0 ? "" : trimmed.slice(space + 1).trim();
  return { char: marker[0], len: marker.length, lang, rest };
}

function isClosingFence(line: string, fence: Fence): boolean {
  const match = /^( {0,3})(`{3,}|~{3,})[ \t]*$/.exec(line);
  if (!match) return false;
  return match[2][0] === fence.char && match[2].length >= fence.len;
}

function parseMeta(rest: string): { title?: string; height?: number } {
  let title: string | undefined;
  let height: number | undefined;
  let cursor = 0;

  while (cursor < rest.length) {
    while (cursor < rest.length && /\s/.test(rest[cursor])) cursor++;
    const keyStart = cursor;
    while (cursor < rest.length && !/[\s=]/.test(rest[cursor])) cursor++;
    const key = rest.slice(keyStart, cursor).toLowerCase();
    if (rest[cursor] !== "=") {
      while (cursor < rest.length && !/\s/.test(rest[cursor])) cursor++;
      continue;
    }
    cursor++;
    let value = "";
    const quote = rest[cursor];
    if (quote === '"' || quote === "'") {
      cursor++;
      while (cursor < rest.length) {
        if (rest[cursor] === "\\" && cursor + 1 < rest.length) {
          cursor++;
          value += rest[cursor];
          cursor++;
          continue;
        }
        if (rest[cursor] === quote) {
          cursor++;
          break;
        }
        value += rest[cursor];
        cursor++;
      }
    } else {
      const valStart = cursor;
      while (cursor < rest.length && !/\s/.test(rest[cursor])) cursor++;
      value = rest.slice(valStart, cursor);
    }

    if (key === "title") {
      const trimmed = value.trim();
      if (trimmed) title = trimmed;
    } else if (key === "height") {
      const numeric = Number.parseInt(value.replace(/px$/i, ""), 10);
      if (Number.isFinite(numeric) && numeric > 0) {
        height = Math.max(MIN_WIDGET_HEIGHT, Math.min(MAX_WIDGET_HEIGHT, numeric));
      }
    }
  }

  return { title, height };
}

function extractHtmlTitle(code: string): string | undefined {
  const match = /<title\b[^>]*>([\s\S]*?)<\/title>/i.exec(code);
  if (!match) return undefined;
  const text = match[1].replace(/\s+/g, " ").trim();
  return text || undefined;
}

/**
 * 将 Markdown 文本拆分为顺序保留的 `markdown` 与 `widget` 片段。
 * - 快速路径：不含 `widget` 顶层围栏标记时零拷贝返回单个 `markdown` 段。
 * - 遇到非 `widget` 围栏时整体跳过到其闭合行，防止普通代码块内的 `widget` 示例被误切。
 */
export function splitWidgetParts(markdown: string): MessagePart[] {
  const normalized = markdown.replace(/\r\n/g, "\n");
  if (!/^ {0,3}(?:`{3,}|~{3,})[ \t]*widget(?=$|\s)/im.test(normalized)) {
    return [{ kind: "markdown", text: normalized }];
  }

  const parts: MessagePart[] = [];
  let proseStart = 0;
  let lineStart = 0;
  let widgetIndex = 0;

  const pushMarkdown = (text: string) => {
    if (!text) return;
    const prev = parts.at(-1);
    if (prev?.kind === "markdown") {
      prev.text = prev.text ? `${prev.text}\n${text}` : text;
    } else {
      parts.push({ kind: "markdown", text });
    }
  };

  while (lineStart <= normalized.length) {
    const newline = normalized.indexOf("\n", lineStart);
    const lineEnd = newline < 0 ? normalized.length : newline;
    const line = normalized.slice(lineStart, lineEnd);
    const fence = parseOpeningFence(line);

    if (!fence) {
      if (newline < 0) break;
      lineStart = newline + 1;
      continue;
    }

    // 扫描到匹配的闭合围栏。
    let scan = newline < 0 ? normalized.length + 1 : newline + 1;
    let closeStart = -1;
    let closeEnd = normalized.length;
    while (scan <= normalized.length) {
      const nextNl = normalized.indexOf("\n", scan);
      const end = nextNl < 0 ? normalized.length : nextNl;
      if (isClosingFence(normalized.slice(scan, end), fence)) {
        closeStart = scan;
        closeEnd = end;
        break;
      }
      if (nextNl < 0) break;
      scan = nextNl + 1;
    }

    if (fence.lang !== "widget") {
      // 普通代码块：整体略过，保持在 markdown 缓冲区中。
      if (closeStart < 0) break;
      if (closeEnd >= normalized.length) break;
      lineStart = closeEnd + 1;
      continue;
    }

    // 把组件前面的普通 Markdown 片段推入。
    if (proseStart < lineStart) {
      pushMarkdown(normalized.slice(proseStart, lineStart - 1));
    }

    const bodyStart = newline < 0 ? normalized.length : newline + 1;
    const complete = closeStart >= 0;
    const bodyEnd = complete
      ? Math.max(bodyStart, closeStart - 1)
      : normalized.length;
    const code = bodyStart <= bodyEnd ? normalized.slice(bodyStart, bodyEnd) : "";
    const meta = parseMeta(fence.rest);
    const title = meta.title ?? extractHtmlTitle(code);

    parts.push({
      kind: "widget",
      index: widgetIndex++,
      title,
      height: meta.height,
      code,
      complete,
    });

    if (!complete) {
      return parts.length > 0 ? parts : [{ kind: "markdown", text: "" }];
    }
    proseStart = closeEnd + 1;
    if (proseStart > normalized.length) break;
    lineStart = proseStart;
  }

  if (proseStart <= normalized.length) {
    pushMarkdown(normalized.slice(proseStart));
  }

  return parts.length > 0 ? parts : [{ kind: "markdown", text: "" }];
}

/** 生成用于下载的 `.html` 安全文件名。 */
export function sanitizeFilename(title: string): string {
  const cleaned = title
    .trim()
    .replace(/[\\/:*?"<>|\u0000-\u001f]+/g, "-")
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 64);
  return `${cleaned || "moc-widget"}.html`;
}
