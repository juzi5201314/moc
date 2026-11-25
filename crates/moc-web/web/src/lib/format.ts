// 展示用的小型格式化函数：相对时间、耗时、token 数。

import { currentLocale, t } from "@/lib/i18n";

/** 「刚刚 / 41分钟 / 3小时 / 1天 / 2天」（en: `just now / 41m / 3h / 1d / 2mo`）。 */
export function relativeTime(atMs: number, now = Date.now()): string {
  const diff = Math.max(0, now - atMs);
  if (diff < 60_000) return t("rel.now");
  if (diff < 3_600_000) return t("rel.min", { n: Math.floor(diff / 60_000) });
  if (diff < 86_400_000) return t("rel.hour", { n: Math.floor(diff / 3_600_000) });
  if (diff < 30 * 86_400_000) return t("rel.day", { n: Math.floor(diff / 86_400_000) });
  if (diff < 365 * 86_400_000) return t("rel.month", { n: Math.floor(diff / (30 * 86_400_000)) });
  return t("rel.year", { n: Math.floor(diff / (365 * 86_400_000)) });
}

/** 毫秒 → `850ms` / `3.2s` / `2m05s` / `1h05m`（与 TUI `labels::duration` 一致）。 */
export function formatDuration(ms: number): string {
  const safe = Math.max(0, Math.round(ms));
  if (safe < 1000) return `${safe}ms`;
  if (safe < 60_000) return `${Math.floor(safe / 1000)}.${Math.floor((safe % 1000) / 100)}s`;
  if (safe < 3_600_000) {
    const minutes = Math.floor(safe / 60_000);
    const rest = Math.floor((safe % 60_000) / 1000);
    return `${minutes}m${String(rest).padStart(2, "0")}s`;
  }
  const hours = Math.floor(safe / 3_600_000);
  const minutes = Math.floor((safe % 3_600_000) / 60_000);
  return `${hours}h${String(minutes).padStart(2, "0")}m`;
}

/** token 数 → `980` / `12.3k` / `1.2M`。 */
export function formatTokens(count: number): string {
  if (count < 1000) return String(count);
  if (count < 1_000_000) return `${(count / 1000).toFixed(count < 10_000 ? 1 : 0)}k`;
  return `${(count / 1_000_000).toFixed(1)}M`;
}

export function clockTime(atMs: number): string {
  return new Date(atMs).toLocaleTimeString(currentLocale() === "zh" ? "zh-CN" : "en-US", {
    hour: "2-digit",
    minute: "2-digit",
  });
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
