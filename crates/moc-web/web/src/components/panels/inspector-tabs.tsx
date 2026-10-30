// 检查面板里按需拉取数据的分页：子代理、上下文、用量、旁路问答。

import {
  ArrowUpIcon,
  ChevronRightIcon,
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
import { Button } from "@/components/ui/button";
import { formatDuration, relativeTime } from "@/lib/format";
import { compactTokens, modelName } from "@/lib/models";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  AgentInfo,
  AgentListResult,
  AgentStatus,
  ContextContentResult,
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
      <p className="text-[13px] text-muted-foreground">{title}</p>
      {hint && <p className="max-w-64 text-xs leading-5 text-text-tertiary">{hint}</p>}
    </div>
  );
}

function PanelError({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="m-3 space-y-2 rounded-lg border border-destructive/25 bg-destructive/5 p-3 text-xs text-destructive">
      <p className="break-words whitespace-pre-wrap">{message}</p>
      <Button size="xs" variant="outline" onClick={onRetry} className="h-6">
        <RefreshCwIcon />
        重试
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
  return (
    <IconButton label="刷新" size="xs" onClick={onClick} disabled={loading}>
      {loading ? <Loader2Icon className="animate-spin" /> : <RefreshCwIcon />}
    </IconButton>
  );
}

// ---------- 子代理 ----------

const AGENT_STATUS: Record<AgentStatus, { label: string; dot: string }> = {
  queued: { label: "排队中", dot: "bg-warn" },
  running: { label: "运行中", dot: "bg-brand animate-pulse" },
  idle: { label: "空闲", dot: "bg-text-tertiary" },
  completed: { label: "已完成", dot: "bg-success" },
  stopped: { label: "已停止", dot: "bg-text-tertiary" },
  failed: { label: "失败", dot: "bg-destructive" },
};

export function AgentsTab({ sessionId }: { sessionId: string }) {
  const live = useTranscript((state) => state.bySession[sessionId]?.agents);
  const readOnly = useTranscript((state) => Boolean(state.bySession[sessionId]?.readOnly));
  const isDraft = sessionId === "draft-session";
  const { data, error, loading, reload } = useFetch(sessionId, () =>
    isDraft
      ? Promise.resolve({ agents: [] })
      : rpc.call<AgentListResult>("agent/list", { session_id: sessionId }),
  );
  const [task, setTask] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  // 列表结果打底，直播事件里的状态更新覆盖其上。
  const agents = useMemo(() => {
    const byId = new Map<string, AgentInfo>();
    for (const agent of data?.agents ?? []) byId.set(agent.id, agent);
    for (const agent of Object.values(live ?? {})) byId.set(agent.id, agent);
    return [...byId.values()].toSorted((left, right) => right.updated_at_ms - left.updated_at_ms);
  }, [data, live]);

  const spawn = async () => {
    const text = task?.trim();
    if (!text) return;
    setBusy("spawn");
    try {
      await rpc.call("agent/spawn", { session_id: sessionId, task: text });
      setTask(null);
      reload();
    } catch (spawnError) {
      toast.error(`启动子代理失败：${errorText(spawnError)}`);
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
      toast.error(`停止失败：${errorText(stopError)}`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="flex flex-col">
      <SectionBar title={`子代理 · ${agents.length}`}>
        {!isDraft && !readOnly && (
          <IconButton
            label="手动启动子代理"
            size="xs"
            onClick={() => setTask((value) => (value === null ? "" : null))}
          >
            <PlusIcon />
          </IconButton>
        )}
        {!isDraft && <RefreshButton loading={loading} onClick={reload} />}
      </SectionBar>

      {task !== null && (
        <div className="mx-3 mb-3 rounded-xl border border-border bg-background p-2">
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
            placeholder="交给子代理的任务，Ctrl+Enter 启动"
            className="w-full resize-none bg-transparent px-1.5 py-1 text-[13px] outline-none placeholder:text-text-tertiary"
          />
          <div className="flex justify-end gap-1.5">
            <Button size="xs" variant="ghost" onClick={() => setTask(null)}>
              取消
            </Button>
            <Button
              size="xs"
              disabled={!task.trim() || busy === "spawn"}
              onClick={() => void spawn()}
            >
              {busy === "spawn" && <Loader2Icon className="animate-spin" />}
              启动
            </Button>
          </div>
        </div>
      )}

      {error && <PanelError message={error} onRetry={reload} />}
      {agents.length === 0 && !loading && !error ? (
        <PanelEmpty
          title="还没有子代理"
          hint="模型调用 agent 工具派出的子任务会出现在这里，可以点开只读查看过程。"
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
                  onClick={() => void openSession(agent.id, { readOnly: true })}
                  className="flex w-full flex-col gap-1 rounded-lg px-2.5 py-2 text-left hover:bg-surface-hover"
                >
                  <span className="flex w-full min-w-0 items-center gap-2 pr-7">
                    <span className={cn("size-1.5 shrink-0 rounded-full", status.dot)} />
                    <span className="min-w-0 flex-1 truncate text-[13px] text-foreground">
                      {agent.title || agent.id}
                    </span>
                    <span className="shrink-0 text-[11px] text-text-tertiary">{status.label}</span>
                  </span>
                  {agent.last_message && (
                    <span className="line-clamp-2 pl-3.5 text-xs leading-5 text-muted-foreground">
                      {agent.last_message}
                    </span>
                  )}
                  <span className="pl-3.5 text-[11px] text-text-tertiary">
                    {modelName(agent.model)} · {relativeTime(agent.updated_at_ms)}
                  </span>
                </button>
                {active && !readOnly && (
                  <IconButton
                    label="停止"
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

// ---------- 上下文 ----------

const SECTION_COLORS = [
  "bg-chart-1",
  "bg-chart-2",
  "bg-chart-3",
  "bg-chart-4",
  "bg-chart-5",
  "bg-chart-6",
];

export function ContextTab({ sessionId }: { sessionId: string }) {
  const isDraft = sessionId === "draft-session";
  const seq = useTranscript((state) => state.bySession[sessionId]?.running);
  const { data, error, loading, reload } = useFetch(`${sessionId}:${String(seq)}`, () =>
    isDraft
      ? Promise.resolve(null)
      : rpc.call<ContextContentResult>("session/context_content", {
          session_id: sessionId,
          max_section_bytes: 64_000,
        }),
  );
  const [open, setOpen] = useState<string | null>(null);

  if (isDraft) return <PanelEmpty title="发送第一条消息后可查看上下文" />;
  if (error) return <PanelError message={error} onRetry={reload} />;
  if (!data) return <PanelEmpty title="正在读取上下文…" />;

  const window = data.context_window || 1;
  const percent = Math.min(100, (data.total_tokens / window) * 100);

  return (
    <div className="flex flex-col">
      <SectionBar title="模型上下文">
        <RefreshButton loading={loading} onClick={reload} />
      </SectionBar>
      <div className="space-y-2.5 px-4 pb-4">
        <div className="flex items-baseline justify-between">
          <span className="text-[22px] font-semibold tabular-nums tracking-tight">
            {percent.toFixed(1)}%
          </span>
          <span className="font-mono text-xs text-text-tertiary">
            {compactTokens(data.total_tokens) || "0"} / {compactTokens(data.context_window)}
          </span>
        </div>
        <div className="flex h-2 overflow-hidden rounded-full bg-muted">
          {data.sections.map((section, index) => (
            <div
              key={section.name}
              className={SECTION_COLORS[index % SECTION_COLORS.length]}
              style={{ width: `${(section.tokens / window) * 100}%` }}
              title={`${section.name} · ${section.tokens.toLocaleString()} tokens`}
            />
          ))}
        </div>
      </div>
      <ul className="border-t border-border-subtle">
        {data.sections.map((section, index) => {
          const expanded = open === section.name;
          return (
            <li key={section.name} className="border-b border-border-subtle">
              <button
                type="button"
                onClick={() => setOpen(expanded ? null : section.name)}
                className="flex h-10 w-full items-center gap-2.5 px-4 text-left text-[13px] hover:bg-surface-hover"
              >
                <span
                  className={cn(
                    "size-2 shrink-0 rounded-sm",
                    SECTION_COLORS[index % SECTION_COLORS.length],
                  )}
                />
                <span className="min-w-0 flex-1 truncate text-foreground">{section.name}</span>
                <span className="shrink-0 font-mono text-xs tabular-nums text-text-tertiary">
                  {section.tokens.toLocaleString()}
                </span>
                <ChevronRightIcon
                  className={cn(
                    "size-3.5 shrink-0 text-text-tertiary transition-transform",
                    expanded && "rotate-90",
                  )}
                />
              </button>
              {expanded && (
                <div className="px-4 pb-3">
                  <pre className="max-h-96 overflow-auto rounded-lg bg-code p-3 font-mono text-[11.5px] leading-[18px] break-words whitespace-pre-wrap text-foreground/90">
                    {section.text || "（空）"}
                  </pre>
                  {section.truncated_bytes > 0 && (
                    <p className="mt-1.5 text-[11px] text-text-tertiary">
                      已截断 {section.truncated_bytes.toLocaleString()} 字节
                    </p>
                  )}
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

// ---------- 用量 ----------

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg bg-muted/60 px-3 py-2">
      <div className="text-[11px] text-text-tertiary">{label}</div>
      <div className="mt-0.5 font-mono text-[13px] tabular-nums text-foreground">{value}</div>
    </div>
  );
}

function UsageCard({ summary, title }: { summary: ModelUsageSummary; title: string }) {
  const tokens = (value: number | undefined) => (value ? value.toLocaleString() : "0");
  return (
    <section className="space-y-2">
      <div className="flex items-baseline justify-between gap-2">
        <span className="min-w-0 truncate text-[13px] font-medium">{title}</span>
        <span className="shrink-0 text-[11px] text-text-tertiary">{summary.calls} 次请求</span>
      </div>
      <div className="grid grid-cols-2 gap-1.5">
        <Metric label="输入" value={tokens(summary.input_tokens)} />
        <Metric label="输出" value={tokens(summary.output_tokens)} />
        <Metric label="缓存命中" value={tokens(summary.cached_input_tokens)} />
        <Metric label="缓存写入" value={tokens(summary.cache_creation_input_tokens)} />
        <Metric label="推理" value={tokens(summary.reasoning_tokens)} />
        <Metric
          label="平均首字"
          value={summary.ttft_calls ? formatDuration(summary.ttft_ms / summary.ttft_calls) : "—"}
        />
        <Metric label="生成耗时" value={formatDuration(summary.gen_ms)} />
        <Metric label="活跃耗时" value={formatDuration(summary.active_ms)} />
      </div>
    </section>
  );
}

export function UsageTab({ sessionId }: { sessionId: string }) {
  const isDraft = sessionId === "draft-session";
  const running = useTranscript((state) => state.bySession[sessionId]?.running);
  const { data, error, loading, reload } = useFetch(`${sessionId}:${String(running)}`, () =>
    isDraft
      ? Promise.resolve(null)
      : rpc.call<SessionUsageResult>("session/usage", { session_id: sessionId }),
  );

  if (isDraft) return <PanelEmpty title="发送第一条消息后可查看用量" />;
  if (error) return <PanelError message={error} onRetry={reload} />;
  if (!data) return <PanelEmpty title="正在读取用量…" />;

  return (
    <div className="flex flex-col">
      <SectionBar title="会话用量">
        <span className="mr-1 font-mono text-[11px] text-text-tertiary">
          工具耗时 {formatDuration(data.tool_ms)}
        </span>
        <RefreshButton loading={loading} onClick={reload} />
      </SectionBar>
      <div className="space-y-5 px-4 pb-4">
        <UsageCard summary={data.total} title="合计" />
        {data.models.length > 1 &&
          data.models.map((summary) => (
            <UsageCard
              key={`${summary.source ?? "session"}:${summary.model}`}
              summary={summary}
              title={
                modelName(summary.model) +
                (summary.source === "sidekick"
                  ? " (sidekick)"
                  : summary.source === "subagent"
                    ? " (子代理)"
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

  if (isDraft) return <PanelEmpty title="发送第一条消息后可使用旁路问答" />;

  const ask = () => {
    const text = question.trim();
    if (!text) return;
    if (useBtw.getState().ask(sessionId, text)) setQuestion("");
    else toast("上一个问题还在回答中");
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <SectionBar title="旁路问答 · 不进入主对话">
        {turns.length > 0 && (
          <IconButton label="清空" size="xs" onClick={() => useBtw.getState().clear(sessionId)}>
            <Trash2Icon />
          </IconButton>
        )}
      </SectionBar>
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-4 pb-4">
        {turns.length === 0 && (
          <PanelEmpty
            title="随手问一句"
            hint="基于当前对话上下文回答，不打断主任务，也不会写进会话历史。"
          />
        )}
        {turns.map((turn) => (
          <div key={turn.requestId} className="space-y-2">
            <div className="ml-auto w-fit max-w-[90%] rounded-2xl rounded-br-md bg-bubble px-3 py-1.5 text-[13px] whitespace-pre-wrap">
              {turn.question}
            </div>
            {turn.answer ? (
              <MessageResponse
                className="[--moc-prose-size:13.5px]"
                isAnimating={turn.status === "generating"}
              >
                {turn.answer}
              </MessageResponse>
            ) : turn.status === "waiting" || turn.status === "generating" ? (
              <Shimmer as="span" className="text-[13px]">
                思考中…
              </Shimmer>
            ) : null}
            {turn.status === "failed" && (
              <p className="text-xs text-destructive">{turn.error ?? "回答失败"}</p>
            )}
            {turn.status === "cancelled" && <p className="text-xs text-text-tertiary">已取消</p>}
          </div>
        ))}
        <div ref={endRef} />
      </div>
      <div className="shrink-0 p-3 pt-0">
        <div className="flex items-end gap-1.5 rounded-xl border border-border bg-background p-1.5 focus-within:border-border-strong">
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
            placeholder="问点什么…"
            className="max-h-32 min-h-7 flex-1 resize-none bg-transparent px-1.5 py-1 text-[13px] leading-5 outline-none placeholder:text-text-tertiary"
          />
          {inflight ? (
            <IconButton label="停止" size="sm" onClick={() => useBtw.getState().cancel(sessionId)}>
              <SquareIcon className="size-3 fill-current" />
            </IconButton>
          ) : (
            <button
              type="button"
              onClick={ask}
              disabled={!question.trim()}
              className="flex size-7 shrink-0 items-center justify-center rounded-full bg-foreground text-background disabled:opacity-25"
              aria-label="提问"
            >
              <ArrowUpIcon className="size-3.5" strokeWidth={2.4} />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
