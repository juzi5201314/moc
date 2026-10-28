// 展示用的小型格式化函数：相对时间、耗时、token 数。

/** 「刚刚 / 41分钟 / 3小时 / 1天 / 2天」。与 DSH 保持完全一致。 */
export function relativeTime(atMs: number, now = Date.now()): string {
  const diff = Math.max(0, now - atMs);
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}分钟`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}小时`;
  if (diff < 30 * 86_400_000) return `${Math.floor(diff / 86_400_000)}天`;
  if (diff < 365 * 86_400_000) return `${Math.floor(diff / (30 * 86_400_000))}月`;
  return `${Math.floor(diff / (365 * 86_400_000))}年`;
}

/** 毫秒 → `850ms` / `3.2s` / `2m05s`。 */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = Math.floor(seconds % 60);
  return `${minutes}m${String(rest).padStart(2, "0")}s`;
}

/** token 数 → `980` / `12.3k` / `1.2M`。 */
export function formatTokens(count: number): string {
  if (count < 1000) return String(count);
  if (count < 1_000_000) return `${(count / 1000).toFixed(count < 10_000 ? 1 : 0)}k`;
  return `${(count / 1_000_000).toFixed(1)}M`;
}

export function clockTime(atMs: number): string {
  return new Date(atMs).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
}

export type DiffLineItem = { kind: "equal" | "insert" | "delete"; text: string };
export type SplitRow = { left?: DiffLineItem; right?: DiffLineItem };

export function toSplitRows(lines: DiffLineItem[]): SplitRow[] {
  const rows: SplitRow[] = [];
  let i = 0;
  while (i < lines.length) {
    const cur = lines[i];
    if (cur.kind === "equal") {
      rows.push({ left: cur, right: cur });
      i++;
    } else if (cur.kind === "delete") {
      // Collect consecutive deletes
      const deletes: DiffLineItem[] = [];
      while (i < lines.length && lines[i].kind === "delete") {
        deletes.push(lines[i]);
        i++;
      }
      // Collect consecutive inserts
      const inserts: DiffLineItem[] = [];
      while (i < lines.length && lines[i].kind === "insert") {
        inserts.push(lines[i]);
        i++;
      }
      const count = Math.max(deletes.length, inserts.length);
      for (let k = 0; k < count; k++) {
        rows.push({ left: deletes[k], right: inserts[k] });
      }
    } else if (cur.kind === "insert") {
      rows.push({ left: undefined, right: cur });
      i++;
    }
  }
  return rows;
}

export type Breadcrumb = { name: string; path: string };

export function breadcrumbs(path: string): Breadcrumb[] {
  if (!path) return [];
  const isWin = /^[a-zA-Z]:\\/.test(path);
  if (isWin) {
    const parts = path.split("\\").filter(Boolean);
    const crumbs: Breadcrumb[] = [];
    let current = "";
    for (let i = 0; i < parts.length; i++) {
      if (i === 0) {
        current = `${parts[i]}\\`;
        crumbs.push({ name: parts[i], path: current });
      } else {
        current = `${current}${current.endsWith("\\") ? "" : "\\"}${parts[i]}`;
        crumbs.push({ name: parts[i], path: current });
      }
    }
    return crumbs;
  }
  const parts = path.split("/").filter(Boolean);
  const crumbs: Breadcrumb[] = [{ name: "/", path: "/" }];
  let current = "";
  for (const part of parts) {
    current += `/${part}`;
    crumbs.push({ name: part, path: current });
  }
  return crumbs;
}
