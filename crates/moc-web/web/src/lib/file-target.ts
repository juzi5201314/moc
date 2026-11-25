// 文件/资源目标的解析纯函数：Markdown 链接、裸内部 URI、工具调用参数 → openFile/revealDir 统一目标。
//
// 支持三种来源：
// - `local://` `artifact://` `history://` `skill://` `tool://` 这类会话空间 URI；
// - `/abs/path`、`~/path`、相对路径（相对当前会话 cwd 解析）；
// - `path:12`、`path:12-20`、`path#L12`、`path#L12-L20` 行号/行范围后缀（path 本身含 `:` 时优先按绝对路径判断）。

/** openFile 的目标：工作区文件或会话空间资源。 */
export type FileTarget = { kind: "fs"; path: string } | { kind: "resource"; uri: string };

/** 会话空间 URI 的 scheme 表（与 core 资源路由一致）。 */
const RESOURCE_SCHEMES = ["local", "artifact", "history", "skill", "tool"] as const;
export type ResourceScheme = (typeof RESOURCE_SCHEMES)[number];

export type ParsedLink = {
  target: FileTarget;
  /** 1 起始的行号；未写行号为 undefined。 */
  line?: number;
  /** 1 起始的结束行号（仅区间大于起始行时存在）。 */
  endLine?: number;
  /** 原始链接以 `/` 结尾时判定为目录引用，点击改为在文件树里展开定位。 */
  isDir?: boolean;
};

/** 是不是会话空间 URI（`local://`、`artifact://jobs/x` 等）。 */
export function isResourceUri(href: string): boolean {
  const scheme = href.split("://", 1)[0];
  return (RESOURCE_SCHEMES as readonly string[]).includes(scheme) && href.includes("://");
}

/** 提取会话空间 URI 的 scheme（即对应的资源引用 kind）。 */
export function resourceKindFromUri(uri: string): ResourceScheme | null {
  const scheme = uri.split("://", 1)[0];
  return (RESOURCE_SCHEMES as readonly string[]).includes(scheme) && uri.includes("://")
    ? (scheme as ResourceScheme)
    : null;
}

/** 去掉末尾的行号或行范围后缀：`:12` / `:12-20` / `#L12` / `#L12-L20` / `#L12-20` / `#12-20`。 */
export function splitLineRef(href: string): { path: string; line?: number; endLine?: number } {
  const hash = /#L?(\d+)(?:-L?(\d+))?$/.exec(href);
  if (hash) {
    const line = Number(hash[1]);
    const end = hash[2] !== undefined ? Number(hash[2]) : undefined;
    return end !== undefined && end > line
      ? { path: href.slice(0, hash.index), line, endLine: end }
      : { path: href.slice(0, hash.index), line };
  }
  // Windows 盘符 `C:\` 也算冒号；只在冒号后是纯数字或数字区间且前面至少有 1 个字符时拆。
  const colon = /^(.+?):(\d+)(?:-(\d+))?$/.exec(href);
  if (colon && !/^[A-Za-z]$/.test(colon[1])) {
    const line = Number(colon[2]);
    const end = colon[3] !== undefined ? Number(colon[3]) : undefined;
    return end !== undefined && end > line
      ? { path: colon[1], line, endLine: end }
      : { path: colon[1], line };
  }
  return { path: href };
}

/** 相对路径按 cwd 解析成绝对路径；`.`/`..` 段折叠，与树上节点 key 对齐。 */
export function resolvePath(path: string, cwd: string): string {
  const joined =
    path.startsWith("/") || path.startsWith("~") || /^[A-Za-z]:[\\/]/.test(path)
      ? path
      : `${cwd.endsWith("/") ? cwd.slice(0, -1) : cwd}/${path}`;
  if (joined.startsWith("~")) return joined;
  const out: string[] = [];
  for (const segment of joined.split("/")) {
    if (segment === "..") out.pop();
    else if (segment !== ".") out.push(segment);
  }
  return out.join("/");
}

/**
 * 把 Markdown 链接 / 工具参数里的路径解析成 openFile 目标。
 * 非路径形式（http、mailto、anchor 等）返回 null。
 */
