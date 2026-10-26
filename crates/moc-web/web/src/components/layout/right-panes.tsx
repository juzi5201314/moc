// 右侧面板各页：计划 / Todo / 变更 / 子代理 / 上下文 / 旁路。
// 页内容在模块加载时注册进 RIGHT_PANES。

import {
  Ban,
  CheckCircle2,
  Circle,
  CircleDashed,
  CircleSlash,
  ExternalLink,
  Loader2,
  Send,
  Square,
  Trash2,
  XCircle,
} from "lucide-react";
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { compact } from "@/components/composer/commands";
import { DiffView } from "@/components/transcript/diff-view";
import { Markdown } from "@/components/transcript/markdown";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { formatDuration, formatTokens } from "@/lib/format";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  AgentInfo,
  AgentListResult,
  AgentStatus,
  PlanStatus,
  TodoStatus,
  ToolDiff,
} from "@/protocol/types";
import { useBtw } from "@/store/btw";
import { openSession, refreshSideData } from "@/store/lifecycle";
import { type DiffMode, usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { RIGHT_PANES } from "./right-panel";
import { runAction } from "./session-actions";

function Empty({ children }: { children: ReactNode }) {
  return <p className="p-6 text-center text-muted-foreground text-sm">{children}</p>;
}

// ---------- 计划 ----------

const PLAN_STATUS_LABEL: Record<PlanStatus, string> = {
  submitted: "待审阅",
  approved: "已批准",
  rejected: "已拒绝",
};

function PlansPane({ sessionId }: { sessionId: string }) {
  const plans = useTranscript((state) => state.bySession[sessionId]?.plans) ?? [];
  const set = useUi((state) => state.set);
  if (plans.length === 0) return <Empty>暂无计划</Empty>;
  return (
    <div className="space-y-4 p-3">
      {plans.toReversed().map((plan) => (
        <article key={plan.plan_id} className="rounded-xl border bg-background p-3">
          <header className="mb-2 flex items-start gap-2">
            <div className="min-w-0 flex-1">
              <h3 className="truncate font-medium text-sm">{plan.title}</h3>
              <p className="truncate font-mono text-muted-foreground text-xs" title={plan.path}>
                {plan.path}
              </p>
            </div>
            <Badge variant={plan.status === "submitted" ? "default" : "secondary"}>
              {PLAN_STATUS_LABEL[plan.status]}
            </Badge>
          </header>
          {plan.status === "submitted" ? (
            <Button
              size="sm"
              className="mb-2"
              onClick={() => set({ planReview: { sessionId, planId: plan.plan_id } })}
            >
              审阅
            </Button>
          ) : null}
          <Markdown text={plan.markdown} className="text-sm" />
        </article>
      ))}
    </div>
  );
}

// ---------- Todo ----------

const TODO_ICON: Record<TodoStatus, { icon: ReactNode; label: string }> = {
  pending: { icon: <Circle className="size-4 text-muted-foreground" aria-hidden />, label: "待办" },
  in_progress: {
    icon: <CircleDashed className="size-4 animate-spin text-amber-500" aria-hidden />,
    label: "进行中",
  },
  completed: {
    icon: <CheckCircle2 className="size-4 text-emerald-500" aria-hidden />,
    label: "已完成",
  },
  blocked: { icon: <Ban className="size-4 text-rose-500" aria-hidden />, label: "受阻" },
  abandoned: {
    icon: <CircleSlash className="size-4 text-muted-foreground" aria-hidden />,
    label: "已放弃",
  },
};

function TodoPane({ sessionId }: { sessionId: string }) {
  const todos = useTranscript((state) => state.bySession[sessionId]?.todos) ?? [];
  if (todos.length === 0) return <Empty>暂无 Todo</Empty>;
  return (
    <div className="space-y-4 p-3">
      {todos.map((phase) => {
        const done = phase.items.filter((item) => item.status === "completed").length;
        return (
          <section key={phase.phase}>
            <h3 className="mb-1 flex items-center gap-2 font-medium text-sm">
              <span className="truncate">{phase.phase}</span>
              <span className="text-muted-foreground text-xs">
                {done}/{phase.items.length}
              </span>
            </h3>
            <ul className="space-y-1">
              {phase.items.map((item) => {
                const meta = TODO_ICON[item.status];
                return (
                  <li key={item.task} className="flex gap-2 text-sm">
                    <span className="mt-0.5 shrink-0" title={meta.label}>
                      {meta.icon}
                      <span className="sr-only">{meta.label}</span>
                    </span>
                    <div className="min-w-0">
                      <p
                        className={cn(
                          item.status === "completed" && "text-muted-foreground line-through",
                          item.status === "abandoned" && "text-muted-foreground line-through",
                        )}
                      >
                        {item.task}
                      </p>
                      {item.reason ? (
                        <p className="text-muted-foreground text-xs">{item.reason}</p>
                      ) : null}
                    </div>
                  </li>
                );
              })}
            </ul>
          </section>
        );
      })}
    </div>
  );
}

// ---------- 变更 ----------

/** 按文件归并本会话的全部 diff：同一文件的 hunk 按时间顺序拼接。 */
function groupDiffsByFile(diffs: ToolDiff[]): { diff: ToolDiff; files: number } {
  const order: string[] = [];
  const byPath: Record<string, ToolDiff["files"][number]> = {};
  let truncated = false;
  for (const diff of diffs) {
    truncated ||= diff.truncated;
    for (const file of diff.files) {
      const existing = byPath[file.path];
      if (existing) existing.hunks.push(...file.hunks);
      else {
        order.push(file.path);
        byPath[file.path] = { path: file.path, hunks: [...file.hunks] };
      }
    }
  }
  const files = order.flatMap((path) => (byPath[path] ? [byPath[path]] : []));
  return { diff: { files, truncated }, files: files.length };
}

function ChangesPane({ sessionId }: { sessionId: string }) {
  const records = useTranscript((state) => state.bySession[sessionId]?.diffs);
  const diffMode = usePrefs((state) => state.diffMode);
  const setPrefs = usePrefs((state) => state.set);
  const grouped = useMemo(
    () => groupDiffsByFile((records ?? []).map((record) => record.diff)),
    [records],
  );
  if (!records || records.length === 0) return <Empty>本会话暂无文件变更</Empty>;
  const toggle = (mode: DiffMode) => setPrefs({ diffMode: mode });
  return (
    <div className="space-y-2 p-3">
      <div className="flex items-center gap-2">
        <span className="flex-1 text-muted-foreground text-xs">
          {grouped.files} 个文件 · {records.length} 次修改
        </span>
        <fieldset className="flex rounded-md border p-0.5" aria-label="diff 显示方式">
          {(["unified", "split"] as const).map((mode) => (
            <button
              key={mode}
              type="button"
              aria-pressed={diffMode === mode}
              onClick={() => toggle(mode)}
              className={cn(
                "rounded px-2 py-0.5 text-xs",
                diffMode === mode ? "bg-accent text-accent-foreground" : "text-muted-foreground",
              )}
            >
              {mode === "unified" ? "统一" : "分栏"}
            </button>
          ))}
        </fieldset>
      </div>
      <DiffView diff={grouped.diff} mode={diffMode} />
    </div>
  );
}

// ---------- 子代理 ----------

const AGENT_STATUS_LABEL: Record<AgentStatus, string> = {
  queued: "排队中",
  running: "运行中",
  completed: "已完成",
  stopped: "已停止",
  failed: "失败",
  idle: "空闲",
};

function agentIcon(status: AgentStatus): ReactNode {
  switch (status) {
    case "running":
      return <Loader2 className="size-4 animate-spin text-amber-500" aria-hidden />;
    case "completed":
      return <CheckCircle2 className="size-4 text-emerald-500" aria-hidden />;
    case "failed":
      return <XCircle className="size-4 text-rose-500" aria-hidden />;
    case "stopped":
      return <Square className="size-4 text-muted-foreground" aria-hidden />;
    default:
      return <Circle className="size-4 text-muted-foreground" aria-hidden />;
  }
}

function AgentsPane({ sessionId }: { sessionId: string }) {
  const agents = useTranscript((state) => state.bySession[sessionId]?.agents);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    rpc
      .call<AgentListResult>("agent/list", { session_id: sessionId })
      .then((result) => {
        useTranscript.getState().mutate(sessionId, (view) => {
          for (const agent of result.agents) view.agents[agent.id] = agent;
        });
      })
      .catch(() => undefined);
  }, [sessionId]);

  const list = useMemo(
    () => Object.values(agents ?? {}).sort((a, b) => b.created_at_ms - a.created_at_ms),
    [agents],
  );
  const anyRunning = list.some((agent) => agent.status === "running");

  useEffect(() => {
    if (!anyRunning) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [anyRunning]);

  if (list.length === 0) return <Empty>暂无子代理</Empty>;

  const stop = (agent: AgentInfo) =>
    runAction(
      () => rpc.call("agent/stop", { session_id: sessionId, agent_id: agent.id }),
      `已请求停止 ${agent.title}`,
    );

  return (
    <ul className="space-y-2 p-3">
      {list.map((agent) => {
        const active = agent.status === "running" || agent.status === "queued";
        const elapsed = (active ? now : agent.updated_at_ms) - agent.created_at_ms;
        return (
          <li key={agent.id} className="rounded-xl border bg-background p-3">
            <div className="flex items-center gap-2">
              {agentIcon(agent.status)}
              <span className="min-w-0 flex-1 truncate font-medium text-sm">{agent.title}</span>
              <span className="text-muted-foreground text-xs">
                {AGENT_STATUS_LABEL[agent.status]} · {formatDuration(Math.max(0, elapsed))}
              </span>
            </div>
            <p className="mt-1 truncate font-mono text-muted-foreground text-xs">{agent.model}</p>
            {agent.last_message ? (
              <p className="mt-1 line-clamp-3 text-sm">{agent.last_message}</p>
            ) : null}
            <div className="mt-2 flex gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => void runAction(() => openSession(agent.id, { readOnly: true }))}
              >
                <ExternalLink aria-hidden />
                打开
              </Button>
              {active ? (
                <Button size="sm" variant="outline" onClick={() => void stop(agent)}>
                  <Square aria-hidden />
                  停止
                </Button>
              ) : null}
            </div>
          </li>
        );
      })}
    </ul>
  );
}

