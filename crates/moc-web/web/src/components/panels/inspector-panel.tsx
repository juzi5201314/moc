// 右侧检查面板：变更 / 计划 / 任务 / 子代理 / 上下文 / 用量 / 旁路。
// 分页标签带计数徽标；需要远端数据的分页只在打开时拉取。

import {
  BanIcon,
  CheckCircle2Icon,
  CircleIcon,
  CircleSlashIcon,
  Columns2Icon,
  Loader2Icon,
  RowsIcon,
  XIcon,
} from "lucide-react";
import { useMemo, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { DiffFile } from "@/components/panels/diff-file";
import { PlanReview } from "@/components/plan/plan-review";
import { cn } from "@/lib/utils";
import type {
  PlanAction,
  PlanInfo,
  SessionEngine,
  TodoStatus,
  ToolFileDiff,
} from "@/protocol/types";
import { approvePlan } from "@/store/actions";
import { type InspectorTab, useLayout } from "@/store/layout";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { AgentsTab, BtwTab, ContextTab, PanelEmpty, UsageTab } from "./inspector-tabs";

const TABS: { id: InspectorTab; label: string }[] = [
  { id: "changes", label: "变更" },
  { id: "plan", label: "计划" },
  { id: "todo", label: "任务" },
  { id: "agents", label: "子代理" },
  { id: "context", label: "上下文" },
  { id: "usage", label: "用量" },
  { id: "btw", label: "旁路" },
];

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function InspectorPanel({ sessionId }: { sessionId: string }) {
  const tab = useLayout((state) => state.inspectorTab);
  const view = useTranscript((state) => state.bySession[sessionId]);

  const counts: Partial<Record<InspectorTab, number>> = {
    changes: new Set(view?.diffs.flatMap((record) => record.diff.files.map((file) => file.path)))
      .size,
    plan: view?.plans.filter((plan) => plan.status === "submitted").length,
    todo: view?.todos.flatMap((phase) => phase.items).filter((item) => item.status !== "completed")
      .length,
    agents: Object.values(view?.agents ?? {}).filter(
      (agent) => agent.status === "running" || agent.status === "queued",
    ).length,
  };

  return (
    <div className="flex h-full w-full min-w-0 flex-col bg-background text-foreground">
      <div className="flex h-12 shrink-0 items-center gap-1 border-b border-border-subtle pr-2 pl-2">
        <div className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none]">
          {TABS.map((item) => {
            const count = counts[item.id] ?? 0;
            return (
              <button
                key={item.id}
                type="button"
                onClick={() => useLayout.getState().openInspector(item.id)}
                className={cn(
                  "inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg px-2.5 text-[13.5px] transition-colors",
                  tab === item.id
                    ? "bg-accent font-medium text-foreground"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                {item.label}
                {count > 0 && (
                  <span
                    className={cn(
                      "min-w-4 rounded-full px-1 text-center text-[11px] leading-4 tabular-nums",
                      item.id === "plan"
                        ? "bg-brand text-brand-foreground"
                        : "bg-foreground/10 text-muted-foreground",
                    )}
                  >
                    {count}
                  </span>
                )}
              </button>
            );
          })}
        </div>
        <IconButton
          label="关闭面板"
          shortcut="Alt R"
          size="sm"
          onClick={() => useLayout.getState().toggleRightbar(false)}
        >
          <XIcon />
        </IconButton>
      </div>

      <div className={cn("min-h-0 flex-1", tab === "btw" ? "flex flex-col" : "overflow-y-auto")}>
        {tab === "changes" && <ChangesTab sessionId={sessionId} />}
        {tab === "plan" && <PlanTab sessionId={sessionId} />}
        {tab === "todo" && <TodoTab sessionId={sessionId} />}
        {tab === "agents" && <AgentsTab key={sessionId} sessionId={sessionId} />}
        {tab === "context" && <ContextTab key={sessionId} sessionId={sessionId} />}
        {tab === "usage" && <UsageTab key={sessionId} sessionId={sessionId} />}
        {tab === "btw" && <BtwTab key={sessionId} sessionId={sessionId} />}
      </div>
    </div>
  );
}

// ---------- 变更 ----------

/** 同一文件的多次编辑合成一张卡片，hunk 按编辑先后排列。 */
function ChangesTab({ sessionId }: { sessionId: string }) {
  const diffs = useTranscript((state) => state.bySession[sessionId]?.diffs);
  const diffMode = usePrefs((state) => state.diffMode);
  const files = useMemo(() => {
    const byPath = new Map<string, ToolFileDiff>();
    for (const record of diffs ?? []) {
      for (const file of record.diff.files) {
        const existing = byPath.get(file.path);
        if (existing) existing.hunks = [...existing.hunks, ...file.hunks];
        else byPath.set(file.path, { path: file.path, hunks: [...file.hunks] });
      }
    }
    return [...byPath.values()];
  }, [diffs]);

  if (files.length === 0) {
    return (
      <PanelEmpty title="还没有文件变更" hint="模型通过编辑工具修改的文件会按文件汇总在这里。" />
    );
  }
  return (
    <div className="flex flex-col">
      <div className="flex h-10 items-center justify-between px-4">
        <span className="text-xs font-medium text-muted-foreground">{files.length} 个文件</span>
        <div className="flex items-center rounded-md bg-muted p-0.5">
          {(["unified", "split"] as const).map((mode) => (
            <IconButton
              key={mode}
              label={mode === "unified" ? "统一视图" : "并排视图"}
              size="xs"
              active={diffMode === mode}
              onClick={() => usePrefs.getState().set({ diffMode: mode })}
              className={cn("size-5", diffMode === mode && "bg-background shadow-xs")}
            >
              {mode === "unified" ? <RowsIcon /> : <Columns2Icon />}
            </IconButton>
          ))}
        </div>
      </div>
      <div className="space-y-2 px-3 pb-4">
        {files.map((file) => (
          <DiffFile key={file.path} file={file} defaultOpen={files.length <= 3} maxHeight={480} />
        ))}
      </div>
    </div>
  );
}

// ---------- 计划 ----------

function PlanTab({ sessionId }: { sessionId: string }) {
  const plans = useTranscript((state) => state.bySession[sessionId]?.plans);
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  // 修订后新版本是新的 plan_id；面板总是停在最新一版。
  const latest = useMemo(
    () => (plans ?? []).toSorted((left, right) => right.version - left.version)[0],
    [plans],
  );

  if (!latest) {
    return (
      <PanelEmpty
        title="还没有计划"
        hint="在输入框打开「计划」后发送任务，模型会先写出计划交给你审阅，批准后才开始改动。"
      />
    );
  }

  const act = async (plan: PlanInfo, action: PlanAction, executionEngine?: SessionEngine) => {
    setBusy(plan.plan_id);
    setActionError(null);
    try {
      await approvePlan(sessionId, plan.plan_id, action, executionEngine);
    } catch (error) {
      setActionError(errorText(error));
    } finally {
      setBusy(null);
    }
  };

  return (
    <PlanReview
      key={latest.plan_id}
      sessionId={sessionId}
      plan={latest}
      busy={busy === latest.plan_id}
      actionError={actionError}
      onAction={(plan, action, executionEngine) => void act(plan, action, executionEngine)}
    />
  );
}

// ---------- 任务 ----------

const TODO_ICON: Record<TodoStatus, { icon: typeof CircleIcon; className: string; label: string }> =
  {
    completed: { icon: CheckCircle2Icon, className: "text-success", label: "已完成" },
    in_progress: { icon: Loader2Icon, className: "animate-spin text-brand", label: "进行中" },
    pending: { icon: CircleIcon, className: "text-text-tertiary", label: "待办" },
    blocked: { icon: BanIcon, className: "text-warn", label: "受阻" },
    abandoned: { icon: CircleSlashIcon, className: "text-text-tertiary", label: "已放弃" },
  };

function TodoTab({ sessionId }: { sessionId: string }) {
  const phases = useTranscript((state) => state.bySession[sessionId]?.todos);
  if (!phases?.some((phase) => phase.items.length > 0)) {
    return (
      <PanelEmpty title="还没有任务清单" hint="模型拆解多步任务时维护的 todo 会实时同步到这里。" />
    );
  }
  return (
    <div className="space-y-4 px-3 py-3">
      {phases.map((phase, phaseIndex) => {
        const done = phase.items.filter((item) => item.status === "completed").length;
        return (
          // biome-ignore lint/suspicious/noArrayIndexKey: todo 由 core 整表替换，位置即身份，阶段名可重复
          <section key={phaseIndex}>
            <div className="mb-1 flex items-baseline justify-between gap-2 px-2">
              <h3 className="min-w-0 truncate text-[13.5px] font-medium">
                {phase.phase || "任务"}
              </h3>
              <span className="shrink-0 text-[11px] tabular-nums text-text-tertiary">
                {done}/{phase.items.length}
              </span>
            </div>
            <ul>
              {phase.items.map((item, index) => {
                const meta = TODO_ICON[item.status];
                const Icon = meta.icon;
                const finished = item.status === "completed" || item.status === "abandoned";
                return (
                  <li
                    // biome-ignore lint/suspicious/noArrayIndexKey: 同上，任务文本可重复
                    key={index}
                    className="flex gap-2.5 rounded-lg px-2 py-1.5"
                  >
                    <span className="flex h-5 shrink-0 items-center" title={meta.label}>
                      <Icon className={cn("size-3.5", meta.className)} />
                    </span>
                    <span className="min-w-0 flex-1 text-[13.5px] leading-5">
                      <span
                        className={cn(
                          finished ? "text-text-tertiary" : "text-foreground",
                          item.status === "abandoned" && "line-through",
                        )}
                      >
                        {item.task}
                      </span>
                      {item.reason && (
                        <span className="block text-xs text-text-tertiary">{item.reason}</span>
                      )}
                    </span>
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
