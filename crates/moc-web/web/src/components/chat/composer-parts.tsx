// Composer 的小部件：状态条（任务 / 提示 + 改动、子代理、费用）、待发队列、会话选项（权限 / 自主程度 / 引擎）、计划开关、上下文圆环、补全菜单。

import {
  CheckCircle2Icon,
  CheckIcon,
  ChevronDownIcon,
  CircleIcon,
  ListChecksIcon,
  Loader2Icon,
  LockIcon,
  ShieldAlertIcon,
  ShieldCheckIcon,
  ShieldIcon,
  XIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { DiffStat, diffStats } from "@/components/panels/diff-file";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { AUTONOMY } from "@/lib/autonomy";
import { sessionCost } from "@/lib/models";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  SessionEngine,
  SessionPermission,
  SessionUsageResult,
  TodoItem,
} from "@/protocol/types";
import type { PendingItem } from "@/store/composer";
import { selectModels, useConnection } from "@/store/connection";
import { useLayout } from "@/store/layout";
import {
  setSessionAutonomy,
  setSessionEngine,
  setSessionPermission,
  togglePlanMode,
} from "@/store/lifecycle";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { AgentsPopover, UsagePopover } from "./status-popovers";

export const CHIP =
  "inline-flex h-8 min-w-0 items-center gap-1.5 rounded-lg px-2.5 text-ui text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-50 data-[state=open]:bg-accent data-[state=open]:text-foreground";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// ---------- 状态条 ----------

/** 状态条右侧的小按钮：只变文字色，点击跳到检查面板对应分页。 */
const STAT =
  "inline-flex h-6 shrink-0 items-center gap-1.5 rounded-md px-1.5 transition-colors hover:text-foreground";

/** 提示文案：反引号片段按代码样式渲染。 */
function TipText({ tip }: { tip: string }) {
  return tip.split("`").map((part, index) =>
    index % 2 === 1 ? (
      // biome-ignore lint/suspicious/noArrayIndexKey: 片段由同一句文案切出，位置即身份
      <code key={index} className="font-mono text-muted-foreground">
        {part}
      </code>
    ) : (
      part
    ),
  );
}

/** 进入会话（含新会话）与每次开始运行时换一条，且不与上一条重复。 */
function useTip(sessionId: string, running: boolean): string | undefined {
  const tips = useConnection((state) => state.server?.tips);
  const count = tips?.length ?? 0;
  const [index, setIndex] = useState(() => Math.floor(Math.random() * Math.max(count, 1)));
  const seen = useRef({ sessionId, running });

  useEffect(() => {
    const previous = seen.current;
    seen.current = { sessionId, running };
    const entered = previous.sessionId !== sessionId;
    const started = running && !previous.running;
    if (count > 1 && (entered || started)) {
      setIndex((current) => (current + 1 + Math.floor(Math.random() * (count - 1))) % count);
    }
  }, [sessionId, running, count]);

  return tips?.[index % Math.max(count, 1)];
}

/** 会话累计费用：进入会话与每轮结束后重读 `session/usage`；没有定价或没有调用时为 0。 */
function useSessionCost(sessionId: string, running: boolean): number {
  const models = useConnection(selectModels);
  const [usage, setUsage] = useState<{ sessionId: string; result: SessionUsageResult } | null>(
    null,
  );
  const isDraft = sessionId === "draft-session";

  useEffect(() => {
    if (isDraft || running) return;
    let alive = true;
    rpc
      .call<SessionUsageResult>("session/usage", { session_id: sessionId })
      .then((result) => alive && setUsage({ sessionId, result }))
      .catch(() => alive && setUsage(null));
    return () => {
      alive = false;
    };
  }, [isDraft, running, sessionId]);

  return usage?.sessionId === sessionId ? sessionCost(usage.result, models) : 0;
}

function formatCost(value: number): string {
  return value < 0.01 ? "<$0.01" : `$${value.toFixed(2)}`;
}

/**
 * 托盘顶部 32px 状态条：左侧有任务时显示任务进度（可展开清单），否则显示随机提示；
 * 右侧依次是本会话改动、后台子代理与会话费用，各自有值才出现。
 */
