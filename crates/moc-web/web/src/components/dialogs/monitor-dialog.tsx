// `/monitor`：moc web 进程资源监控。样本来自服务端 procstats 环形缓冲
// （1Hz × 10 分钟），打开期间每 2s 拉取一次。

import { useEffect, useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { fetchProcStats, type ProcSample, type ProcStats } from "@/lib/web-api";
import { useUi } from "@/store/ui";

function formatBytes(bytes: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 100 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/** 相邻样本差分得到每秒速率；计数器回退或时间倒退的样本对跳过。 */
function rates(samples: ProcSample[], field: (sample: ProcSample) => number): number[] {
  const out: number[] = [];
  for (let i = 1; i < samples.length; i += 1) {
    const dt = (samples[i - 1].ageMs - samples[i].ageMs) / 1000;
    const delta = field(samples[i]) - field(samples[i - 1]);
    if (dt > 0 && delta >= 0) out.push(delta / dt);
  }
  return out;
}

type Series = { points: number[]; className: string };

/** 共享纵轴刻度的折线：同一图里的多条序列按合并 min/max 归一。 */
function Spark({ series }: { series: Series[] }) {
  const width = 200;
  const height = 36;
  const all = series.flatMap((item) => item.points);
  if (all.length < 2) {
    return <div className="flex h-9 items-center text-2xs text-text-tertiary">样本不足…</div>;
  }
  const max = Math.max(...all);
  const min = Math.min(...all);
  const span = max - min || 1;
  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      aria-hidden="true"
      className="h-9 w-full text-brand"
    >
      {series.map((item) => (
        <polyline
          key={item.className}
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          vectorEffect="non-scaling-stroke"
          className={item.className}
          points={item.points
            .map(
              (value, i) =>
                `${(i / (item.points.length - 1)) * width},${
                  height - 2 - ((value - min) / span) * (height - 4)
                }`,
            )
            .join(" ")}
        />
      ))}
    </svg>
  );
}

function Chart({
  title,
  value,
  legend,
  series,
}: {
  title: string;
  value: string;
  legend?: { label: string; className: string }[];
  series: Series[];
}) {
  return (
    <div className="flex flex-col gap-1.5 rounded-lg border border-border/60 px-3 py-2.5">
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs text-muted-foreground">{title}</span>
        <span className="font-mono text-xs font-medium">{value}</span>
      </div>
      <Spark series={series} />
      {legend && (
        <div className="flex gap-3">
          {legend.map((item) => (
            <span
              key={item.label}
              className={`flex items-center gap-1 text-2xs text-muted-foreground ${item.className}`}
            >
              <span className="inline-block h-px w-3 bg-current" />
              {item.label}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

export function MonitorDialog() {
  const [stats, setStats] = useState<ProcStats | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      fetchProcStats()
        .then((result) => {
          if (alive) {
            setStats(result);
            setError(null);
          }
        })
        .catch((failure: unknown) => {
          if (alive) setError(failure instanceof Error ? failure.message : String(failure));
        });
    void load();
    const timer = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);

  const samples = stats?.samples ?? [];
  const latest = samples.at(-1);

  return (
    <Dialog open onOpenChange={(open) => !open && useUi.getState().set({ monitor: false })}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>进程资源监控</DialogTitle>
          <DialogDescription>
            moc web 进程近 {stats ? Math.round(stats.windowSecs / 60) : 10} 分钟，1Hz
            采样；网络为收发的 HTTP(S) 流量。
          </DialogDescription>
        </DialogHeader>
        <div className="min-h-0 overflow-y-auto">
          {error ? (
            <p role="alert" className="text-sm text-destructive">
              读取失败：{error}
            </p>
          ) : !stats ? (
            <p role="status" className="text-sm text-muted-foreground">
              正在读取样本…
            </p>
          ) : samples.length === 0 ? (
            <p className="text-sm text-muted-foreground">尚无样本，稍等几秒。</p>
          ) : (
            <div className="grid gap-3 sm:grid-cols-2">
              <Chart
                title="常驻内存"
                value={latest ? formatBytes(latest.rssBytes) : "—"}
                series={[{ points: samples.map((s) => s.rssBytes), className: "text-brand" }]}
              />
              <Chart
                title="CPU（单核=100%）"
                value={latest ? `${latest.cpuPct.toFixed(0)}%` : "—"}
                series={[{ points: samples.map((s) => s.cpuPct), className: "text-warn" }]}
              />
              <Chart
                title="磁盘读写"
                value={
                  latest
                    ? `${formatBytes(latest.diskReadBytes)} / ${formatBytes(latest.diskWriteBytes)}`
                    : "—"
                }
                legend={[
                  { label: "读/s", className: "text-brand" },
                  { label: "写/s", className: "text-warn" },
                ]}
                series={[
                  { points: rates(samples, (s) => s.diskReadBytes), className: "text-brand" },
                  { points: rates(samples, (s) => s.diskWriteBytes), className: "text-warn" },
                ]}
              />
              <Chart
                title="网络收发"
                value={
                  latest
                    ? `${formatBytes(latest.netRxBytes)} / ${formatBytes(latest.netTxBytes)}`
                    : "—"
                }
                legend={[
                  { label: "收/s", className: "text-brand" },
                  { label: "发/s", className: "text-warn" },
                ]}
                series={[
                  { points: rates(samples, (s) => s.netRxBytes), className: "text-brand" },
                  { points: rates(samples, (s) => s.netTxBytes), className: "text-warn" },
                ]}
              />
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
