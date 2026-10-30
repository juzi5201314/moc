// Composer 的小部件：任务托盘、待发队列、会话选项（权限 / 自主程度 / 引擎）、计划开关、上下文圆环、补全菜单。

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
import { type ReactNode, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
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
import { compactTokens } from "@/lib/models";
import { cn } from "@/lib/utils";
import type { SessionEngine, SessionPermission, TodoItem } from "@/protocol/types";
import type { PendingItem } from "@/store/composer";
import { useLayout } from "@/store/layout";
import {
  setSessionAutonomy,
  setSessionEngine,
  setSessionPermission,
  togglePlanMode,
} from "@/store/lifecycle";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";

export const CHIP =
  "inline-flex h-8 min-w-0 items-center gap-1.5 rounded-lg px-2.5 text-[13px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-50 data-[state=open]:bg-accent data-[state=open]:text-foreground";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// ---------- 任务托盘 ----------

/** 输入框上沿的任务进度：收起时只露出进行中的一项。 */
export function TodoTray({ items }: { items: TodoItem[] }) {
  const [open, setOpen] = useState(false);
  const done = items.filter((item) => item.status === "completed").length;
  const current =
    items.find((item) => item.status === "in_progress") ??
    items.find((item) => item.status !== "completed");

  return (
    <div className="text-xs">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className="flex h-8 w-full items-center gap-2 rounded-lg px-2.5 text-muted-foreground transition-colors hover:text-foreground"
        aria-expanded={open}
      >
        <ListChecksIcon className="size-3.5 shrink-0" />
        <span className="shrink-0 font-medium text-foreground">任务</span>
        <span className="shrink-0 tabular-nums text-text-tertiary">
          {done}/{items.length}
        </span>
        {!open && current && <span className="min-w-0 truncate">{current.task}</span>}
        <ChevronDownIcon
          className={cn("ml-auto size-3.5 shrink-0 transition-transform", !open && "rotate-180")}
        />
      </button>
      {open && (
        <ul className="max-h-48 space-y-0.5 overflow-y-auto px-2.5 pb-2">
          {items.map((item, index) => (
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
                "shrink-0 rounded px-1.5 py-px text-[10.5px] font-medium",
                item.kind === "steer" ? "bg-brand/10 text-brand" : "bg-muted text-muted-foreground",
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
        <DropdownMenuLabel className="px-2 py-1.5 text-[11px] font-medium text-text-tertiary">
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
            <p className="px-2 pt-1 pb-1.5 text-[11px] text-text-tertiary">
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
      <DropdownMenuSubTrigger className="h-8 rounded-lg px-2 text-[13px]">
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
        <span className="text-[13px] text-foreground">{label}</span>
        <span className="text-[11.5px] leading-4 text-text-tertiary">{description}</span>
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

export function ContextRing({ used, window }: { used: number; window: number }) {
  const percent = Math.min(100, Math.round((used / window) * 100));
  const size = 18;
  const stroke = 2;
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={() => useLayout.getState().openInspector("context")}
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
      </TooltipTrigger>
      <TooltipContent side="top">
        上下文 {compactTokens(used) || "0"} / {compactTokens(window)} · {percent}%
      </TooltipContent>
    </Tooltip>
  );
}

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
    <div className="absolute inset-x-0 bottom-full z-30 mb-2 overflow-hidden rounded-xl border border-border bg-popover shadow-lg">
      <div className="flex items-center justify-between px-3 pt-2 pb-1 text-[11px] text-text-tertiary">
        <span className="font-medium">{heading}</span>
        <span>↑↓ 选择 · Tab 补全 · Esc 关闭</span>
      </div>
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
              "flex h-8 w-full items-center gap-2.5 rounded-lg px-2 text-left text-[13px]",
              index === active ? "bg-accent text-foreground" : "text-muted-foreground",
            )}
          >
            {item.icon && (
              <span className="flex size-4 shrink-0 items-center justify-center text-text-tertiary [&_svg]:size-3.5">
                {item.icon}
              </span>
            )}
            <span className="shrink-0 font-mono text-[12.5px] text-foreground">{item.title}</span>
            {item.hint && (
              <span className="shrink-0 font-mono text-[12px] text-text-tertiary">{item.hint}</span>
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
