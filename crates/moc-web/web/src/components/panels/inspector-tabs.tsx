// 检查面板里按需拉取数据的分页：子代理、上下文、用量、旁路问答。

import {
  ArrowUpIcon,
  Loader2Icon,
  PlusIcon,
  RefreshCwIcon,
  SquareIcon,
  Trash2Icon,
} from "lucide-react";
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { MessageResponse } from "@/components/ai-elements/message";
import { Shimmer } from "@/components/ai-elements/shimmer";
import { IconButton } from "@/components/common/icon-button";
import { PlainLines } from "@/components/common/plain-lines";
import { Button } from "@/components/ui/button";
import { formatDuration, relativeTime } from "@/lib/format";
import { type MsgKey, useT } from "@/lib/i18n";
import { modelName } from "@/lib/models";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  AgentInfo,
  AgentListResult,
  AgentStatus,
  ModelUsageSummary,
  SessionUsageResult,
} from "@/protocol/types";
import { useBtw } from "@/store/btw";
import { openSession } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 进入分页即拉取；`nonce` 变化时重拉。卸载或 key 变化后迟到的结果丢弃。 */
function useFetch<T>(key: string, load: () => Promise<T>) {
  const [state, setState] = useState<{ data: T | null; error: string | null; loading: boolean }>({
    data: null,
    error: null,
    loading: true,
  });
  const [nonce, setNonce] = useState(0);
  const loadRef = useRef(load);
  loadRef.current = load;

  // biome-ignore lint/correctness/useExhaustiveDependencies: key 与 nonce 都是重拉信号，load 走 ref 取最新
  useEffect(() => {
    let alive = true;
    setState((current) => ({ ...current, loading: true, error: null }));
    loadRef
      .current()
      .then((data) => alive && setState({ data, error: null, loading: false }))
      .catch(
        (error) =>
          alive && setState((current) => ({ ...current, error: errorText(error), loading: false })),
      );
    return () => {
      alive = false;
    };
  }, [key, nonce]);

  return { ...state, reload: () => setNonce((value) => value + 1) };
}

export function PanelEmpty({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="flex flex-col items-center gap-1 px-6 py-16 text-center">
      <p className="text-ui text-muted-foreground">{title}</p>
      {hint && <p className="max-w-64 text-xs leading-5 text-balance text-text-tertiary">{hint}</p>}
    </div>
  );
}

function PanelError({ message, onRetry }: { message: string; onRetry: () => void }) {
  const t = useT();
  return (
    <div className="m-3 space-y-2 rounded-lg border border-destructive/25 bg-destructive/5 p-3 text-xs text-destructive">
      <p className="break-words whitespace-pre-wrap">{message}</p>
      <Button size="xs" variant="outline" onClick={onRetry} className="h-6">
        <RefreshCwIcon />
        {t("common.retry")}
      </Button>
    </div>
  );
}

function SectionBar({ title, children }: { title: ReactNode; children?: ReactNode }) {
  return (
    <div className="flex h-9 items-center justify-between gap-2 px-4">
      <span className="text-xs font-medium text-muted-foreground">{title}</span>
      <div className="flex items-center gap-0.5">{children}</div>
    </div>
  );
}

function RefreshButton({ loading, onClick }: { loading: boolean; onClick: () => void }) {
  const t = useT();
  return (
    <IconButton label={t("it.refresh")} size="xs" onClick={onClick} disabled={loading}>
      {loading ? <Loader2Icon className="animate-spin" /> : <RefreshCwIcon />}
    </IconButton>
  );
}

// ---------- 子代理 ----------

const AGENT_STATUS: Record<AgentStatus, { label: MsgKey; dot: string }> = {
  queued: { label: "ag.queued", dot: "bg-warn" },
  running: { label: "ag.running", dot: "bg-brand animate-pulse" },
  idle: { label: "ag.idle", dot: "bg-text-tertiary" },
  completed: { label: "ag.completed", dot: "bg-success" },
  stopped: { label: "ag.stopped", dot: "bg-text-tertiary" },
  failed: { label: "ag.failed", dot: "bg-destructive" },
};