export function parseLinkTarget(href: string, cwd: string): ParsedLink | null {
  const trimmed = href.trim();
  if (!trimmed || trimmed.startsWith("#")) return null;
  // `scheme:` 已知协议先挡掉（tel:1 不能拆成 tel + 行号 1）；路径里的 :12 不在名单内。
  const scheme = /^([a-zA-Z][\w+.-]*):/.exec(trimmed)?.[1]?.toLowerCase();
  if (scheme && KNOWN_SCHEMES.has(scheme)) return null;
  const { path, line, endLine } = splitLineRef(trimmed);
  if (isResourceUri(path)) {
    const result: ParsedLink = { target: { kind: "resource", uri: path } };
    if (line !== undefined) result.line = line;
    if (endLine !== undefined) result.endLine = endLine;
    return result;
  }
  if (/^[a-zA-Z][\w+.-]*:/.test(path)) return null;
  const isDir = path.length > 1 && path.endsWith("/");
  const cleanPath = isDir ? path.replace(/\/+$/, "") : path;
  const result: ParsedLink = { target: { kind: "fs", path: resolvePath(cleanPath, cwd) } };
  if (line !== undefined) result.line = line;
  if (endLine !== undefined) result.endLine = endLine;
  if (isDir) result.isDir = true;
  return result;
}