// ---------- 上下文 ----------

const SECTION_COLOR = [
  "bg-sky-500",
  "bg-violet-500",
  "bg-emerald-500",
  "bg-amber-500",
  "bg-rose-500",
  "bg-slate-500",
];

function ContextPane({ sessionId }: { sessionId: string }) {
  const context = useTranscript((state) => state.bySession[sessionId]?.context);
  const readOnly = useTranscript((state) => state.bySession[sessionId]?.readOnly ?? true);
  const set = useUi((state) => state.set);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void refreshSideData(sessionId);
  }, [sessionId]);

  if (!context) return <Empty>上下文用量暂不可用</Empty>;
  const window = Math.max(context.context_window, 1);
  const percent = Math.min(100, (context.total_tokens / window) * 100);

  const runCompact = async () => {
    setBusy(true);
    try {
      await compact(sessionId);
      await refreshSideData(sessionId);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-3 p-3">
      <div>
        <div className="mb-1 flex items-baseline justify-between text-sm">
          <span className="font-medium">
            {formatTokens(context.total_tokens)} / {formatTokens(context.context_window)}
          </span>
          <span className="text-muted-foreground text-xs">{percent.toFixed(1)}%</span>
        </div>
        <div
          className="flex h-2.5 overflow-hidden rounded-full bg-muted"
          role="img"
          aria-label={`上下文占用 ${percent.toFixed(1)}%`}
        >
          {context.sections.map((section, index) => (
            <div
              key={section.name}
              className={SECTION_COLOR[index % SECTION_COLOR.length]}
              style={{ width: `${(section.tokens / window) * 100}%` }}
            />
          ))}
        </div>
      </div>
      <ul className="space-y-1 text-sm">
        {context.sections.map((section, index) => (
          <li key={section.name} className="flex items-center gap-2">
            <span
              className={cn("size-2.5 rounded-sm", SECTION_COLOR[index % SECTION_COLOR.length])}
              aria-hidden
            />
            <span className="flex-1">{section.name}</span>
            <span className="font-mono text-muted-foreground text-xs">
              {formatTokens(section.tokens)}
            </span>
          </li>
        ))}
      </ul>
      <div className="flex gap-2">
        <Button size="sm" variant="outline" onClick={() => set({ contextContent: sessionId })}>
          查看正文
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={busy || readOnly}
          onClick={() => void runCompact()}
        >
          {busy ? <Loader2 className="animate-spin" aria-hidden /> : null}
          压缩
        </Button>
      </div>
    </div>
  );
}

