// 任务清单：工具简略档保留整份清单的前三项和后两项，完整档显示全部。

import { BanIcon, CheckCircle2Icon, CircleIcon, CircleSlashIcon, Loader2Icon } from "lucide-react";
import { Fragment } from "react";
import { cn } from "@/lib/utils";
import type { TodoPhase, TodoStatus } from "@/protocol/types";

const TODO_ICON: Record<TodoStatus, { icon: typeof CircleIcon; className: string; label: string }> =
  {
    completed: { icon: CheckCircle2Icon, className: "text-success", label: "已完成" },
    in_progress: { icon: Loader2Icon, className: "animate-spin text-brand", label: "进行中" },
    pending: { icon: CircleIcon, className: "text-text-tertiary", label: "待办" },
    blocked: { icon: BanIcon, className: "text-warn", label: "受阻" },
    abandoned: { icon: CircleSlashIcon, className: "text-text-tertiary", label: "已放弃" },
  };

export function TodoPhases({
  phases,
  brief = false,
  className,
}: {
  phases: TodoPhase[];
  brief?: boolean;
  className?: string;
}) {
  const total = phases.reduce((count, phase) => count + phase.items.length, 0);
  const omitted = brief ? Math.max(0, total - 5) : 0;
  let offset = 0;
  return (
    <div className={cn("space-y-4", className)}>
      {phases.map((phase, phaseIndex) => {
        const start = offset;
        offset += phase.items.length;
        if (omitted > 0 && start >= 3 && offset <= total - 2) return null;
        const done = phase.items.filter((item) => item.status === "completed").length;
        return (
          // biome-ignore lint/suspicious/noArrayIndexKey: todo 由 core 整表替换，位置即身份，阶段名可重复
          <section key={phaseIndex}>
            <div className="mb-1 flex items-baseline justify-between gap-2 px-2">
              <h3 className="min-w-0 truncate text-sm font-medium">{phase.phase || "任务"}</h3>
              <span className="shrink-0 text-2xs tabular-nums text-text-tertiary">
                {done}/{phase.items.length}
              </span>
            </div>
            <ul>
              {phase.items.map((item, index) => {
                const position = start + index;
                if (omitted > 0 && position >= 3 && position < total - 2) return null;
                const meta = TODO_ICON[item.status];
                const Icon = meta.icon;
                const finished = item.status === "completed" || item.status === "abandoned";
                return (
                  // biome-ignore lint/suspicious/noArrayIndexKey: todo 快照按位置标识任务
                  <Fragment key={index}>
                    <li className="flex gap-2.5 rounded-lg px-2 py-1.5">
                      <span className="flex h-5 shrink-0 items-center" title={meta.label}>
                        <Icon className={cn("size-3.5", meta.className)} />
                      </span>
                      <span className="min-w-0 flex-1 text-sm leading-5">
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
                    {omitted > 0 && position === 2 && (
                      <li className="px-2 py-1.5 text-xs text-text-tertiary">
                        ⋯ 已省略 {omitted} 项
                      </li>
                    )}
                  </Fragment>
                );
              })}
            </ul>
          </section>
        );
      })}
    </div>
  );
}
