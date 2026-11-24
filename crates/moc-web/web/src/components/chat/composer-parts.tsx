// Composer 的小部件：状态条（任务 / 提示 + 改动、子代理、费用）、待发队列、会话选项（权限 / 自主程度 / 引擎）、计划开关、上下文圆环、补全菜单。

import {
  CheckCircle2Icon,
  CheckIcon,
  ChevronDownIcon,
  CircleIcon,
  CornerUpLeftIcon,
  GripVerticalIcon,
  ListChecksIcon,
  Loader2Icon,
  LockIcon,
  PencilIcon,
  RefreshCwIcon,
  ShieldAlertIcon,
  ShieldCheckIcon,
  ShieldIcon,
  TerminalIcon,
  XIcon,
} from "lucide-react";
import {
  type CSSProperties,
  type DragEvent,
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { toast } from "sonner";
import { DiffStat, diffStats } from "@/components/panels/diff-file";
import { Button } from "@/components/ui/button";
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
import { Popover, PopoverAnchor, PopoverContent } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { AUTONOMY } from "@/lib/autonomy";
import { type MsgKey, useT } from "@/lib/i18n";
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
import { AgentsPopover, McpPopover, UsagePopover } from "./status-popovers";

export function ShellModeNotice({
  text,
  permission,
}: {
  text: string;
  permission?: SessionPermission;
}) {
  const t = useT();
  if (!text.startsWith("!")) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 px-4 pt-2.5 text-xs font-medium text-warn">
      <TerminalIcon className="size-3.5 shrink-0" />
      <span>{text.startsWith("!!") ? t("cb.bash_exclude") : t("cb.bash_include")}</span>
      {permission && permission !== "full" && <span>{t("cb.bash_full_access")}</span>}
    </div>
  );
}

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
      // leading-none：等宽字体的上下伸部与正文不同，继承行高会撑高行盒，使整行偏离状态条中线。
      // biome-ignore lint/suspicious/noArrayIndexKey: 片段由同一句文案切出，位置即身份
      <code key={index} className="font-mono leading-none text-muted-foreground">
        {part}
      </code>
    ) : (
      part
    ),
  );
}

/**
 * 状态条提示：放不下时右缘渐隐示意还有后文，悬停后整行匀速平移到末尾，移开复位。
 * 量内层的 offsetWidth：平移走 transform 不改变它，悬停中重测也不会归零。
 */
function TipLine({ tip }: { tip: string }) {
  const t = useT();
  const box = useRef<HTMLParagraphElement>(null);
  const line = useRef<HTMLSpanElement>(null);
  const [overflow, setOverflow] = useState(0);

  useLayoutEffect(() => {
    const outer = box.current;
    const inner = line.current;
    if (!outer || !inner) return;
    const measure = () =>
      setOverflow(Math.max(0, Math.ceil(inner.offsetWidth - outer.clientWidth)));
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(outer);
    observer.observe(inner);
    return () => observer.disconnect();
  }, []);

  // 平移与渐隐没有现成组件，用最少的 Tailwind 组合；速度 50px/s，减弱动效时直接跳到末尾。
  return (
    <p
      ref={box}
      className={cn(
        "group/tip ml-1.5 mr-1 min-w-0 overflow-hidden whitespace-nowrap animate-in fade-in-0 duration-300",
        overflow > 0 && "mask-r-from-[calc(100%-1.5rem)] hover:mask-none",
      )}
      title={tip.replaceAll("`", "")}
    >
      <span
        ref={line}
        className="inline-block transition-transform duration-300 group-hover/tip:translate-x-(--tip-shift) group-hover/tip:delay-300 group-hover/tip:duration-(--tip-duration) group-hover/tip:ease-linear motion-reduce:transition-none"
        style={
          {
            "--tip-shift": `-${overflow}px`,
            "--tip-duration": `${overflow * 20}ms`,
          } as CSSProperties
        }
      >
        <span className="mr-1 font-medium">{t("cp.tips")}</span>
        <TipText tip={tip} />
      </span>
    </p>
  );
}

function nextTipIndex(current: number, count: number): number {
  return (current + 1 + Math.floor(Math.random() * (count - 1))) % count;
}

