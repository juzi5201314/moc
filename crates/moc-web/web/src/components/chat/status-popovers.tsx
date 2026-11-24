// 状态条入口：上下文圆环弹简略上下文窗口（再进详细模态），费用与子代理保留轻量浮层。
// 子代理 chip → 子代理 Popover（承载原 AgentsTab 全部功能）。
// 受控状态在 useUi.statusPopover；打开时对应 chip 必须渲染出来（计数可为 0）。

import {
  AlertCircleIcon,
  ChevronRightIcon,
  Loader2Icon,
  RefreshCwIcon,
  Settings2Icon,
} from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { sectionTone } from "@/components/context/section-tone";
import { AgentsTab, UsageTab } from "@/components/panels/inspector-tabs";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useT } from "@/lib/i18n";
import { compactTokens } from "@/lib/models";
import { cn } from "@/lib/utils";
import type { ContextUsageResult, McpServerInfo, McpServerPhase } from "@/protocol/types";
import { selectFeatures, useConnection } from "@/store/connection";
import { mcpReconnectPending, useMcp } from "@/store/mcp";
import { refreshContext, useTranscript } from "@/store/transcript";
import { openSettings, useUi } from "@/store/ui";

function popoverFor(sessionId: string, kind: "usage" | "agents" | "mcp") {
  return {
    open:
      useUi.getState().statusPopover?.sessionId === sessionId &&
      useUi.getState().statusPopover?.kind === kind,
    set: (open: boolean) =>
      useUi.getState().set({ statusPopover: open ? { sessionId, kind } : null }),
  };
}

/**
 * 上下文圆环：点击弹出简略上下文窗口（占用条 + 分块图例），点别处收起；
 * 底部入口进入详细上下文窗口（与 /context 同一个模态）。
 */
export function ContextRing({
  sessionId,
  context,
}: {
  sessionId: string;
  context: ContextUsageResult;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const used = context.total_tokens;
  const windowSize = context.context_window;
  const percent = Math.min(100, Math.round((used / windowSize) * 100));
  const share = (tokens: number) => `${((tokens / windowSize) * 100).toFixed(1)}%`;
  const size = 18;
  const stroke = 2;
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        // 圆环数据只在会话事件后刷新，打开时重读一次，读不到沿用旧值。
        if (next) void refreshContext(sessionId).catch(() => undefined);
      }}
    >
      <Tooltip>
        <TooltipTrigger asChild>
          <PopoverTrigger asChild>
            <button
              type="button"
              className="flex size-7 items-center justify-center rounded-lg hover:bg-accent data-[state=open]:bg-accent"
              aria-label={t("pop.context", { n: percent })}
              disabled={sessionId === "draft-session"}
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
        </TooltipTrigger>
        {/* 圆环只有弧长，悬停给出精确用量，不必打开上下文窗口。 */}
        <TooltipContent side="top">
          {`${t("pop.context", { n: percent })} · ${compactTokens(used) || 0} / ${compactTokens(windowSize)}`}
        </TooltipContent>
      </Tooltip>
      <PopoverContent side="top" align="end" sideOffset={8} className="w-72 p-0">
        <div className="px-3.5 pt-3 pb-2">
          <div className="text-xs text-muted-foreground">{t("cv.title")}</div>
          <div className="mt-1 flex items-baseline justify-between gap-3 text-ui tabular-nums">
            <span className="font-medium text-foreground">
              {t("pop.ctx_used", { n: share(used) })}
            </span>
            <span className="text-muted-foreground">
              {compactTokens(used) || 0} / {compactTokens(windowSize)}
            </span>
          </div>
          {/* 轨道与 Free space 用前景色透明度：bg-muted 在深色下与浮层同色，会整段消失。 */}
          <div
            className="mt-2 flex h-1.5 overflow-hidden rounded-full bg-foreground/10"
            aria-hidden="true"
          >
            {context.sections.map((section, index) => {
              const tone = sectionTone(section.name, index);
              return (
                <div
                  key={section.name}
                  className={tone.className}
                  style={{ width: share(section.tokens), opacity: tone.opacity }}
                />
              );
            })}
          </div>
        </div>
        <ul className="px-3.5 pb-2">
          {[
            ...context.sections.map((section, index) => ({
              name: section.name,
              tokens: section.tokens,
              tone: sectionTone(section.name, index),
            })),
            {
              name: t("pop.ctx_free"),
              tokens: Math.max(0, windowSize - used),
              tone: { className: "bg-foreground/10", opacity: 1 },
            },
          ].map((row) => (
            <li key={row.name} className="flex h-7 items-center gap-2.5 text-ui">
              <span
                className={cn("size-2 shrink-0 rounded-[3px]", row.tone.className)}
                style={{ opacity: row.tone.opacity }}
              />
              <span className="min-w-0 flex-1 truncate text-foreground">{row.name}</span>
              <span className="tabular-nums text-muted-foreground">
                {compactTokens(row.tokens) || 0}
              </span>
              <span className="w-12 text-right tabular-nums text-foreground">
                {share(row.tokens)}
              </span>
            </li>
          ))}
        </ul>
        <button
          type="button"
          onClick={() => {
            setOpen(false);
            useUi.getState().set({ contextDialog: sessionId });
          }}
          className="flex h-9 w-full items-center gap-2 border-t border-border-subtle px-3.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          {t("pop.ctx_details")}
          <ChevronRightIcon className="ml-auto size-3.5" />
        </button>
      </PopoverContent>
    </Popover>
  );
}