// ---------- 旁路 ----------

const BTW_STATUS_LABEL = {
  waiting: "等待中…",
  generating: "生成中…",
  done: "",
  cancelled: "已取消",
  failed: "失败",
} as const;

function BtwPane({ sessionId }: { sessionId: string }) {
  const thread = useBtw((state) => state.threads[sessionId]);
  const { ask, cancel, clear } = useBtw.getState();
  const focusBtw = useUi((state) => state.focusBtw);
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const endRef = useRef<HTMLDivElement>(null);
  const inflight = thread?.inflight ?? null;
  const turns = thread?.turns ?? [];

  useEffect(() => {
    if (focusBtw > 0) inputRef.current?.focus();
  }, [focusBtw]);

  const lastAnswer = turns.at(-1)?.answer;
  // biome-ignore lint/correctness/useExhaustiveDependencies: 新内容到达时滚到底
  useEffect(() => {
    endRef.current?.scrollIntoView({ block: "end" });
  }, [turns.length, lastAnswer]);

  const submit = () => {
    const question = draft.trim();
    if (!question) return;
    if (!ask(sessionId, question)) {
      toast.error("已有一条旁路请求在进行");
      return;
    }
    setDraft("");
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-3">
        {turns.length === 0 ? (
          <p className="text-center text-muted-foreground text-sm">
            旁路问答不进入主转写，也不影响正在运行的任务。
          </p>
        ) : (
          turns.map((turn) => (
            <div key={turn.requestId} className="space-y-2">
              <p className="ml-auto w-fit max-w-[90%] rounded-xl bg-muted px-3 py-2 text-sm">
                {turn.question}
              </p>
              {turn.reasoning ? (
                <p className="line-clamp-4 whitespace-pre-wrap border-l-2 pl-2 text-muted-foreground text-xs">
                  {turn.reasoning}
                </p>
              ) : null}
              {turn.answer ? <Markdown text={turn.answer} className="text-sm" /> : null}
              {BTW_STATUS_LABEL[turn.status] ? (
                <p
                  className={cn(
                    "text-xs",
                    turn.status === "failed" ? "text-rose-500" : "text-muted-foreground",
                  )}
                >
                  {BTW_STATUS_LABEL[turn.status]}
                  {turn.error ? `：${turn.error}` : ""}
                </p>
              ) : null}
            </div>
          ))
        )}
        <div ref={endRef} />
      </div>
      <div className="space-y-2 border-t p-2">
        <Textarea
          ref={inputRef}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              submit();
            }
          }}
          placeholder="顺便问一句…（Enter 发送）"
          aria-label="旁路问题"
          className="max-h-40 min-h-16 resize-none text-sm"
        />
        <div className="flex gap-2">
          <Button size="sm" onClick={submit} disabled={!draft.trim() || inflight !== null}>
            <Send aria-hidden />
            发送
          </Button>
          {inflight !== null ? (
            <Button size="sm" variant="outline" onClick={() => cancel(sessionId)}>
              <Square aria-hidden />
              取消
            </Button>
          ) : null}
          <Button
            size="sm"
            variant="ghost"
            className="ml-auto"
            disabled={turns.length === 0}
            onClick={() => clear(sessionId)}
          >
            <Trash2 aria-hidden />
            清空
          </Button>
        </div>
      </div>
    </div>
  );
}

RIGHT_PANES.plan = PlansPane;
RIGHT_PANES.todo = TodoPane;
RIGHT_PANES.changes = ChangesPane;
RIGHT_PANES.agents = AgentsPane;
RIGHT_PANES.context = ContextPane;
RIGHT_PANES.btw = BtwPane;