/** 进入会话（含新会话）、每次开始运行或手动刷新时随机换一条，且不与上一条重复。 */
function useTip(sessionId: string, running: boolean) {
  const tips = useConnection((state) => state.server?.tips);
  const count = tips?.length ?? 0;
  const [index, setIndex] = useState(() => Math.floor(Math.random() * Math.max(count, 1)));
  const seen = useRef({ sessionId, running });

  const refresh = useCallback(() => {
    if (count > 1) {
      setIndex((current) => nextTipIndex(current, count));
    }
  }, [count]);

  useEffect(() => {
    const previous = seen.current;
    seen.current = { sessionId, running };
    const entered = previous.sessionId !== sessionId;
    const started = running && !previous.running;
    if (count > 1 && (entered || started)) {
      setIndex((current) => nextTipIndex(current, count));
    }
  }, [sessionId, running, count]);

  return {
    tip: tips?.[index % Math.max(count, 1)],
    canRefresh: count > 1,
    refresh,
  };
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
 * 托盘顶部 36px 状态条：左侧有任务时显示任务进度（可展开清单），右侧显示本会话改动；
 * 没有任务但有改动时左侧显示改动汇总、右侧「查看变更」；都没有时显示随机提示。
 * 后台子代理与会话费用始终排在最右，各自有值才出现。
 */
export function ComposerStatus({ sessionId, todos }: { sessionId: string; todos: TodoItem[] }) {
  const t = useT();
  const view = useTranscript((state) => state.bySession[sessionId]);
  const running = Boolean(view?.running);
  const [open, setOpen] = useState(false);
  const { tip, canRefresh, refresh: refreshTip } = useTip(sessionId, running);
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
    <div className="text-ui">
      <div className="flex h-9 items-center gap-2 px-2.5 text-muted-foreground">
        <div className="flex min-w-0 flex-1 items-center">
          {todos.length > 0 ? (
            <button
              type="button"
              onClick={() => setOpen((value) => !value)}
              className="flex h-6 min-w-0 items-center gap-1.5 rounded-md px-1.5 text-muted-foreground transition-colors hover:text-foreground"
              aria-expanded={open}
            >
              <ListChecksIcon className="size-3.5 shrink-0" />
              <span className="shrink-0 font-medium text-foreground">{t("cp.tasks")}</span>
              <span className="shrink-0 tabular-nums text-muted-foreground">
                {done}/{todos.length}
              </span>
              {!open && current && <span className="min-w-0 truncate">{current.task}</span>}
              <ChevronDownIcon
                className={cn("size-3 shrink-0 transition-transform", !open && "rotate-180")}
              />
            </button>
          ) : changes.files > 0 ? (
            <div className="flex min-w-0 items-center gap-2 px-1.5 text-sm leading-5">
              <PencilIcon className="size-4 shrink-0 text-muted-foreground" />
              <span className="truncate text-foreground">
                {t("cp.files_changed", { n: changes.files })}
              </span>
              <DiffStat
                added={changes.added}
                removed={changes.removed}
                className="font-sans text-sm"
              />
            </div>
          ) : (
            tip && (
              <>
                <TipLine key={tip} tip={tip} />
                {canRefresh && (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-xs"
                        aria-label={t("cp.next_tip")}
                        onClick={refreshTip}
                        className="text-text-tertiary hover:bg-accent hover:text-foreground dark:hover:bg-accent"
                      >
                        <RefreshCwIcon />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent side="top">{t("cp.next_tip")}</TooltipContent>
                  </Tooltip>
                )}
              </>
            )
          )}
        </div>
        {todos.length > 0 && changes.files > 0 && (
          <button
            type="button"
            onClick={() => inspect("changes")}
            className={STAT}
            title={t("cp.changes")}
          >
            <span>{t("cp.files_n", { n: changes.files })}</span>
            <DiffStat
              added={changes.added}
              removed={changes.removed}
              className="font-sans text-ui"
            />
          </button>
        )}
        <McpPopover sessionId={sessionId} />
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

/** 行尾操作按钮：悬停 / 键盘聚焦该行时浮现。 */
const ROW_ACTION =
  "flex size-5 shrink-0 items-center justify-center rounded text-text-tertiary hover:bg-accent hover:text-foreground disabled:opacity-40";

/**
 * 待发区卡片：头部计数 + 折叠，行内拖动排序；
 * 排队项可编辑回草稿或立即发送（移到队首并中断当前轮），补充项只支持撤回。
 */
export function PendingList({
  items,
  onRemove,
  onEdit,
  onSendNow,
  onMove,
}: {
  items: PendingItem[];
  onRemove: (item: PendingItem) => void;
  onEdit: (item: PendingItem) => void;
  onSendNow: (item: PendingItem) => void;
  onMove: (key: string, toIndex: number) => void;
}) {
  const t = useT();
  const [collapsed, setCollapsed] = useState(false);
  const [dragKey, setDragKey] = useState<string | null>(null);

  // 原生 HTML5 拖动：拖到目标行上/下半区即时重排，无第三方依赖。
  const onDragOver = (event: DragEvent<HTMLLIElement>, item: PendingItem) => {
    if (!dragKey || dragKey === item.key) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
    const from = items.findIndex((entry) => entry.key === dragKey);
    if (from < 0) return;
    let to = items.indexOf(item);
    const rect = event.currentTarget.getBoundingClientRect();
    if (event.clientY > rect.top + rect.height / 2) to += 1;
    if (to > from) to -= 1;
    if (to !== from) onMove(dragKey, to);
  };

  return (
    <div className="rounded-[16px] bg-card shadow-[var(--elevation-composer-inset)]">
      <button
        type="button"
        onClick={() => setCollapsed((value) => !value)}
        aria-expanded={!collapsed}
        className="flex h-8 w-full items-center gap-1.5 rounded-[16px] px-3 text-xs text-muted-foreground transition-colors hover:text-foreground"
      >
        <span className="font-medium">{t("q.count", { n: items.length })}</span>
        <ChevronDownIcon
          className={cn("ml-auto size-3.5 transition-transform", collapsed && "rotate-180")}
        />
      </button>
      {!collapsed && (
        <ul className="space-y-0.5 px-1.5 pb-1.5">
          {items.map((item) => {
            const locked = item.kind === "steer" && item.steerId === undefined;
            const queued = item.kind === "queue";
            return (
              <li
                key={item.key}
                draggable
                onDragStart={(event) => {
                  setDragKey(item.key);
                  event.dataTransfer.setData("text/plain", item.key);
                  event.dataTransfer.effectAllowed = "move";
                }}
                onDragOver={(event) => onDragOver(event, item)}
                onDragEnd={() => setDragKey(null)}
                className={cn(
                  "group flex h-7 min-w-0 items-center gap-1.5 rounded-md px-1.5 text-xs hover:bg-accent/60",
                  dragKey === item.key && "opacity-50",
                )}
              >
                <GripVerticalIcon
                  className="size-3.5 shrink-0 cursor-grab text-text-tertiary"
                  aria-label={t("q.drag")}
                />
                <span
                  className={cn(
                    "shrink-0 rounded px-1.5 py-px text-2xs font-medium",
                    item.kind === "steer"
                      ? "bg-brand/10 text-brand"
                      : "bg-card text-muted-foreground",
                  )}
                >
                  {item.kind === "steer"
                    ? t("q.steer")
                    : item.mode === "ask"
                      ? t("q.ask")
                      : t("q.queue")}
                </span>
                <span className="min-w-0 flex-1 truncate text-foreground/90">
                  {item.text || t("q.image")}
                  {item.images.length > 0 && (
                    <span className="text-text-tertiary">
                      {t("q.images_n", { n: item.images.length })}
                    </span>
                  )}
                </span>
                <span className="flex shrink-0 items-center opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
                  {queued && (
                    <>
                      <button
                        type="button"
                        onClick={() => onEdit(item)}
                        className={ROW_ACTION}
                        aria-label={t("q.edit")}
                        title={t("q.edit")}
                      >
                        <PencilIcon className="size-3.5" />
                      </button>
                      <button
                        type="button"
                        onClick={() => onSendNow(item)}
                        className={ROW_ACTION}
                        aria-label={t("q.send_now")}
                        title={t("q.send_now")}
                      >
                        <CornerUpLeftIcon className="size-3.5" />
                      </button>
                    </>
                  )}
                  <button
                    type="button"
                    onClick={() => onRemove(item)}
                    disabled={locked}
                    className={ROW_ACTION}
                    aria-label={item.kind === "steer" ? t("q.unsteer") : t("q.dequeue")}
                    title={item.kind === "steer" ? t("q.unsteer") : t("q.dequeue")}
                  >
                    <XIcon className="size-3.5" />
                  </button>
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

// ---------- 会话选项：权限 + 自主程度 + 引擎 ----------

const PERMISSIONS: {
  id: SessionPermission;
  label: MsgKey;
  description: MsgKey;
  icon: typeof ShieldIcon;
}[] = [
  {
    id: "full",
    label: "perm.full",
    description: "perm.full_desc",
    icon: ShieldAlertIcon,
  },
  {
    id: "workspace",
    label: "perm.workspace",
    description: "perm.workspace_desc",
    icon: ShieldCheckIcon,
  },
  {
    id: "workspace_only",
    label: "perm.workspace_only",
    description: "perm.workspace_only_desc",
    icon: LockIcon,
  },
];

const ENGINES: { id: SessionEngine; label: MsgKey; description: MsgKey }[] = [
  { id: "standard", label: "engine.standard", description: "engine.standard_desc" },
  { id: "ptc", label: "engine.ptc", description: "engine.ptc_desc" },
  { id: "clm_experimental", label: "engine.clm", description: "engine.clm_desc" },
];

export function SessionOptions({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const t = useT();
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
    void action.catch((error) => toast.error(t("sc.switch_failed", { error: errorText(error) })));

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button type="button" disabled={disabled} className={CHIP} aria-label={t("cp.options")}>
          <Icon className="size-3.5 shrink-0" />
          <span className="truncate">{current ? t(current.label) : t("cp.perm_default")}</span>
          {autonomy.id !== "default" && (
            <span className="truncate text-text-tertiary">· {t(autonomy.label)}</span>
          )}
          {engine.id !== "standard" && (
            <span className="text-text-tertiary">· {t(engine.label)}</span>
          )}
          <ChevronDownIcon className="size-3 shrink-0 opacity-60" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="top" sideOffset={8} className="w-72 p-1">
        <DropdownMenuLabel className="px-2 py-1.5 text-2xs font-medium text-text-tertiary">
          {t("cp.perm_title")}
        </DropdownMenuLabel>
        {PERMISSIONS.map((item) => (
          <OptionItem
            key={item.id}
            icon={<item.icon className="size-4" />}
            label={t(item.label)}
            description={t(item.description)}
            selected={item.id === permission}
            onSelect={() => run(setSessionPermission(sessionId, item.id))}
          />
        ))}
        <DropdownMenuSeparator />
        <OptionGroup title={t("cp.autonomy")} value={t(autonomy.label)}>
          {AUTONOMY.map((item) => (
            <OptionItem
              key={item.id}
              label={t(item.label)}
              description={t(item.description)}
              selected={item.id === autonomy.id}
              onSelect={() => run(setSessionAutonomy(sessionId, item.id))}
            />
          ))}
        </OptionGroup>
        <OptionGroup
          title={t("cp.engine")}
          value={
            info?.engine_locked
              ? t("cp.engine_locked", { label: t(engine.label) })
              : t(engine.label)
          }
        >
          {ENGINES.map((item) => (
            <OptionItem
              key={item.id}
              label={t(item.label)}
              description={t(item.description)}
              selected={item.id === engine.id}
              disabled={info?.engine_locked && item.id !== engine.id}
              onSelect={() => run(setSessionEngine(sessionId, item.id))}
            />
          ))}
          {info?.engine_locked && (
            <p className="px-2 pt-1 pb-1.5 text-2xs text-text-tertiary">
              {t("cp.engine_locked_hint")}
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

/** 只在 plan 模式下出现，点击退出；进入走 `/plan` 或 Alt+J。 */
export function PlanToggle({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const t = useT();
  const active = useTranscript((state) => Boolean(state.bySession[sessionId]?.info.plan_active));
  if (!active) return null;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          onClick={() =>
            void togglePlanMode(sessionId, false).catch((error) =>
              toast.error(t("slash.plan_failed", { error: errorText(error) })),
            )
          }
          className={cn(CHIP, "bg-brand/10 text-brand hover:bg-brand/15 hover:text-brand")}
        >
          <ListChecksIcon className="size-3.5" />
          <span>{t("cp.plan")}</span>
        </button>
      </TooltipTrigger>
      <TooltipContent side="top">{t("cp.plan_exit")} · Alt+J</TooltipContent>
    </Tooltip>
  );
}

// ---------- 上下文圆环 ----------

// 上下文圆环与 /context 打开同一个总览。
export { ContextRing } from "./status-popovers";

// ---------- 补全菜单 ----------

export type Suggestion = {
  key: string;
  title: string;
  /** `title` 中命中查询的字符下标（码点序号），高亮显示。 */
  titleMatches?: number[];
  /** 标题后的次要信息（如文件所在目录），放不下时从左侧省略。 */
  subtitle?: string;
  subtitleMatches?: number[];
  detail?: string;
  hint?: string;
  /** 详情区首行；缺省为 `title`。 */
  signature?: string;
  /** 来源元信息（如插件名），展示在详情区底部。 */
  source?: string;
  icon?: ReactNode;
};

/** 侧边独立详情卡低于此宽度时，降级为列表远端内嵌详情栏。 */
const SIDE_CARD_MIN_WIDTH = 220;
/** 侧边卡片间距（sideOffset=8）与视口碰撞留白（collisionPadding=12）之和。 */
const SIDE_CHROME_PX = 20;

/** 按码点下标高亮命中字符；无命中时原样返回文本。 */
function Highlighted({ text, matches }: { text: string; matches?: number[] }) {
  if (!matches?.length) return text;
  const hit = new Set(matches);
  const parts: { text: string; hit: boolean }[] = [];
  Array.from(text).forEach((char, index) => {
    const last = parts.at(-1);
    if (last && last.hit === hit.has(index)) last.text += char;
    else parts.push({ text: char, hit: hit.has(index) });
  });
  return parts.map((part, index) =>
    part.hit ? (
      // biome-ignore lint/suspicious/noArrayIndexKey: 片段按文本顺序生成，不会重排
      <span key={index} className="text-brand">
        {part.text}
      </span>
    ) : (
      part.text
    ),
  );
}

/** 详情卡正文：完整折行展示命令名/路径、参数语法、说明与来源，不截断、不靠隐形占位撑高。 */
function SuggestionDetailBody({ item }: { item: Suggestion }) {
  return (
    <div className="space-y-1.5 text-xs">
      <p className="break-all font-mono text-xs font-medium text-foreground">
        {item.signature ?? item.title}
      </p>
      {item.hint && (
        <p className="rounded-md bg-[var(--inline-code-bg)] px-2 py-1 font-mono text-2xs leading-relaxed text-muted-foreground [overflow-wrap:anywhere]">
          {item.hint}
        </p>
      )}
      {item.detail && (
        <p className="leading-relaxed break-words text-muted-foreground">{item.detail}</p>
      )}
      {item.source && <p className="text-2xs text-text-tertiary">{item.source}</p>}
    </div>
  );
}

type MenuLayout = {
  placed: boolean;
  actualSide: "top" | "bottom";
  dockSide: boolean;
  /** 外层定位 wrapper 每次变更 transform/尺寸时更新，用于触发侧边浮层同步重定位。 */
  anchorKey: string;
};

/**
 * 观测补全主浮层的真实定位方向与左右剩余空间：
 * - 等外层 Radix Popper 结束 `translate(0, -200%)` 预测量后再挂载侧边浮层，避免首帧闪到屏外；
 * - 任一侧可用宽度 ≥ 220px 时启用侧边独立详情卡，否则降级为远离输入框一端的内嵌栏。
 */
function useSuggestionLayout(
  menuEl: HTMLDivElement | null,
  preferredSide: "top" | "bottom",
): MenuLayout {
  const [layout, setLayout] = useState<MenuLayout>({
    placed: false,
    actualSide: preferredSide,
    dockSide: true,
    anchorKey: "",
  });

  useLayoutEffect(() => {
    if (!menuEl) return;
    const wrapper = menuEl.closest<HTMLElement>("[data-radix-popper-content-wrapper]");
    const measure = () => {
      const transform = wrapper?.style.transform ?? "";
      // Radix Popper 尚未定位完时会把 wrapper 移到 -200%，此时不读取坐标。
      if (transform.includes("-200%")) return;
      const rect = menuEl.getBoundingClientRect();
      const actualSide: "top" | "bottom" =
        menuEl.dataset.side === "bottom"
          ? "bottom"
          : menuEl.dataset.side === "top"
            ? "top"
            : preferredSide;
      const availableSide = Math.max(window.innerWidth - rect.right, rect.left) - SIDE_CHROME_PX;
      const dockSide = availableSide >= SIDE_CARD_MIN_WIDTH;
      const anchorKey = `${transform}|${Math.round(rect.width)}x${Math.round(rect.height)}|${actualSide}`;
      setLayout((prev) =>
        prev.placed &&
        prev.actualSide === actualSide &&
        prev.dockSide === dockSide &&
        prev.anchorKey === anchorKey
          ? prev
          : { placed: true, actualSide, dockSide, anchorKey },
      );
    };

    measure();
    const resizeObserver = new ResizeObserver(measure);
    resizeObserver.observe(menuEl);
    const mutationObserver = new MutationObserver(measure);
    if (wrapper) {
      mutationObserver.observe(wrapper, { attributes: true, attributeFilter: ["style"] });
    }
    mutationObserver.observe(menuEl, { attributes: true, attributeFilter: ["data-side"] });
    window.addEventListener("resize", measure);
    return () => {
      resizeObserver.disconnect();
      mutationObserver.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [menuEl, preferredSide]);

  return layout;
}

export function SuggestionMenu({
  id,
  side,
  wide = false,
  heading,
  items,
  active,
  onPick,
  onHover,
  inputRef,
}: {
  id: string;
  side: "bottom" | "top";
  /** 路径类候选需要更宽的浮层。 */
  wide?: boolean;
  heading: string;
  items: Suggestion[];
  active: number;
  onPick: (index: number) => void;
  onHover: (index: number) => void;
  /** 输入框容器：在其中点击（移动光标）不算外部交互，不关闭补全。 */
  inputRef: RefObject<HTMLElement | null>;
}) {
  const listRef = useRef<HTMLDivElement>(null);
  const detailRef = useRef<HTMLDivElement>(null);
  const [menuEl, setMenuEl] = useState<HTMLDivElement | null>(null);
  const [titleTruncated, setTitleTruncated] = useState(false);

  const safeActive = Math.min(Math.max(active, 0), Math.max(items.length - 1, 0));
  const current = items[safeActive];
  const { placed, actualSide, dockSide, anchorKey } = useSuggestionLayout(menuEl, side);

  useEffect(() => {
    listRef.current
      ?.querySelector(`[data-index="${safeActive}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [safeActive]);

  // 即使某项既无说明也无参数，若超长标题在行内被截断，仍在详情卡里展示完整文本。
  const currentTitle = current?.title;
  useLayoutEffect(() => {
    if (!anchorKey || !currentTitle) {
      setTitleTruncated(false);
      return;
    }
    const titleEl = listRef.current?.querySelector<HTMLElement>(
      `[data-index="${safeActive}"] [data-slot="suggestion-title"]`,
    );
    setTitleTruncated(Boolean(titleEl && titleEl.scrollWidth > titleEl.clientWidth + 1));
  }, [safeActive, currentTitle, anchorKey]);

  // 依赖 anchorKey：外层列表因过滤缩高/位移时刷新锚对象引用，使侧卡立即跟上。
  const anchorRef = useMemo(
    () =>
      menuEl && anchorKey
        ? {
            current: {
              getBoundingClientRect: () => menuEl.getBoundingClientRect(),
              contextElement: menuEl,
            },
          }
        : undefined,
    [menuEl, anchorKey],
  );

  const hasDetail = Boolean(
    current &&
      (current.detail || current.hint || current.signature || current.source || titleTruncated),
  );
  const showSideDetail = Boolean(placed && dockSide && hasDetail && anchorRef);
  const showInlineDetail = Boolean(placed && !dockSide && hasDetail);
  // 窄屏降级时把详情放在远离输入框的一端（top 置顶、bottom 置底），避免列表位移动摇悬停焦点。
  const inlineDetail = showInlineDetail && current && (
    <div
      ref={detailRef}
      role="note"
      data-slot="suggestion-detail"
      data-placement={actualSide === "top" ? "inline-top" : "inline-bottom"}
      className={cn(
        "max-h-40 shrink-0 overflow-y-auto px-2",
        actualSide === "top"
          ? "mb-1 border-b border-border-subtle pt-1 pb-2"
          : "mt-1 border-t border-border-subtle pt-2 pb-1",
      )}
    >
      <SuggestionDetailBody item={current} />
    </div>
  );

  return (
    <PopoverContent
      ref={setMenuEl}
      side={side}
      align="start"
      sideOffset={4}
      alignOffset={8}
      collisionPadding={12}
      role="presentation"
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onInteractOutside={(event) => {
        const target = event.target as Node | null;
        if (inputRef.current?.contains(target) || detailRef.current?.contains(target)) {
          event.preventDefault();
        }
      }}
      className={cn(
        "flex max-h-[var(--radix-popover-content-available-height)] max-w-[var(--radix-popover-content-available-width)] flex-col p-1.5",
        wide ? "w-[28rem]" : "w-[22rem]",
      )}
    >
      {actualSide === "top" && inlineDetail}
      <div className="px-2 py-1 text-2xs text-text-tertiary">{heading}</div>
      <div
        ref={listRef}
        id={id}
        className="min-h-0 max-h-64 overflow-y-auto"
        role="listbox"
        aria-label={heading}
      >
        {items.map((item, index) => {
          const hasTrailing = Boolean(item.subtitle || item.hint);
          return (
            <button
              key={item.key}
              type="button"
              role="option"
              id={`${id}-${index}`}
              aria-selected={index === safeActive}
              data-index={index}
              tabIndex={-1}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onPick(index)}
              onMouseMove={() => onHover(index)}
              className={cn(
                "flex h-[30px] w-full items-center gap-2 rounded-md px-2 text-left text-ui transition-colors",
                index === safeActive ? "bg-accent text-foreground" : "text-popover-foreground",
              )}
            >
              {item.icon && (
                <span className="flex size-4 shrink-0 items-center justify-center text-text-tertiary [&_svg]:size-3.5">
                  {item.icon}
                </span>
              )}
              {/* 有尾随参数/目录时，标题优先不缩并在 72% 处优雅截断，避免挤掉尾随信息或溢出容器；无尾随项时可占满整行。 */}
              <span
                data-slot="suggestion-title"
                className={cn("min-w-0 truncate", hasTrailing && "max-w-[72%] shrink-0")}
              >
                <Highlighted text={item.title} matches={item.titleMatches} />
              </span>
              {item.subtitle && (
                // rtl 让省略号出现在左侧，保留离文件最近的几级目录；bdi 保持路径本身从左到右
                <span dir="rtl" className="min-w-0 truncate text-left text-xs text-text-tertiary">
                  <bdi>
                    <Highlighted text={item.subtitle} matches={item.subtitleMatches} />
                  </bdi>
                </span>
              )}
              {item.hint && (
                <span className="ml-auto min-w-0 truncate font-mono text-2xs text-text-tertiary">
                  {item.hint}
                </span>
              )}
            </button>
          );
        })}
      </div>
      {actualSide === "bottom" && inlineDetail}

      {/* 独立侧边详情卡：与列表几何解耦，主列表高度不随详情跳变；靠输入框一侧与列表齐平。 */}
      <Popover open={showSideDetail}>
        {anchorRef && <PopoverAnchor virtualRef={anchorRef} />}
        {current && (
          <PopoverContent
            ref={detailRef}
            role="note"
            data-slot="suggestion-detail"
            data-placement="side"
            side="right"
            align={actualSide === "top" ? "end" : "start"}
            sideOffset={8}
            collisionPadding={12}
            onOpenAutoFocus={(event) => event.preventDefault()}
            onCloseAutoFocus={(event) => event.preventDefault()}
            onInteractOutside={(event) => event.preventDefault()}
            onEscapeKeyDown={(event) => event.preventDefault()}
            className="max-h-[min(22rem,var(--radix-popover-content-available-height))] w-72 max-w-[var(--radix-popover-content-available-width)] overflow-y-auto p-3"
          >
            <SuggestionDetailBody item={current} />
          </PopoverContent>
        )}
      </Popover>
    </PopoverContent>
  );
}