/** 费用 chip → 用量 Popover（原 UsageTab 收紧到 w-80）。 */
export function UsagePopover({ sessionId, label }: { sessionId: string; label: string }) {
  const t = useT();
  const controlled = useUi((state) => state.statusPopover);
  const open = controlled?.sessionId === sessionId && controlled.kind === "usage";
  const setOpen = (next: boolean) => popoverFor(sessionId, "usage").set(next);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md px-1.5 font-mono tabular-nums transition-colors hover:text-foreground"
          title={t("pop.usage")}
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
  const t = useT();
  const controlled = useUi((state) => state.statusPopover);
  const open = controlled?.sessionId === sessionId && controlled.kind === "agents";
  const setOpen = (next: boolean) => popoverFor(sessionId, "agents").set(next);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md px-1.5 transition-colors hover:text-foreground"
          title={t("pop.agents")}
        >
          {running > 0 && <span className="size-1.5 animate-pulse rounded-full bg-brand" />}
          {running > 0 ? t("pop.agents_running", { n: running }) : t("pop.agents_n", { n: total })}
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

const MCP_SLOW_MS = 2000;
const MCP_WARN_SECS = 8;

function elapsedSecs(startedAtMs: number | null | undefined, nowMs: number): number | null {
  if (startedAtMs == null) return null;
  return Math.max(0, Math.floor((nowMs - startedAtMs) / 1000));
}

function mcpPhaseDot(phase: McpServerPhase): string {
  switch (phase) {
    case "connected":
      return "bg-success";
    case "connecting":
      return "bg-brand";
    case "reconnecting":
      return "bg-warn";
    case "failed":
      return "bg-destructive";
    case "disabled":
      return "bg-text-tertiary/50";
  }
}

function McpServerRow({
  server,
  nowMs,
  busy,
  onReconnect,
}: {
  server: McpServerInfo;
  nowMs: number;
  busy: boolean;
  onReconnect: (name: string) => void;
}) {
  const t = useT();
  const secs = elapsedSecs(server.started_at_ms, nowMs);
  const stderr = server.phase !== "connected" ? (server.recent_stderr ?? []).slice(-3) : [];
  const statusText = (() => {
    switch (server.phase) {
      case "connected":
        return t("pop.mcp_tools", { n: server.tool_count ?? 0 });
      case "connecting":
        return secs !== null ? t("pop.mcp_connecting_s", { s: secs }) : t("pop.mcp_connecting");
      case "reconnecting":
        return t("pop.mcp_reconnecting", { n: server.attempts ?? 1 });
      case "failed":
        return t("pop.mcp_failed");
      case "disabled":
        return t("pop.mcp_disabled");
    }
  })();

  return (
    <div className="space-y-1 px-3 py-2.5">
      <div className="flex items-center gap-2">
        {server.phase === "connecting" ? (
          <Loader2Icon className="size-3 shrink-0 animate-spin text-brand" />
        ) : (
          <span className={cn("size-1.5 shrink-0 rounded-full", mcpPhaseDot(server.phase))} />
        )}
        <span className="min-w-0 truncate text-ui font-medium text-foreground">{server.name}</span>
        <span className="shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-2xs text-text-tertiary">
          {server.transport}
        </span>
        <span
          className={cn(
            "ml-auto shrink-0 font-mono text-2xs tabular-nums",
            server.phase === "failed"
              ? "text-destructive"
              : server.phase === "reconnecting"
                ? "text-warn"
                : server.phase === "connecting"
                  ? "text-brand"
                  : "text-text-tertiary",
          )}
        >
          {statusText}
        </span>
        {server.phase !== "disabled" && (
          <button
            type="button"
            disabled={busy}
            onClick={() => onReconnect(server.name)}
            className="flex size-6 shrink-0 items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-accent hover:text-foreground disabled:opacity-40"
            title={t("pop.mcp_reconnect_one")}
            aria-label={t("pop.mcp_reconnect_one")}
          >
            <RefreshCwIcon className={cn("size-3", busy && "animate-spin")} />
          </button>
        )}
      </div>
      {server.target && (
        <div
          className="truncate pl-3.5 font-mono text-2xs text-text-tertiary"
          title={server.target}
        >
          {server.target}
        </div>
      )}
      {server.error && (
        <div
          className={cn(
            "break-words pl-3.5 text-xs",
            server.phase === "failed" ? "text-destructive" : "text-warn",
          )}
        >
          {server.error}
        </div>
      )}
      {stderr.length > 0 && (
        <pre className="ml-3.5 max-h-24 overflow-x-auto overflow-y-auto whitespace-pre-wrap break-all rounded-md border border-border bg-muted/40 px-2 py-1.5 font-mono text-2xs text-muted-foreground">
          {stderr.join("\n")}
        </pre>
      )}
    </div>
  );
}

/**
 * MCP 状态 chip 与诊断 Popover：
 * - 在欢迎页（开始会话前）与会话状态条共用；
 * - 无配置且无错误时隐藏，健康时低饱和度展示计数，慢连接（≥2s）/重连/失败/配置错误时主动显色提醒。
 */
export function McpPopover({
  sessionId,
  cwd: explicitCwd,
  side = "top",
  align = "end",
}: {
  sessionId: string;
  cwd?: string;
  side?: "top" | "bottom";
  align?: "start" | "end";
}) {
  const t = useT();
  const features = useConnection(selectFeatures);
  const defaultCwd = useConnection((state) => state.config?.cwd ?? "");
  const viewCwd = useTranscript((state) => state.bySession[sessionId]?.info.cwd ?? "");
  const cwd = explicitCwd || viewCwd || defaultCwd;
  const snapshot = useMcp((state) => (cwd ? state.byCwd[cwd] : undefined));
  const reconnecting = useMcp((state) => state.reconnecting);
  const controlled = useUi((state) => state.statusPopover);
  const open = controlled?.sessionId === sessionId && controlled.kind === "mcp";
  const setOpen = (next: boolean) => popoverFor(sessionId, "mcp").set(next);

  const servers = snapshot?.servers ?? [];
  const configError = snapshot?.config_error ?? null;
  const hasActiveHandshake = servers.some(
    (server) => server.phase === "connecting" || server.phase === "reconnecting",
  );
  const [nowMs, setNowMs] = useState(() => Date.now());

  useEffect(() => {
    if (!cwd || features?.mcp === false) return;
    if (useMcp.getState().byCwd[cwd]) return;
    void useMcp
      .getState()
      .refresh(sessionId !== "draft-session" ? { sessionId, cwd } : { cwd })
      .catch(() => undefined);
  }, [cwd, features?.mcp, sessionId]);

  useEffect(() => {
    if (!hasActiveHandshake) return;
    setNowMs(Date.now());
    const timer = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [hasActiveHandshake]);

  if (features?.mcp === false) return null;
  if (!open && !configError && servers.length === 0) return null;

  const enabledServers = servers.filter((server) => server.phase !== "disabled");
  const connectedCount = enabledServers.filter((server) => server.phase === "connected").length;
  const failedCount = enabledServers.filter((server) => server.phase === "failed").length;
  const reconnectingCount = enabledServers.filter(
    (server) => server.phase === "reconnecting",
  ).length;
  const connectingServers = enabledServers.filter((server) => server.phase === "connecting");
  const maxConnectingSecs = connectingServers.reduce((max, server) => {
    const secs = elapsedSecs(server.started_at_ms, nowMs) ?? 0;
    return Math.max(max, secs);
  }, 0);
  const hasSlowConnecting = connectingServers.some(
    (server) =>
      server.started_at_ms != null && Math.max(0, nowMs - server.started_at_ms) >= MCP_SLOW_MS,
  );

  const triggerTone =
    configError || failedCount > 0
      ? "text-destructive"
      : reconnectingCount > 0 || (hasSlowConnecting && maxConnectingSecs >= MCP_WARN_SECS)
        ? "text-warn"
        : connectingServers.length > 0
          ? "text-brand"
          : "text-muted-foreground";

  const triggerLabel = configError
    ? t("pop.mcp_config_err")
    : failedCount > 0
      ? t("pop.mcp_issue", { n: failedCount + reconnectingCount })
      : reconnectingCount > 0
        ? t("pop.mcp_issue", { n: reconnectingCount })
        : hasSlowConnecting
          ? t("pop.mcp_connecting_s", { s: maxConnectingSecs })
          : connectingServers.length > 0
            ? t("pop.mcp_connecting")
            : `${connectedCount}/${enabledServers.length}`;

  const handleReconnect = (server?: string) => {
    void useMcp
      .getState()
      .reconnect({ sessionId, cwd, server })
      .then(() => {
        toast.success(
          server ? t("slash.mcp_reconnecting_one", { name: server }) : t("slash.mcp_reconnecting"),
        );
      })
      .catch((error) => {
        toast.error(
          t("slash.mcp_reconnect_failed", {
            error: error instanceof Error ? error.message : String(error),
          }),
        );
      });
  };

  const allBusy = mcpReconnectPending(reconnecting, cwd);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className={cn(
            "inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md px-1.5 text-ui transition-colors hover:bg-accent hover:text-foreground data-[state=open]:bg-accent",
            triggerTone,
          )}
          title={t("pop.mcp")}
        >
          {configError || failedCount > 0 ? (
            <AlertCircleIcon className="size-3 shrink-0" />
          ) : connectingServers.length > 0 || reconnectingCount > 0 ? (
            <Loader2Icon className="size-3 shrink-0 animate-spin" />
          ) : (
            <span className="size-1.5 shrink-0 rounded-full bg-success" />
          )}
          <span className="font-mono text-xs tabular-nums">MCP {triggerLabel}</span>
        </button>
      </PopoverTrigger>
      <PopoverContent side={side} align={align} sideOffset={8} className="w-96 p-0">
        <div className="flex items-center gap-2 border-b border-border px-3 py-2">
          <span className="text-ui font-medium text-foreground">MCP</span>
          {enabledServers.length > 0 && (
            <span className="font-mono text-2xs tabular-nums text-text-tertiary">
              {connectedCount}/{enabledServers.length}
            </span>
          )}
          <div className="ml-auto flex items-center gap-1">
            {enabledServers.length > 0 && (
              <button
                type="button"
                disabled={allBusy}
                onClick={() => handleReconnect()}
                className="inline-flex h-6 items-center gap-1 rounded-md px-2 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:opacity-40"
              >
                <RefreshCwIcon className={cn("size-3", allBusy && "animate-spin")} />
                <span>{t("pop.mcp_reconnect_all")}</span>
              </button>
            )}
            <button
              type="button"
              onClick={() => {
                setOpen(false);
                openSettings("mcp");
              }}
              className="flex size-6 items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-accent hover:text-foreground"
              title={t("pop.mcp_settings")}
              aria-label={t("pop.mcp_settings")}
            >
              <Settings2Icon className="size-3.5" />
            </button>
          </div>
        </div>
        <div className="max-h-[70vh] overflow-y-auto">
          {configError && (
            <div className="border-b border-border bg-destructive/10 px-3 py-2 text-xs text-destructive">
              {t("pop.mcp_config_err_detail", { error: configError })}
            </div>
          )}
          {servers.length === 0 ? (
            <div className="px-3 py-5 text-center text-xs text-text-tertiary">
              {t("pop.mcp_empty")}
            </div>
          ) : (
            <div className="divide-y divide-border">
              {servers.map((server) => (
                <McpServerRow
                  key={server.name}
                  server={server}
                  nowMs={nowMs}
                  busy={mcpReconnectPending(reconnecting, cwd, server.name)}
                  onReconnect={(name) => handleReconnect(name)}
                />
              ))}
            </div>
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
