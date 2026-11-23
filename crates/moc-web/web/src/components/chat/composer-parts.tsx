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
  ShieldAlertIcon,
  ShieldCheckIcon,
  ShieldIcon,
  TerminalIcon,
  XIcon,
} from "lucide-react";
import {
  type DragEvent,
  type ReactNode,
  type RefObject,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
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
import { PopoverContent } from "@/components/ui/popover";
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
import { AgentsPopover, UsagePopover } from "./status-popovers";

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
 * 托盘顶部 36px 状态条：左侧有任务时显示任务进度（可展开清单），右侧显示本会话改动；
 * 没有任务但有改动时左侧显示改动汇总、右侧「查看变更」；都没有时显示随机提示。
 * 后台子代理与会话费用始终排在最右，各自有值才出现。
 */
export function ComposerStatus({ sessionId, todos }: { sessionId: string; todos: TodoItem[] }) {
  const t = useT();
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
            <div className="flex min-w-0 items-center gap-1.5 px-1.5 text-muted-foreground">
              <PencilIcon className="size-3.5 shrink-0" />
              <span className="truncate text-foreground">
                {t("cp.files_changed", { n: changes.files })}
              </span>
              <DiffStat added={changes.added} removed={changes.removed} />
            </div>
          ) : (
            tip && (
              <p
                key={tip}
                className="min-w-0 truncate px-1.5 animate-in fade-in-0 duration-300"
                title={tip.replaceAll("`", "")}
              >
                <span className="mr-1 font-medium">{t("cp.tips")}</span>
                <TipText tip={tip} />
              </p>
            )
          )}
        </div>
        {todos.length === 0 && changes.files > 0 && (
          <button
            type="button"
            onClick={() => inspect("changes")}
            className="h-6 shrink-0 rounded-md px-1.5 text-link transition-colors hover:text-link/80"
          >
            {t("cp.view_changes")}
          </button>
        )}
        {todos.length > 0 && changes.files > 0 && (
          <button
            type="button"
            onClick={() => inspect("changes")}
            className={STAT}
            title={t("cp.changes")}
          >
            <span>{t("cp.files_n", { n: changes.files })}</span>
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

export function PlanToggle({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const t = useT();
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
              toast.error(t("slash.plan_failed", { error: errorText(error) })),
            )
          }
          className={cn(
            CHIP,
            active && "bg-brand/10 text-brand hover:bg-brand/15 hover:text-brand",
          )}
        >
          <ListChecksIcon className="size-3.5" />
          <span>{t("cp.plan")}</span>
        </button>
      </TooltipTrigger>
      <TooltipContent side="top">
        {active ? t("cp.plan_exit") : t("cp.plan_enter")} · Alt+J
      </TooltipContent>
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
  detail?: string;
  hint?: string;
  icon?: ReactNode;
};

export function SuggestionMenu({
  id,
  side,
  heading,
  items,
  active,
  onPick,
  onHover,
  inputRef,
}: {
  id: string;
  side: "bottom" | "top";
  heading: string;
  items: Suggestion[];
  active: number;
  onPick: (index: number) => void;
  onHover: (index: number) => void;
  inputRef: RefObject<HTMLTextAreaElement | null>;
}) {
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    listRef.current
      ?.querySelector(`[data-index="${active}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [active]);

  return (
    <PopoverContent
      side={side}
      align="start"
      sideOffset={4}
      alignOffset={8}
      collisionPadding={12}
      role="presentation"
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onInteractOutside={(event) => {
        if (event.target === inputRef.current) event.preventDefault();
      }}
      className="flex max-h-[var(--radix-popover-content-available-height)] w-[300px] max-w-[var(--radix-popover-content-available-width)] flex-col p-1.5"
    >
      <div className="px-2 py-1 text-2xs text-text-tertiary">{heading}</div>
      <div
        ref={listRef}
        id={id}
        className="min-h-0 max-h-64 overflow-y-auto"
        role="listbox"
        aria-label={heading}
      >
        {items.map((item, index) => (
          <button
            key={item.key}
            type="button"
            role="option"
            id={`${id}-${index}`}
            aria-selected={index === active}
            data-index={index}
            tabIndex={-1}
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => onPick(index)}
            onMouseMove={() => onHover(index)}
            className={cn(
              "flex h-[30px] w-full items-center gap-2 rounded-md px-2 text-left text-ui transition-colors",
              index === active ? "bg-accent text-foreground" : "text-popover-foreground",
            )}
          >
            {item.icon && (
              <span className="flex size-4 shrink-0 items-center justify-center text-text-tertiary [&_svg]:size-3.5">
                {item.icon}
              </span>
            )}
            <span className="min-w-0 truncate">{item.title}</span>
            {item.hint && (
              <span className="ml-auto min-w-0 truncate font-mono text-2xs text-text-tertiary">
                {item.hint}
              </span>
            )}
          </button>
        ))}
      </div>
      {items[active]?.detail && (
        <p
          className="mt-1 line-clamp-3 shrink-0 rounded-md bg-muted px-2 py-1.5 text-xs text-muted-foreground"
          title={items[active].detail}
        >
          {items[active].detail}
        </p>
      )}
    </PopoverContent>
  );
}