export function ComposerStatus({ sessionId, todos }: { sessionId: string; todos: TodoItem[] }) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const running = Boolean(view?.running);
  const [open, setOpen] = useState(false);
  const tip = useTip(sessionId, running);
  const cost = useSessionCost(sessionId, running);

  const changes = useMemo(() => {
    const paths = new Set<string>();
    let added = 0;
    let removed = 0;
    for (const record of view?.diffs ?? []) {
      for (const file of record.diff.files) {
        paths.add(file.path);
        const stats = diffStats(file);
        added += stats.added;
        removed += stats.removed;
      }
    }
    return { files: paths.size, added, removed };
  }, [view?.diffs]);
  const agents = Object.values(view?.agents ?? {}).filter(
    (agent) => agent.status === "running" || agent.status === "queued",
  ).length;

  const done = todos.filter((item) => item.status === "completed").length;
  const current =
    todos.find((item) => item.status === "in_progress") ??
    todos.find((item) => item.status !== "completed");
  const inspect = (tab: "changes") => useLayout.getState().openInspector(tab);
  const agentTotal = Object.keys(view?.agents ?? {}).length;
  const popoverKind = useUi((state) => state.statusPopover);
  const popoverOpen = popoverKind?.sessionId === sessionId ? popoverKind.kind : null;
  const usageOpen = popoverOpen === "usage";
  const agentsOpen = popoverOpen === "agents";

  return (
    <div className="text-xs">
      <div className="flex h-7 items-center gap-2 px-3.5 text-text-tertiary">
        <div className="flex min-w-0 flex-1 items-center">
          {todos.length > 0 ? (
            <button
              type="button"
              onClick={() => setOpen((value) => !value)}
              className="flex h-6 min-w-0 items-center gap-1.5 rounded-md px-1.5 text-muted-foreground transition-colors hover:text-foreground"
              aria-expanded={open}
            >
              <ListChecksIcon className="size-3.5 shrink-0" />
              <span className="shrink-0 font-medium text-foreground">任务</span>
              <span className="shrink-0 tabular-nums text-text-tertiary">
                {done}/{todos.length}
              </span>
              {!open && current && <span className="min-w-0 truncate">{current.task}</span>}
              <ChevronDownIcon
                className={cn("size-3 shrink-0 transition-transform", !open && "rotate-180")}
              />
            </button>
          ) : (
            tip && (
              <p
                key={tip}
                className="min-w-0 truncate px-1.5 animate-in fade-in-0 duration-300"
                title={tip.replaceAll("`", "")}
              >
                <TipText tip={tip} />
              </p>
            )
          )}
        </div>
        {changes.files > 0 && (
          <button
            type="button"
            onClick={() => inspect("changes")}
            className={STAT}
            title="查看本会话改动"
          >
            <span>{changes.files} 个文件</span>
            <DiffStat added={changes.added} removed={changes.removed} />
          </button>
        )}
        {(agents > 0 || agentTotal > 0 || agentsOpen) && (
          <AgentsPopover sessionId={sessionId} running={agents} total={agentTotal} />
        )}
        {(cost > 0 || usageOpen) && <UsagePopover sessionId={sessionId} label={formatCost(cost)} />}
      </div>
      {open && todos.length > 0 && (
        <ul className="max-h-48 space-y-0.5 overflow-y-auto px-5 pt-0.5 pb-2 text-ui">
          {todos.map((item, index) => (
            <li
              // biome-ignore lint/suspicious/noArrayIndexKey: todo 由 core 整表替换，位置即身份，任务文本可重复
              key={index}
              className="flex items-start gap-2 py-0.5 leading-5"
            >
              <span className="flex h-5 shrink-0 items-center">
                {item.status === "completed" ? (
                  <CheckCircle2Icon className="size-3.5 text-success" />
                ) : item.status === "in_progress" ? (
                  <Loader2Icon className="size-3.5 animate-spin text-brand" />
                ) : (
                  <CircleIcon className="size-3.5 text-text-tertiary" />
                )}
              </span>
              <span
                className={cn(
                  "min-w-0",
                  item.status === "completed"
                    ? "text-text-tertiary line-through"
                    : "text-foreground",
                )}
              >
                {item.task}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ---------- 待发队列 ----------

export function PendingList({
  items,
  onRemove,
}: {
  items: PendingItem[];
  onRemove: (item: PendingItem) => void;
}) {
  return (
    <ul className="space-y-0.5 px-1 pb-1">
      {items.map((item) => {
        const locked = item.kind === "steer" && item.steerId === undefined;
        return (
          <li
            key={item.key}
            className="flex h-7 min-w-0 items-center gap-2 rounded-md px-1.5 text-xs hover:bg-accent/60"
          >
            <span
              className={cn(
                "shrink-0 rounded px-1.5 py-px text-2xs font-medium",
                item.kind === "steer" ? "bg-brand/10 text-brand" : "bg-card text-muted-foreground",
              )}
            >
              {item.kind === "steer" ? "补充" : item.mode === "ask" ? "问答" : "排队"}
            </span>
            <span className="min-w-0 flex-1 truncate text-foreground/90">
              {item.text || "图片"}
              {item.images.length > 0 && (
                <span className="text-text-tertiary"> · {item.images.length} 张图片</span>
              )}
            </span>
            <button
              type="button"
              onClick={() => onRemove(item)}
              disabled={locked}
              className="flex size-5 shrink-0 items-center justify-center rounded text-text-tertiary hover:bg-accent hover:text-foreground disabled:opacity-40"
              aria-label={item.kind === "steer" ? "撤回补充" : "移出队列"}
              title={item.kind === "steer" ? "撤回补充" : "移出队列"}
            >
              <XIcon className="size-3.5" />
            </button>
          </li>
        );
      })}
    </ul>
  );
}

// ---------- 会话选项：权限 + 自主程度 + 引擎 ----------

const PERMISSIONS: {
  id: SessionPermission;
  label: string;
  description: string;
  icon: typeof ShieldIcon;
}[] = [
  {
    id: "full",
    label: "完全访问",
    description: "不设沙盒边界，命令可读写整台机器",
    icon: ShieldAlertIcon,
  },
  {
    id: "workspace",
    label: "工作区可写",
    description: "宿主只读，只能写工作区与临时目录",
    icon: ShieldCheckIcon,
  },
  {
    id: "workspace_only",
    label: "仅限工作区",
    description: "工作区外的宿主路径不可读也不可写",
    icon: LockIcon,
  },
];

const ENGINES: { id: SessionEngine; label: string; description: string }[] = [
  { id: "standard", label: "标准", description: "模型直接调用工具" },
  { id: "ptc", label: "PTC", description: "模型只写代码，工具经代码沙盒调用" },
  { id: "clm_experimental", label: "CLM（实验）", description: "模型自主维护实时上下文" },
];

export function SessionOptions({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const info = useTranscript((state) => state.bySession[sessionId]?.info);
  const draft = useSessions((state) => state.draftSession);
  const isDraft = sessionId === "draft-session";
  // 草稿没选过权限时由 core 按 `[sandbox].default_mode` 决定，这里不臆测。
  const permission = isDraft ? draft?.permission : info?.permission;
  const current = PERMISSIONS.find((item) => item.id === permission);
  const engine = ENGINES.find((item) => item.id === info?.engine) ?? ENGINES[0];
  const autonomy = AUTONOMY.find((item) => item.id === info?.autonomy) ?? AUTONOMY[0];
  const Icon = current?.icon ?? ShieldIcon;

  const run = (action: Promise<void>) =>
    void action.catch((error) => toast.error(`切换失败：${errorText(error)}`));

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          className={CHIP}
          aria-label="会话权限、自主程度与引擎"
        >
          <Icon className="size-3.5 shrink-0" />
          <span className="truncate">{current?.label ?? "默认权限"}</span>
          {autonomy.id !== "default" && (
            <span className="truncate text-text-tertiary">· {autonomy.label}</span>
          )}
          {engine.id !== "standard" && <span className="text-text-tertiary">· {engine.label}</span>}
          <ChevronDownIcon className="size-3 shrink-0 opacity-60" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="top" sideOffset={8} className="w-72 p-1">
        <DropdownMenuLabel className="px-2 py-1.5 text-2xs font-medium text-text-tertiary">
          命令权限
        </DropdownMenuLabel>
        {PERMISSIONS.map((item) => (
          <OptionItem
            key={item.id}
            icon={<item.icon className="size-4" />}
            label={item.label}
            description={item.description}
            selected={item.id === permission}
            onSelect={() => run(setSessionPermission(sessionId, item.id))}
          />
        ))}
        <DropdownMenuSeparator />
        <OptionGroup title="自主程度" value={autonomy.label}>
          {AUTONOMY.map((item) => (
            <OptionItem
              key={item.id}
              label={item.label}
              description={item.description}
              selected={item.id === autonomy.id}
              onSelect={() => run(setSessionAutonomy(sessionId, item.id))}
            />
          ))}
        </OptionGroup>
        <OptionGroup
          title="会话引擎"
          value={info?.engine_locked ? `${engine.label} · 已锁定` : engine.label}
        >
          {ENGINES.map((item) => (
            <OptionItem
              key={item.id}
              label={item.label}
              description={item.description}
              selected={item.id === engine.id}
              disabled={info?.engine_locked && item.id !== engine.id}
              onSelect={() => run(setSessionEngine(sessionId, item.id))}
            />
          ))}
          {info?.engine_locked && (
            <p className="px-2 pt-1 pb-1.5 text-2xs text-text-tertiary">
              首条消息后引擎已锁定；换引擎请新建会话
            </p>
          )}
        </OptionGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** 次级选项：一行显示当前值，悬停展开子菜单，避免主菜单过长。 */
function OptionGroup({
  title,
  value,
  children,
}: {
  title: string;
  value: string;
  children: ReactNode;
}) {
  return (
    <DropdownMenuSub>
      <DropdownMenuSubTrigger className="h-8 rounded-lg px-2 text-ui">
        <span>{title}</span>
        <span className="ml-auto truncate text-xs text-text-tertiary">{value}</span>
      </DropdownMenuSubTrigger>
      <DropdownMenuSubContent sideOffset={6} className="w-72 rounded-xl p-1">
        {children}
      </DropdownMenuSubContent>
    </DropdownMenuSub>
  );
}

function OptionItem({
  icon,
  label,
  description,
  selected,
  disabled,
  onSelect,
}: {
  icon?: ReactNode;
  label: string;
  description: string;
  selected: boolean;
  disabled?: boolean;
  onSelect: () => void;
}) {
  return (
    <DropdownMenuItem
      disabled={disabled}
      onSelect={() => !selected && onSelect()}
      className="items-start gap-2.5 rounded-lg px-2 py-1.5"
    >
      {icon && <span className="mt-0.5 text-muted-foreground">{icon}</span>}
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="text-ui text-foreground">{label}</span>
        <span className="text-xs leading-4 text-text-tertiary">{description}</span>
      </span>
      <CheckIcon className={cn("mt-0.5 size-4 text-brand", !selected && "invisible")} />
    </DropdownMenuItem>
  );
}

// ---------- 计划开关 ----------

export function PlanToggle({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const active = useTranscript((state) => Boolean(state.bySession[sessionId]?.info.plan_active));
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          aria-pressed={active}
          onClick={() =>
            void togglePlanMode(sessionId, !active).catch((error) =>
              toast.error(`切换计划状态失败：${errorText(error)}`),
            )
          }
          className={cn(
            CHIP,
            active && "bg-brand/10 text-brand hover:bg-brand/15 hover:text-brand",
          )}
        >
          <ListChecksIcon className="size-3.5" />
          <span>计划</span>
        </button>
      </TooltipTrigger>
      <TooltipContent side="top">
        {active ? "退出计划状态" : "先写计划，审阅通过再动手"} · Alt+J
      </TooltipContent>
    </Tooltip>
  );
}

// ---------- 上下文圆环 ----------

// 圆环 + Popover 在 status-popovers.tsx；这里保留再导出给 composer-bar。
export { ContextRingPopover as ContextRing } from "./status-popovers";

// ---------- 补全菜单 ----------

export type Suggestion = {
  key: string;
  title: string;
  detail?: string;
  hint?: string;
  icon?: ReactNode;
};

export function SuggestionMenu({
  heading,
  items,
  active,
  onPick,
  onHover,
}: {
  heading: string;
  items: Suggestion[];
  active: number;
  onPick: (index: number) => void;
  onHover: (index: number) => void;
}) {
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    listRef.current
      ?.querySelector(`[data-index="${active}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [active]);

  return (
    <div className="absolute inset-x-0 bottom-full z-30 mb-2 overflow-hidden rounded-xl bg-popover shadow-[var(--elevation-popover)]">
      <div className="px-3 pt-2 pb-1 text-2xs font-medium text-text-tertiary">{heading}</div>
      <div ref={listRef} className="max-h-64 overflow-y-auto p-1" role="listbox">
        {items.map((item, index) => (
          <button
            key={item.key}
            type="button"
            role="option"
            aria-selected={index === active}
            data-index={index}
            // 按下即选中：避免 textarea 先失焦
            onMouseDown={(event) => {
              event.preventDefault();
              onPick(index);
            }}
            onMouseMove={() => onHover(index)}
            className={cn(
              "flex h-8 w-full items-center gap-2.5 rounded-lg px-2 text-left text-ui",
              index === active ? "bg-accent text-foreground" : "text-muted-foreground",
            )}
          >
            {item.icon && (
              <span className="flex size-4 shrink-0 items-center justify-center text-text-tertiary [&_svg]:size-3.5">
                {item.icon}
              </span>
            )}
            <span className="shrink-0 font-mono text-xs text-foreground">{item.title}</span>
            {item.hint && (
              <span className="shrink-0 font-mono text-xs text-text-tertiary">{item.hint}</span>
            )}
            {item.detail && (
              <span className="ml-auto min-w-0 truncate pl-3 text-xs text-text-tertiary">
                {item.detail}
              </span>
            )}
          </button>
        ))}
      </div>
    </div>
  );
}
