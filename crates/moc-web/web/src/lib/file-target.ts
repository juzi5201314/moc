// 文件/资源目标的解析纯函数：Markdown 链接、工具调用参数 → openFile 的统一目标。
//
// 支持三种来源：
// - `local://` `artifact://` `history://` `skill://` `tool://` 这类会话空间 URI；
// - `/abs/path`、`~/path`、相对路径（相对当前会话 cwd 解析）；
// - `path:12` 或 `path#L12` 行号后缀（path 本身含 `:` 时优先按绝对路径判断）。

/** openFile 的目标：工作区文件或会话空间资源。 */
export type FileTarget = { kind: "fs"; path: string } | { kind: "resource"; uri: string };

/** 会话空间 URI 的 scheme 表（与 core 资源路由一致）。 */
const RESOURCE_SCHEMES = ["local", "artifact", "history", "skill", "tool"] as const;

export type ParsedLink = {
  target: FileTarget;
  /** 1 起始的行号；未写行号为 undefined。 */
  line?: number;
};

/** 是不是会话空间 URI（`local://`、`artifact://jobs/x` 等）。 */
export function isResourceUri(href: string): boolean {
  const scheme = href.split("://", 1)[0];
  return (RESOURCE_SCHEMES as readonly string[]).includes(scheme) && href.includes("://");
}

/** 去掉末尾的行号后缀：`:12`（数字）或 `#L12`（含 `#12`）。 */
export function splitLineRef(href: string): { path: string; line?: number } {
  const hash = /#L?(\d+)$/.exec(href);
  if (hash) return { path: href.slice(0, hash.index), line: Number(hash[1]) };
  // Windows 盘符 `C:\` 也算冒号；只在冒号后是纯数字且前面至少有 1 个字符时拆。
  const colon = /^(.+?):(\d+)$/.exec(href);
  if (colon && !/^[A-Za-z]$/.test(colon[1])) return { path: colon[1], line: Number(colon[2]) };
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
  const { path, line } = splitLineRef(trimmed);
  if (isResourceUri(path)) return { target: { kind: "resource", uri: path }, line };
  if (/^[a-zA-Z][\w+.-]*:/.test(path)) return null;
  return { target: { kind: "fs", path: resolvePath(path, cwd) }, line };
}

/** 链接目标是本地文件路径（浏览器里点不开：裸路径变同源 404、file:// 被拦）。 */
export function isLocalFileRef(href: string): boolean {
  const target = href.trim();
  if (/^file:\/\//i.test(target)) return true;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(target) || target.startsWith("#")) return false;
  return true;
}

/** 本地路径链接里剥出路径本体（file:// → 裸路径）。 */
export function localFileRefPath(href: string): string {
  const target = href.trim();
  if (!/^file:\/\//i.test(target)) return target;
  try {
    return decodeURIComponent(new URL(target).pathname);
  } catch {
    return target;
  }
}

/** 资源 URI / 行号路径经 rehype-sanitize 会剥掉 href；remark 层先改写成哨兵 fragment。 */
export const RESOURCE_SENTINEL = "#moc-res:";
export const FS_SENTINEL = "#moc-fs:";

/**
 * MarkdownAnchor 的目标判定：href + cwd → { target, line } | null。
 * 集中一处纯函数，组件渲染时只算一次。
 */
export function resolveAnchorTarget(href: string | undefined, cwd: string): ParsedLink | null {
  if (!href) return null;
  if (href.startsWith(RESOURCE_SENTINEL)) {
    const { path, line } = splitLineRef(href.slice(RESOURCE_SENTINEL.length));
    return { target: { kind: "resource", uri: path }, line };
  }
  if (href.startsWith(FS_SENTINEL)) {
    return parseLinkTarget(localFileRefPath(href.slice(FS_SENTINEL.length)), cwd);
  }
  if (isResourceUri(href)) {
    const { path, line } = splitLineRef(href);
    return { target: { kind: "resource", uri: path }, line };
  }
  if (isLocalFileRef(href)) {
    return parseLinkTarget(localFileRefPath(href), cwd);
  }
  return null;
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
