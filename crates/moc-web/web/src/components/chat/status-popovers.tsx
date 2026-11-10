// 状态条入口：上下文圆环打开统一面板，费用与子代理保留轻量浮层。
// 子代理 chip → 子代理 Popover（承载原 AgentsTab 全部功能）。
// 受控状态在 useUi.statusPopover；打开时对应 chip 必须渲染出来（计数可为 0）。

import { AgentsTab, UsageTab } from "@/components/panels/inspector-tabs";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";
import { useUi } from "@/store/ui";

function popoverFor(sessionId: string, kind: "usage" | "agents") {
  return {
    open:
      useUi.getState().statusPopover?.sessionId === sessionId &&
      useUi.getState().statusPopover?.kind === kind,
    set: (open: boolean) =>
      useUi.getState().set({ statusPopover: open ? { sessionId, kind } : null }),
  };
}

/** 上下文圆环：与 /context 使用同一入口。 */
export function ContextRing({
  sessionId,
  used,
  window: windowSize,
}: {
  sessionId: string;
  used: number;
  window: number;
}) {
  const percent = Math.min(100, Math.round((used / windowSize) * 100));
  const size = 18;
  const stroke = 2;
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;

  return (
    <button
      type="button"
      className="flex size-7 items-center justify-center rounded-lg hover:bg-accent"
      aria-label={`上下文已用 ${percent}%`}
      disabled={sessionId === "draft-session"}
      onClick={() => useUi.getState().set({ contextDialog: sessionId })}
    >
      <svg
        width={size}
        height={size}
        viewBox={`0 0 ${size} ${size}`}
        className={cn(
          "-rotate-90",
          percent >= 90
            ? "text-destructive"
            : percent >= 75
              ? "text-warn"
              : "text-muted-foreground",
        )}
        aria-hidden="true"
      >
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeOpacity={0.2}
          strokeWidth={stroke}
        />
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={circumference}
          strokeDashoffset={circumference * (1 - percent / 100)}
        />
      </svg>
    </button>
  );
}

/** 费用 chip → 用量 Popover（原 UsageTab 收紧到 w-80）。 */
export function UsagePopover({ sessionId, label }: { sessionId: string; label: string }) {
  const controlled = useUi((state) => state.statusPopover);
  const open = controlled?.sessionId === sessionId && controlled.kind === "usage";
  const setOpen = (next: boolean) => popoverFor(sessionId, "usage").set(next);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md px-1.5 font-mono tabular-nums transition-colors hover:text-foreground"
          title="查看本会话用量"
        >
          {label}
        </button>
      </PopoverTrigger>
      <PopoverContent side="top" align="end" sideOffset={8} className="w-80 p-0">
        <div className="max-h-[70vh] overflow-y-auto">
          <UsageTab key={sessionId} sessionId={sessionId} />
        </div>
      </PopoverContent>
    </Popover>
  );
}

/** 子代理 chip → 子代理 Popover（原 AgentsTab 全部功能）。 */
export function AgentsPopover({
  sessionId,
  running,
  total,
}: {
  sessionId: string;
  running: number;
  total: number;
}) {
  const controlled = useUi((state) => state.statusPopover);
  const open = controlled?.sessionId === sessionId && controlled.kind === "agents";
  const setOpen = (next: boolean) => popoverFor(sessionId, "agents").set(next);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md px-1.5 transition-colors hover:text-foreground"
          title="查看子代理"
        >
          {running > 0 && <span className="size-1.5 animate-pulse rounded-full bg-brand" />}
          {running > 0 ? `${running} 个子代理运行中` : `${total} 个子代理`}
        </button>
      </PopoverTrigger>
      <PopoverContent side="top" align="end" sideOffset={8} className="w-96 p-0">
        <div className="max-h-[70vh] overflow-y-auto">
          <AgentsTab key={sessionId} sessionId={sessionId} />
        </div>
      </PopoverContent>
    </Popover>
  );
}
