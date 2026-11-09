// 状态条浮层：上下文圆环 → 上下文 Popover，费用 chip → 用量 Popover，
// 子代理 chip → 子代理 Popover（承载原 AgentsTab 全部功能）。
// 受控状态在 useUi.statusPopover；打开时对应 chip 必须渲染出来（计数可为 0）。

import { toast } from "sonner";
import { AgentsTab, UsageTab } from "@/components/panels/inspector-tabs";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { compactTokens } from "@/lib/models";
import { cn } from "@/lib/utils";
import { compactSession, refreshSideData } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function popoverFor(sessionId: string, kind: "context" | "usage" | "agents") {
  return {
    open:
      useUi.getState().statusPopover?.sessionId === sessionId &&
      useUi.getState().statusPopover?.kind === kind,
    set: (open: boolean) =>
      useUi.getState().set({ statusPopover: open ? { sessionId, kind } : null }),
  };
}

/** 上下文圆环 + Popover：分段横条、图例、压缩与「查看完整上下文」。 */
export function ContextRingPopover({
  sessionId,
  used,
  window: windowSize,
}: {
  sessionId: string;
  used: number;
  window: number;
}) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const controlled = useUi((state) => state.statusPopover);
  const open = controlled?.sessionId === sessionId && controlled.kind === "context";
  const percent = Math.min(100, Math.round((used / windowSize) * 100));
  const sections = view?.context?.sections ?? [];
  const isDraft = sessionId === "draft-session";
  const disabled = isDraft || Boolean(view?.readOnly) || Boolean(view?.running);
  const size = 18;
  const stroke = 2;
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;

  const setOpen = (next: boolean) => {
    if (next) refreshSideData(sessionId);
    popoverFor(sessionId, "context").set(next);
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="flex size-7 items-center justify-center rounded-lg hover:bg-accent"
          aria-label={`上下文已用 ${percent}%`}
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
      </PopoverTrigger>
      <PopoverContent side="top" align="end" sideOffset={8} className="w-80 p-4">
        <p className="text-title font-medium">上下文</p>
        <p className="mt-1 text-heading font-semibold tabular-nums">
          {compactTokens(used) || "0"} / {compactTokens(windowSize)}
          <span className="ml-2 text-sm font-normal text-muted-foreground">{percent}%</span>
        </p>
        {sections.length > 0 && (
          <>
            <div className="mt-3 flex h-1.5 w-full overflow-hidden rounded-full">
              {sections.map((section, index) => (
                <div
                  key={section.name}
                  className={cn(
                    "h-full",
                    section.name === "messages"
                      ? "bg-brand"
                      : SECTION_GRAY[index % SECTION_GRAY.length],
                  )}
                  style={{ width: `${Math.max(1, (section.tokens / Math.max(used, 1)) * 100)}%` }}
                />
              ))}
            </div>
            <ul className="mt-2.5 space-y-1">
              {sections.map((section, index) => (
                <li key={section.name} className="flex items-center gap-2 text-xs">
                  <span
                    className={cn(
                      "size-2 rounded-full",
                      section.name === "messages"
                        ? "bg-brand"
                        : SECTION_GRAY[index % SECTION_GRAY.length],
                    )}
                  />
                  <span className="min-w-0 flex-1 text-muted-foreground">{section.name}</span>
                  <span className="font-mono tabular-nums text-text-tertiary">
                    {compactTokens(section.tokens)} ·{" "}
                    {Math.round((section.tokens / Math.max(used, 1)) * 100)}%
                  </span>
                </li>
              ))}
            </ul>
          </>
        )}
        <div className="mt-4 flex gap-2">
          <Button
            size="xs"
            variant="outline"
            disabled={disabled}
            onClick={() =>
              void compactSession(sessionId)
                .then((result) =>
                  toast.success(
                    `上下文已压缩：${result.tokens_before.toLocaleString()} → ${result.tokens_after.toLocaleString()} tokens`,
                  ),
                )
                .catch((error) => toast.error(`压缩失败：${errorText(error)}`))
            }
          >
            压缩
          </Button>
          <Button
            size="xs"
            variant="outline"
            onClick={() => {
              popoverFor(sessionId, "context").set(false);
              useUi.getState().set({ contextDialog: sessionId });
            }}
          >
            查看完整上下文
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

const SECTION_GRAY = [
  "bg-foreground/30",
  "bg-foreground/20",
  "bg-foreground/12",
  "bg-foreground/8",
];

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
