// 展示用的小型格式化函数：相对时间、耗时、token 数。

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** 「刚刚 / 5 分钟前 / 3 小时前 / 2 天前 / 2026-10-01」。 */
export function relativeTime(atMs: number, now = Date.now()): string {
  const diff = Math.max(0, now - atMs);
  if (diff < MINUTE) return "刚刚";
  if (diff < HOUR) return `${Math.floor(diff / MINUTE)} 分钟前`;
  if (diff < DAY) return `${Math.floor(diff / HOUR)} 小时前`;
  if (diff < 7 * DAY) return `${Math.floor(diff / DAY)} 天前`;
  return new Date(atMs).toLocaleDateString("zh-CN");
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