export function AgentsTab({ sessionId }: { sessionId: string }) {
  const t = useT();
  const live = useTranscript((state) => state.bySession[sessionId]?.agents);
  const readOnly = useTranscript((state) => Boolean(state.bySession[sessionId]?.readOnly));
  const isDraft = sessionId === "draft-session";
  const { data, error, loading, reload } = useFetch(sessionId, () =>
    isDraft
      ? Promise.resolve({ agents: [], roles: [] })
      : rpc.call<AgentListResult>("agent/list", { session_id: sessionId }),
  );
  const [task, setTask] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [roleName, setRoleName] = useState("");
  const roles = data?.roles ?? [];
  const selectedRole = roles.find((role) => role.name === roleName) ?? roles[0];

  // 列表结果打底，直播事件里的状态更新覆盖其上。
  const agents = useMemo(() => {
    const byId = new Map<string, AgentInfo>();
    for (const agent of data?.agents ?? []) byId.set(agent.id, agent);
    for (const agent of Object.values(live ?? {})) byId.set(agent.id, agent);
    return [...byId.values()].toSorted((left, right) => right.updated_at_ms - left.updated_at_ms);
  }, [data, live]);

  const spawn = async () => {
    const text = task?.trim();
    if (!text || !selectedRole || readOnly || busy) return;
    setBusy("spawn");
    try {
      await rpc.call("agent/spawn", { session_id: sessionId, role: selectedRole.name, task: text });
      setTask(null);
      reload();
    } catch (spawnError) {
      toast.error(t("it.spawn_failed", { error: errorText(spawnError) }));
    } finally {
      setBusy(null);
    }
  };

  const stop = async (agentId: string) => {
    setBusy(agentId);
    try {
      await rpc.call("agent/stop", { session_id: sessionId, agent_id: agentId });
      reload();
    } catch (stopError) {
      toast.error(t("cb.stop_failed", { error: errorText(stopError) }));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="flex flex-col">
      <SectionBar title={t("it.agents_n", { n: agents.length })}>
        {!isDraft && !readOnly && (
          <IconButton
            label={t("it.spawn")}
            size="xs"
            disabled={loading || roles.length === 0}
            onClick={() => setTask((value) => (value === null ? "" : null))}
          >
            <PlusIcon />
          </IconButton>
        )}
        {!isDraft && <RefreshButton loading={loading} onClick={reload} />}
      </SectionBar>

      {task !== null && (
        <div className="mx-3 mb-3 rounded-xl bg-muted p-2 ring-1 ring-inset ring-border-subtle">
          <label className="flex items-center gap-2 px-1.5 pb-2 text-xs text-muted-foreground">
            {t("it.role")}
            <select
              aria-label={t("it.role_aria")}
              value={selectedRole?.name ?? ""}
              onChange={(event) => setRoleName(event.target.value)}
              disabled={roles.length === 0 || busy === "spawn"}
              className="min-w-0 flex-1 rounded bg-background px-2 py-1 text-foreground"
            >
              {roles.map((role) => (
                <option key={role.name} value={role.name}>
                  {role.name}
                </option>
              ))}
            </select>
          </label>
          <p className="px-1.5 pb-2 text-xs text-muted-foreground">
            {selectedRole?.description ?? t("it.no_roles")}
          </p>
          <textarea
            // biome-ignore lint/a11y/noAutofocus: 点开即输入
            autoFocus
            value={task}
            onChange={(event) => setTask(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) void spawn();
              if (event.key === "Escape") setTask(null);
            }}
            rows={3}
            placeholder={t("it.task_ph")}
            className="w-full resize-none bg-transparent px-1.5 py-1 text-ui outline-none placeholder:text-text-tertiary"
          />
          <div className="flex justify-end gap-1.5">
            <Button size="xs" variant="ghost" onClick={() => setTask(null)}>
              {t("common.cancel")}
            </Button>
            <Button
              size="xs"
              disabled={!task.trim() || !selectedRole || busy === "spawn"}
              onClick={() => void spawn()}
            >
              {busy === "spawn" && <Loader2Icon className="animate-spin" />}
              {t("it.start")}
            </Button>
          </div>
        </div>
      )}

      {error && <PanelError message={error} onRetry={reload} />}
      {agents.length === 0 && !loading && !error ? (
        <PanelEmpty
          title={t("it.no_agents")}
          hint={roles.length === 0 ? t("it.no_roles_hint") : t("it.no_agents_hint")}
        />
      ) : (
        <ul className="space-y-px px-2 pb-3">
          {agents.map((agent) => {
            const status = AGENT_STATUS[agent.status];
            const active = agent.status === "running" || agent.status === "queued";
            return (
              <li key={agent.id} className="group/agent relative">
                <button
                  type="button"
                  onClick={() =>
                    void openSession(agent.id, { readOnly: true }).catch(() => undefined)
                  }
                  className="flex w-full flex-col gap-1 rounded-lg px-2.5 py-2 text-left hover:bg-surface-hover"
                >
                  <span className="flex w-full min-w-0 items-center gap-2 pr-7">
                    <span className={cn("size-1.5 shrink-0 rounded-full", status.dot)} />
                    <span className="min-w-0 flex-1 truncate text-ui text-foreground">
                      {agent.title || agent.id}
                    </span>
                    <span className="shrink-0 text-2xs text-text-tertiary">{t(status.label)}</span>
                  </span>
                  {agent.last_message && (
                    <span className="line-clamp-2 pl-3.5 text-xs leading-5 text-muted-foreground">
                      {agent.last_message}
                    </span>
                  )}
                  <span className="pl-3.5 text-2xs text-text-tertiary">
                    {modelName(agent.model)} · {relativeTime(agent.updated_at_ms)}
                  </span>
                </button>
                {active && !readOnly && (
                  <IconButton
                    label={t("keys.stop")}
                    size="xs"
                    disabled={busy === agent.id}
                    onClick={() => void stop(agent.id)}
                    className="absolute top-1.5 right-1.5"
                  >
                    {busy === agent.id ? (
                      <Loader2Icon className="animate-spin" />
                    ) : (
                      <SquareIcon className="size-3 fill-current" />
                    )}
                  </IconButton>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

// ContextViewer 已迁到 components/context/context-viewer.tsx（ContextDialog 承载）。

// ---------- 用量 ----------

function Metric({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className={cn("rounded-lg bg-muted/60 px-3 py-2", className)}>
      <div className="text-2xs text-text-tertiary">{label}</div>
      <div className="mt-0.5 font-mono text-ui tabular-nums text-foreground">{value}</div>
    </div>
  );
}

/** 输出速率（与 TUI `usage_tps` 同口径）：有 gen_ms 优先，否则无 ttft 采样时退回 active_ms。 */
function outputTps(summary: ModelUsageSummary): string {
  const output = summary.output_tokens;
  if (!output) return "—";
  const genMs = summary.gen_ms ?? 0;
  const ms = genMs > 0 ? genMs : (summary.ttft_calls ?? 0) === 0 ? summary.active_ms : 0;
  if (ms <= 0) return "—";
  return `${((output * 1000) / ms).toFixed(1)} tok/s`;
}

function UsageCard({ summary, title }: { summary: ModelUsageSummary; title: string }) {
  const t = useT();
  const tokens = (value: number | undefined) => (value ? value.toLocaleString() : "0");
  return (
    <section className="space-y-2">
      <div className="flex items-baseline justify-between gap-2">
        <span className="min-w-0 truncate text-ui font-medium">{title}</span>
        <span className="shrink-0 text-2xs text-text-tertiary">
          {t("it.calls", { n: summary.calls })}
        </span>
      </div>
      <div className="grid grid-cols-2 gap-1.5">
        <Metric label={t("mp.input")} value={tokens(summary.input_tokens)} />
        <Metric label={t("mp.output")} value={tokens(summary.output_tokens)} />
        <Metric
          label={t("mp.cache_read")}
          value={tokens(summary.cached_input_tokens ?? undefined)}
        />
        <Metric
          label={t("mp.cache_write")}
          value={tokens(summary.cache_creation_input_tokens ?? undefined)}
        />
        <Metric label={t("it.reasoning")} value={tokens(summary.reasoning_tokens ?? undefined)} />
        <Metric
          label={t("it.ttft")}
          value={
            summary.ttft_calls ? formatDuration((summary.ttft_ms ?? 0) / summary.ttft_calls) : "—"
          }
        />
        <Metric label={t("it.gen_ms")} value={formatDuration(summary.gen_ms ?? 0)} />
        <Metric label={t("it.active_ms")} value={formatDuration(summary.active_ms)} />
        <Metric label={t("it.tps")} value={outputTps(summary)} className="col-span-2" />
      </div>
    </section>
  );
}

export function UsageTab({ sessionId }: { sessionId: string }) {
  const t = useT();
  const isDraft = sessionId === "draft-session";
  const running = useTranscript((state) => state.bySession[sessionId]?.running);
  const { data, error, loading, reload } = useFetch(`${sessionId}:${String(running)}`, () =>
    isDraft
      ? Promise.resolve(null)
      : rpc.call<SessionUsageResult>("session/usage", { session_id: sessionId }),
  );

  if (isDraft) return <PanelEmpty title={t("it.usage_draft")} />;
  if (error) return <PanelError message={error} onRetry={reload} />;
  if (!data) return <PanelEmpty title={t("it.usage_loading")} />;

  return (
    <div className="flex flex-col">
      <SectionBar title={t("it.usage_title")}>
        <span className="mr-1 font-mono text-2xs text-text-tertiary">
          {t("it.tool_ms", { n: formatDuration(data.tool_ms ?? 0) })}
        </span>
        <RefreshButton loading={loading} onClick={reload} />
      </SectionBar>
      <div className="space-y-5 px-4 pb-4">
        <UsageCard summary={data.total} title={t("it.total")} />
        {data.models.length > 1 &&
          data.models.map((summary) => (
            <UsageCard
              key={`${summary.source ?? "session"}:${summary.model}`}
              summary={summary}
              title={
                modelName(summary.model) +
                (summary.source === "sidekick"
                  ? t("it.src_sidekick")
                  : summary.source === "subagent"
                    ? t("it.src_subagent")
                    : "")
              }
            />
          ))}
      </div>
    </div>
  );
}

// ---------- 旁路问答 ----------

export function BtwTab({ sessionId }: { sessionId: string }) {
  const t = useT();
  const thread = useBtw((state) => state.threads[sessionId]);
  const focus = useUi((state) => state.focusBtw);
  const [question, setQuestion] = useState("");
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const endRef = useRef<HTMLDivElement>(null);
  const isDraft = sessionId === "draft-session";
  const turns = thread?.turns ?? [];
  const inflight = thread?.inflight != null;
  const lastAnswer = turns.at(-1)?.answer;

  useEffect(() => {
    if (focus > 0) inputRef.current?.focus();
  }, [focus]);

  useEffect(() => {
    void turns.length;
    void lastAnswer;
    endRef.current?.scrollIntoView({ block: "end" });
  }, [turns.length, lastAnswer]);

  if (isDraft) return <PanelEmpty title={t("it.btw_draft")} />;

  const ask = () => {
    const text = question.trim();
    if (!text) return;
    if (useBtw.getState().ask(sessionId, text)) setQuestion("");
    else toast(t("btw.busy"));
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SectionBar title={t("it.btw_title")}>
        {turns.length > 0 && (
          <IconButton
            label={t("common.clear")}
            size="xs"
            onClick={() => useBtw.getState().clear(sessionId)}
          >
            <Trash2Icon />
          </IconButton>
        )}
      </SectionBar>
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-4 pb-4">
        {turns.length === 0 && (
          <PanelEmpty title={t("it.btw_empty")} hint={t("it.btw_empty_hint")} />
        )}
        {turns.map((turn) => (
          <div key={turn.requestId} className="space-y-2">
            <div className="ml-auto w-fit max-w-[90%] rounded-2xl rounded-br-md bg-bubble px-3 py-1.5 text-ui">
              <PlainLines text={turn.question} />
            </div>
            {turn.answer ? (
              <MessageResponse
                sessionId={sessionId}
                className="[--moc-prose-size:13.5px]"
                isAnimating={turn.status === "generating"}
              >
                {turn.answer}
              </MessageResponse>
            ) : turn.status === "waiting" || turn.status === "generating" ? (
              <Shimmer as="span" className="text-ui">
                {`${t("verb.thinking_ing")}…`}
              </Shimmer>
            ) : null}
            {turn.status === "failed" && (
              <p className="text-xs text-destructive">{turn.error ?? t("it.answer_failed")}</p>
            )}
            {turn.status === "cancelled" && (
              <p className="text-xs text-text-tertiary">{t("act.cancelled")}</p>
            )}
          </div>
        ))}
        <div ref={endRef} />
      </div>
      <div className="shrink-0 p-3 pt-0">
        <div className="flex items-end gap-1.5 rounded-xl bg-muted p-1.5 ring-1 ring-inset ring-border-subtle focus-within:ring-border-strong">
          <textarea
            ref={inputRef}
            value={question}
            rows={1}
            onChange={(event) => setQuestion(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                ask();
              }
            }}
            placeholder={t("it.btw_ph")}
            className="max-h-32 min-h-7 flex-1 resize-none bg-transparent px-1.5 py-1 text-ui leading-5 outline-none placeholder:text-text-tertiary"
          />
          {inflight ? (
            <IconButton
              label={t("keys.stop")}
              size="sm"
              onClick={() => useBtw.getState().cancel(sessionId)}
            >
              <SquareIcon className="size-3 fill-current" />
            </IconButton>
          ) : (
            <button
              type="button"
              onClick={ask}
              disabled={!question.trim()}
              className="flex size-7 shrink-0 items-center justify-center rounded-full bg-foreground text-background disabled:opacity-25"
              aria-label={t("it.ask")}
            >
              <ArrowUpIcon className="size-3.5" strokeWidth={2.4} />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