/** 链接目标是本地文件路径（浏览器里点不开：裸路径变同源 404、file:// 被拦）。 */
export function isLocalFileRef(href: string): boolean {
  const target = href.trim();
  if (/^file:\/\//i.test(target)) return true;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(target) || target.startsWith("#")) return false;
  return true;
}

/** Markdown 渲染后的 href 已百分号编码（空格、中文等）；还原成真实路径，非法序列原样保留。 */
function decodeHref(href: string): string {
  try {
    return decodeURIComponent(href);
  } catch {
    return href;
  }
}

/** 本地路径链接里剥出路径本体（file:// → 裸路径并保留 `#L…` 行号），并解开百分号编码。 */
export function localFileRefPath(href: string): string {
  const target = href.trim();
  if (!/^file:\/\//i.test(target)) return decodeHref(target);
  try {
    const parsed = new URL(target);
    return `${decodeHref(parsed.pathname)}${parsed.hash}`;
  } catch {
    return target;
  }
}

/** 资源 URI / 行号路径经 rehype-sanitize 会剥掉 href；remark 层先改写成哨兵 fragment。 */
export const RESOURCE_SENTINEL = "#moc-res:";
export const FS_SENTINEL = "#moc-fs:";
/** 裸正文自动扫描到的候选内部 URI：渲染前须经 `resource/inspect` 确认真实存在才转胶囊。 */
export const AUTO_RESOURCE_SENTINEL = "#moc-auto-res:";

/**
 * MarkdownAnchor 的目标判定：href + cwd → { target, line, endLine, isDir } | null。
 * 集中一处纯函数，组件渲染时只算一次。
 */
export function resolveAnchorTarget(href: string | undefined, cwd: string): ParsedLink | null {
  if (!href) return null;
  if (href.startsWith(RESOURCE_SENTINEL)) {
    const { path, line, endLine } = splitLineRef(decodeHref(href.slice(RESOURCE_SENTINEL.length)));
    const result: ParsedLink = { target: { kind: "resource", uri: path } };
    if (line !== undefined) result.line = line;
    if (endLine !== undefined) result.endLine = endLine;
    return result;
  }
  if (href.startsWith(FS_SENTINEL)) {
    return parseLinkTarget(localFileRefPath(href.slice(FS_SENTINEL.length)), cwd);
  }
  if (isResourceUri(href)) {
    const { path, line, endLine } = splitLineRef(decodeHref(href));
    const result: ParsedLink = { target: { kind: "resource", uri: path } };
    if (line !== undefined) result.line = line;
    if (endLine !== undefined) result.endLine = endLine;
    return result;
  }
  if (isLocalFileRef(href)) {
    return parseLinkTarget(localFileRefPath(href), cwd);
  }
  return null;
}

/** 判断 href 是否是外链（http/https/mailto）；过滤 Streamdown 的流式占位链接。 */
export function externalLinkKind(href: string | undefined): "external" | "mailto" | null {
  if (!href || href === "streamdown:incomplete-link") return null;
  const trimmed = href.trim();
  if (/^https?:\/\//i.test(trimmed)) return "external";
  if (/^mailto:/i.test(trimmed)) return "mailto";
  return null;
}

/** 胶囊右侧的行号附注文案（如 `L9` / `L532-725`）。 */
export function formatLineBadge(line?: number, endLine?: number): string | undefined {
  if (line === undefined) return undefined;
  return endLine !== undefined && endLine > line ? `L${line}-${endLine}` : `L${line}`;
}

/** 判断可见锚文本是否已写出对应行号，避免胶囊内重复叠加行号附注。 */
export function labelMentionsLine(text: string, line: number, endLine?: number): boolean {
  const escaped = String(line);
  if (endLine !== undefined && endLine > line) {
    if (new RegExp(`(?:^|[:#L\\s-])${escaped}\\s*[-–~]\\s*L?${endLine}(?:\\b|$)`).test(text)) {
      return true;
    }
  }
  return new RegExp(`(?::|#L?|\\bL)${escaped}(?:\\b|[-–]|$)`).test(text);
}

/** 供 Tooltip 展示的干净目标路径（剥掉内部哨兵，工作区路径优先相对 cwd）。 */
export function formatTargetDetail(parsed: ParsedLink, cwd: string): string {
  const suffix =
    parsed.line !== undefined
      ? parsed.endLine !== undefined && parsed.endLine > parsed.line
        ? `:${parsed.line}-${parsed.endLine}`
        : `:${parsed.line}`
      : "";
  if (parsed.target.kind === "resource") return `${parsed.target.uri}${suffix}`;
  const full = parsed.target.path;
  const base = cwd.replace(/\/+$/, "");
  const shown = base && full.startsWith(`${base}/`) ? full.slice(base.length + 1) : full;
  return `${shown}${parsed.isDir ? "/" : ""}${suffix}`;
}

/**
 * 裸正文内部 URI 候选扫描：仅提取形如 `scheme://authority[/path][line]` 的具体资源，
 * 排除命名空间根（`skill://`）与占位符（`local://<id>.md`）；且允许紧邻中文字符出现。
 * 是否真实存在必须再经 `resource/inspect` 核验。
 */
export type InternalUriSpan = {
  from: number;
  to: number;
  raw: string;
  uri: string;
  line?: number;
  endLine?: number;
};

const BARE_INTERNAL_URI =
  /(local|artifact|history|skill|tool):\/\/([A-Za-z0-9._~/%@+-]+(?::\d+(?:-\d+)?|#L?\d+(?:-L?\d+)?)?)/g;
const URI_LEFT_BOUNDARY = /[A-Za-z0-9_+.-]/;
const TRAILING_PUNCT = /[.,;:)\]'"!?]+$/;

/** 检查 authority 是否是有效的具体资源（非空、非 `.`/`..`、不含双斜杠）。 */
function hasConcreteResourceBody(uri: string): boolean {
  const after = uri.slice(uri.indexOf("://") + 3);
  if (!after || after.startsWith("/") || after.includes("//")) return false;
  if (/[<>\\?]/.test(after)) return false;
  const segments = after.split("/");
  return segments.every((seg) => seg.length > 0 && seg !== "." && seg !== "..");
}

export function scanInternalUris(text: string): InternalUriSpan[] {
  if (!text.includes("://")) return [];
  const spans: InternalUriSpan[] = [];
  BARE_INTERNAL_URI.lastIndex = 0;
  for (const match of text.matchAll(BARE_INTERNAL_URI)) {
    const start = match.index;
    if (start > 0 && URI_LEFT_BOUNDARY.test(text[start - 1])) continue;
    let raw = match[0];
    // 去掉紧贴末尾的英文句末标点（如 `See local://plan_1.md.`），保留合法行号后缀。
    const stripped = raw.replace(TRAILING_PUNCT, "");
    if (!stripped) continue;
    raw = stripped;
    const { path, line, endLine } = splitLineRef(raw);
    if (!isResourceUri(path) || !hasConcreteResourceBody(path)) continue;
    const span: InternalUriSpan = {
      from: start,
      to: start + raw.length,
      raw,
      uri: path,
    };
    if (line !== undefined) span.line = line;
    if (endLine !== undefined) span.endLine = endLine;
    spans.push(span);
  }
  return spans;
}

/**
 * 行内代码内部 URI 候选判定：只有整段代码恰好是一个具体内部 URI（可带行号后缀）时才命中，
 * 含空白的句子（如 `` `该moc工具使用如skill://skill-a来表示skill a` ``）直接返回 null。
 */
export function matchSingleInternalUri(
  text: string,
): { raw: string; uri: string; line?: number; endLine?: number } | null {
  const trimmed = text.trim();
  if (!trimmed || /\s/.test(trimmed) || !isResourceUri(trimmed)) return null;
  const { path, line, endLine } = splitLineRef(trimmed);
  if (!isResourceUri(path) || !hasConcreteResourceBody(path)) return null;
  const result: { raw: string; uri: string; line?: number; endLine?: number } = {
    raw: trimmed,
    uri: path,
  };
  if (line !== undefined) result.line = line;
  if (endLine !== undefined) result.endLine = endLine;
  return result;
}

/** 已注册的外部协议：识别后不接管，交回默认 <a> 行为。 */
const KNOWN_SCHEMES = new Set([
  "http",
  "https",
  "mailto",
  "tel",
  "ftp",
  "ftps",
  "file",
  "javascript",
  "data",
  "blob",
  "ws",
  "wss",
  "ssh",
]);
