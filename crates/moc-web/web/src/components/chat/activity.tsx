// 执行轨迹：思考、工具、命令逐行排列。每行先是一条标题（图标 · 动作 · 对象 · 尾注），
// 正文按显示档位（隐藏 / 简略 / 完整，见 lib/tool-view.ts）露出；点标题在「完整 ↔
// 当前档位的收起形态」之间切换。全局档位一变，行按新档位重建，手动展开随之收回。

import { ChevronRightIcon, CircleAlertIcon, Loader2Icon } from "lucide-react";
import { type ReactNode, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { MessageResponse } from "@/components/ai-elements/message";
import { NoticeScope, toolBody } from "@/components/chat/tool-views";
import { PlainLines } from "@/components/common/plain-lines";
import { DiffStat, diffStats } from "@/components/panels/diff-file";
import { Button } from "@/components/ui/button";
import { parseLinkTarget } from "@/lib/file-target";
import { clockTime, formatDuration } from "@/lib/format";
import { t, useT } from "@/lib/i18n";
import {
  BRIEF_BODY_ROWS,
  BRIEF_THINKING_ROWS,
  bodyPolicy,
  type DetailLevel,
  findFiles,
  grepCounts,
  groupActivity,
  headLines,
  parseArgs,
  preferBuiltinRedirect,
  toggledLevel,
  toolDisplayName,
  toolExpandable,
  toolFailed,
  toolHeadline,
} from "@/lib/tool-view";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import { activitySnippet } from "@/lib/turn";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type { ImageSource, JobInfo, JobResult } from "@/protocol/types";
import { reanswerAsk } from "@/store/actions";
import { openFile } from "@/store/files";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";

export function imageSrc(source: ImageSource): string {
  return "Url" in source
    ? source.Url
    : `data:${source.Base64.media_type};base64,${source.Base64.data}`;
}

/** 尾注里的耗时：与 TUI `labels::duration` 一致，不足 1 秒按毫秒展示。 */
function Duration({ ms }: { ms?: number }) {
  if (ms === undefined) return null;
  return <span className="text-2xs tabular-nums text-text-tertiary">{formatDuration(ms)}</span>;
}

function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [active]);
  return now;
}

/**
 * 折叠钉住的标题时把它滚回视口顶端：否则正文收起后标题的原位远在视口之上，视口落到无关内容。
 * 标题是父容器首个子元素，比父容器顶缘低即为钉住；scroll-margin 让出外层钉住标题的高度。
 */
export function toggleKeepingHeader(
  header: HTMLElement | null,
  collapsing: boolean,
  toggle: () => void,
) {
  const parent = header?.parentElement;
  const stuck =
    collapsing &&
    header != null &&
    parent != null &&
    header.getBoundingClientRect().top - parent.getBoundingClientRect().top > 0.5;
  toggle();
  if (stuck) requestAnimationFrame(() => header.scrollIntoView({ block: "start" }));
}

/** 标题行：图标 · 动作 · 对象 · 次级范围 · 尾注；可展开时整行可点。 */
function RowHeader({
  verb,
  target,
  targetTitle,
  scope,
  failed,
  warn,
  emphasized,
  suffix,
  action,
  interactive,
  open,
  pulse,
  sticky,
  inlineSuffix,
  atMs,
  onToggle,
  onTargetClick,
}: {
  /** 顶层活动的发生时刻：悬停时显示在正文列左侧槽。 */
  atMs?: number;
  verb: string;
  target?: string;
  /** 目标被缩写显示时（如文件只留文件名），悬停提示里的完整文本。 */
  targetTitle?: string;
  onTargetClick?: () => void;
  scope?: string;
  failed?: boolean;
  warn?: boolean;
  emphasized?: boolean;
  suffix?: ReactNode;
  action?: ReactNode;
  interactive: boolean;
  open: boolean;
  pulse?: boolean;
  /** 长块正文滚过顶缘后把标题钉在滚动容器顶部，随时可点回折叠；块尾处被下一块顶走。 */
  sticky?: boolean;
  /** 工具尾注紧跟摘要；其他活动保留右侧尾注。 */
  inlineSuffix?: boolean;
  onToggle: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  return (
    <div
      ref={ref}
      className={cn(
        // 参照 Devin：整行同一档 tertiary 灰、26px 行距，悬停整行提到 muted；「过程」标题保持 muted，高出一级。
        "group/row flex h-6.5 w-full min-w-0 items-center gap-1.5 rounded-md text-left text-ui text-text-tertiary transition-colors",
        interactive && "hover:text-muted-foreground",
        // 钉住时正文从行底滚过，需要不透明背景与层级；嵌套行不钉，避免多层叠头。
        // 位于已展开的「过程」内时，--moc-sticky-top 让出过程标题的高度，两级标题叠放不互相遮挡。
        sticky
          ? "sticky top-[var(--moc-sticky-top,0px)] z-10 scroll-mt-[var(--moc-sticky-top,0px)] bg-background"
          : "relative",
      )}
    >
      <button
        type="button"
        disabled={!interactive}
        aria-label={target ? `${verb} ${target}` : verb}
        aria-expanded={interactive ? open : undefined}
        title={onTargetClick ? undefined : target}
        onClick={() => toggleKeepingHeader(ref.current, Boolean(sticky) && open, onToggle)}
        className="absolute inset-0 rounded-md"
      />
      {/* 参照 Devin：时间落在正文列左外侧的固定槽，--moc-row-indent 抵消「过程」缩进让各级对齐；
          列外留白不足（主舞台 <56rem）时不显示，免得被裁切。 */}
      {atMs !== undefined && (
        <span className="pointer-events-none absolute right-full mr-[calc(var(--moc-row-indent,0px)+0.75rem)] hidden whitespace-nowrap text-2xs tabular-nums text-text-tertiary opacity-0 transition-opacity group-hover/row:opacity-100 @min-[56rem]:block">
          {clockTime(atMs)}
        </span>
      )}
      {/* 参照 Devin 活动日志：常态只留文字。失败只把动词染成 destructive，告警图标放到尾注前，
          行首不插图标，整列动词左缘保持对齐。 */}
      <span
        className={cn(
          "shrink-0",
          emphasized && "font-medium text-foreground",
          failed && "text-destructive",
          warn && !failed && "text-warn",
          pulse && "animate-pulse",
        )}
      >
        {verb}
      </span>
      {(target || scope || !inlineSuffix) && (
        <span
          className={cn("flex min-w-0 items-baseline gap-2 truncate", !inlineSuffix && "flex-1")}
        >
          {target &&
            (onTargetClick ? (
              <button
                type="button"
                className={cn("relative min-w-0 truncate text-left hover:text-foreground")}
                title={targetTitle ?? target}
                onClick={onTargetClick}
              >
                {target}
              </button>
            ) : (
              <span
                className={cn("min-w-0 truncate", emphasized && "text-muted-foreground")}
                title={targetTitle ?? target}
              >
                {target}
              </span>
            ))}
          {scope && (
            <span className="shrink-0 truncate text-xs text-text-tertiary/70">{scope}</span>
          )}
        </span>
      )}
      {!inlineSuffix && action && <span className="relative shrink-0">{action}</span>}
      <span className="flex shrink-0 items-center gap-1.5">
        {failed && <CircleAlertIcon className="size-3.5 text-destructive" />}
        {suffix}
        {interactive && (
          <ChevronRightIcon
            className={cn(
              "size-3.5 text-text-tertiary opacity-0 transition-all group-hover/row:opacity-100",
              open && "rotate-90",
            )}
          />
        )}
      </span>
      {inlineSuffix && action && <span className="relative shrink-0">{action}</span>}
    </div>
  );
}

function Body({ children }: { children: ReactNode }) {
  return <div className="space-y-1.5 pt-0.5 pb-2">{children}</div>;
}

// ---------- 思考 ----------

/**
 * 简略档流式中的思考：整段 Markdown 照常渲染（不截断源文本，免得切坏语法），
 * 视口只留末尾 {@link BRIEF_THINKING_ROWS} 行并贴底；内容溢出时顶部渐隐。
 */
function ThinkingTail({ text }: { text: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [clipped, setClipped] = useState(false);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 流式正文每次增长都要重新贴底
  useLayoutEffect(() => {
    const node = ref.current;
    if (!node) return;
    node.scrollTop = node.scrollHeight;
    setClipped(node.scrollHeight > node.clientHeight + 1);
  }, [text]);
  return (
    <div
      ref={ref}
      className={cn(
        "overflow-hidden",
        clipped && "[mask-image:linear-gradient(to_bottom,transparent,black_1.75rem)]",
      )}
      style={{ maxHeight: `calc(${BRIEF_THINKING_ROWS} * 1.5 * 13.5px)` }}
    >
      <ThinkingProse text={text} animating />
    </div>
  );
}

function ThinkingProse({ text, animating }: { text: string; animating?: boolean }) {
  return (
    <div className="[--moc-prose-color:var(--muted-foreground)] [--moc-prose-leading:1.5] [--moc-prose-size:13.5px]">
      <MessageResponse isAnimating={animating}>{text}</MessageResponse>
    </div>
  );
}

function ReasoningRow({
  block,
  axis,
  live,
}: {
  block: Extract<Block, { kind: "reasoning" }>;
  axis: DetailLevel;
  live: boolean;
}) {
  const t = useT();
  const [level, setLevel] = useState(axis);
  const streaming = live && block.durationMs === undefined;
  const now = useNow(streaming);
  const hasText = block.text.trim().length > 0;
  const elapsedMs =
    streaming && block.startedAt !== undefined ? Math.max(0, now - block.startedAt) : undefined;
  const verb = streaming
    ? t("verb.thinking_ing")
    : block.durationMs !== undefined
      ? t("act.thought_for", { n: formatDuration(block.durationMs) })
      : t("verb.thinking");
  const tokensText =
    !streaming && block.tokens && block.tokens > 0
      ? t(block.tokens === 1 ? "act.tokens_one" : "act.tokens_many", {
          n: block.tokens.toLocaleString(),
        })
      : undefined;
  // 完整档展开全文；简略档只在流式中露出末尾，结束后收成标题行；隐藏档只有标题。
  const body = !hasText ? null : level === "full" ? (
    <ThinkingProse text={block.text} animating={streaming} />
  ) : level === "brief" && streaming ? (
    <ThinkingTail text={block.text} />
  ) : null;
  return (
    <div>
      <RowHeader
        atMs={block.atMs}
        verb={verb}
        target={body || level === "hidden" ? undefined : activitySnippet(block, 160)}
        pulse={streaming}
        interactive={hasText}
        open={level === "full"}
        sticky={hasText}
        inlineSuffix
        onToggle={() => setLevel(toggledLevel(axis, level))}
        suffix={
          streaming ? (
            <Duration ms={elapsedMs} />
          ) : tokensText ? (
            <span className="text-2xs tabular-nums text-text-tertiary">· {tokensText}</span>
          ) : undefined
        }
      />
      {body && <div className="ml-0.5 border-l border-border-subtle pt-0.5 pb-2 pl-3">{body}</div>}
    </div>
  );
}

// ---------- 工具 ----------

type DiffTotal = { added: number; removed: number };

/** 把工具块自身 diff（不含子调用）的行数累加进 total。 */
function addDiffTotals(tools: readonly ToolBlock[], total: DiffTotal): DiffTotal {
  for (const tool of tools) {
    for (const file of tool.diff?.files ?? []) {
      const stats = diffStats(file);
      total.added += stats.added;
      total.removed += stats.removed;
    }
  }
  return total;
}

/** 本轮全部工具（含子调用）的改动行数合计：过程标题的 +N −M。 */
export function turnDiffTotal(blocks: readonly Block[]): DiffTotal {
  const total = { added: 0, removed: 0 };
  const visit = (tools: readonly ToolBlock[]) => {
    addDiffTotals(tools, total);
    for (const tool of tools) visit(tool.children);
  };
  visit(blocks.filter((block): block is ToolBlock => block.kind === "tool"));
  return total;
}

function toolSuffix(
  tool: ToolBlock,
  stat?: string,
  statError?: boolean,
  statWarn?: boolean,
  now = Date.now(),
): ReactNode {
  if (tool.preview) {
    return (
      <span className="text-2xs text-text-tertiary">
        {tool.status === "queued" ? t("act.queued") : t("act.generating")}
      </span>
    );
  }
  const bgBadge =
    tool.job &&
    tool.job.origin !== "foreground" &&
    (tool.job.status === "running" || tool.job.status === "killing") ? (
      <span className="font-mono text-2xs text-text-tertiary">
        {t("act.bg_job", { id: tool.job.job_id })}
      </span>
    ) : null;
  if (tool.status === "running") {
    const elapsed = tool.job?.started_at_ms ? Math.max(0, now - tool.job.started_at_ms) : undefined;
    return (
      <>
        {bgBadge}
        {tool.awaitingPermission ? (
          <span className="text-2xs text-warn">{t("act.wait_auth")}</span>
        ) : (
          <Loader2Icon className="size-3 animate-spin text-text-tertiary" />
        )}
        <Duration ms={elapsed} />
      </>
    );
  }
  if (tool.status === "denied")
    return <span className="text-2xs text-warn">{t("act.denied")}</span>;
  if (tool.status === "cancelled")
    return <span className="text-2xs text-text-tertiary">{t("act.cancelled")}</span>;
  const total = addDiffTotals([tool], { added: 0, removed: 0 });
  return (
    <>
      {bgBadge}
      {stat && (
        <span
          className={cn(
            "text-2xs tabular-nums",
            statError ? "text-destructive" : statWarn ? "text-warn" : "text-text-tertiary",
          )}
        >
          {stat}
        </span>
      )}
      {(total.added > 0 || total.removed > 0) && <DiffStat {...total} />}
      {tool.diff?.truncated && <span className="text-2xs text-text-tertiary">…</span>}
      <Duration ms={tool.durationMs} />
    </>
  );
}

function ToolRow({
  tool,
  axis,
  sessionId,
  sticky,
}: {
  tool: ToolBlock;
  axis: DetailLevel;
  sessionId: string;
  /** 仅顶层行钉住标题；嵌套子工具与合并组成员由各自的组头承载折叠入口。 */
  sticky?: boolean;
}) {
  const t = useT();
  const [level, setLevel] = useState(axis);
  const now = useNow(tool.status === "running" && Boolean(tool.job));
  const args = parseArgs(tool.arguments);
  const headline = toolHeadline(tool);
  const redirect = preferBuiltinRedirect(tool);
  const failed = toolFailed(tool) && !redirect;
  const interactive =
    toolExpandable(tool) &&
    Boolean(
      tool.preview ||
        tool.result ||
        tool.liveOutput ||
        tool.diff ||
        tool.arguments.trim() ||
        tool.progress?.length,
    );
  const toggle = () => setLevel(toggledLevel(axis, level));
  const body = toolBody(tool, level, () => setLevel("full"));
  // read / edit / write 的目标路径可点击：在文件分页打开（read 带 offset 行号）。
  const targetClick = (() => {
    if (tool.preview || (tool.name !== "read" && tool.name !== "edit" && tool.name !== "write"))
      return undefined;
    const target = headline.target;
    if (!target) return undefined;
    return () => {
      const cwd = useTranscript.getState().bySession[sessionId]?.info.cwd ?? "";
      const parsed = parseLinkTarget(target, cwd);
      const offset = tool.name === "read" ? argOffset(tool) : undefined;
      if (parsed) openFile(sessionId, parsed.target, { line: parsed.line ?? offset });
    };
  })();
  // wait_agent / sidekick 的进度行已由 toolBody 渲染，避免在 ToolExtras 中重复展示。
  const showExtraProgress =
    tool.name !== "wait_agent" && tool.name !== "sidekick" && Boolean(tool.progress?.length);
  const extras =
    level !== "hidden" &&
    (level === "full" || bodyPolicy(tool.name) !== "summary" || tool.name === "web_fetch") &&
    (tool.images.length > 0 || showExtraProgress);
  const canBackground = tool.job?.origin === "foreground" && tool.job.status === "running";
  const background = () => {
    if (!tool.job) return;
    void rpc
      .call<JobResult>("job/background", { session_id: sessionId, job_id: tool.job.job_id })
      .catch((error) =>
        toast.error(
          t("act.bg_failed", {
            error: error instanceof Error ? error.message : String(error),
          }),
        ),
      );
  };
  // 已收场的提问可以重答：core 截回未回答状态后按原参数重放提问卡片。
  const canReask = tool.name === "ask" && !tool.preview && tool.status !== "running";
  const [reasking, setReasking] = useState(false);
  const reask = () => {
    if (reasking) return;
    setReasking(true);
    void reanswerAsk(sessionId, tool.toolCallId).finally(() => setReasking(false));
  };
  const headerAction = canBackground ? (
    <Button
      type="button"
      variant="ghost"
      size="xs"
      className="h-auto rounded px-1 text-2xs text-brand hover:bg-brand/10 hover:text-brand"
      onClick={(event) => {
        event.stopPropagation();
        background();
      }}
    >
      {t("act.to_bg")}
    </Button>
  ) : canReask ? (
    <Button
      type="button"
      variant="ghost"
      size="xs"
      disabled={reasking}
      className="h-auto rounded px-1 text-2xs text-brand hover:bg-brand/10 hover:text-brand"
      onClick={(event) => {
        event.stopPropagation();
        reask();
      }}
    >
      {t("act.reask")}
    </Button>
  ) : undefined;
  return (
    <div>
      <RowHeader
        atMs={sticky ? tool.atMs : undefined}
        verb={headline.verb ?? toolDisplayName(tool.name, args)}
        // 可点击的文件目标只显示文件名（含行范围），与合并组一致；完整路径留给悬停提示，点击仍按完整路径打开。
        target={
          targetClick
            ? headline.target
                .replace(/[\\/]+$/, "")
                .split(/[\\/]/)
                .pop() || headline.target
            : headline.target
        }
        targetTitle={targetClick ? headline.target : undefined}
        scope={headline.scope}
        failed={failed}
        warn={redirect}
        pulse={tool.status === "generating"}
        suffix={toolSuffix(tool, headline.stat, headline.statError, headline.statWarn, now)}
        inlineSuffix
        action={headerAction}
        interactive={interactive}
        open={level === "full"}
        sticky={sticky && interactive}
        onToggle={toggle}
        onTargetClick={targetClick}
      />
      {(body || extras) && (
        <Body>
          {body}
          {extras && <ToolExtras tool={tool} showProgress={showExtraProgress} />}
        </Body>
      )}
      {tool.children.length > 0 && (
        <div className="ml-0.5 border-l border-border-subtle pl-3">
          <NoticeScope result={tool.result}>
            {tool.children.map((child) => (
              <ToolRow key={`${child.id}:${axis}`} tool={child} axis={axis} sessionId={sessionId} />
            ))}
          </NoticeScope>
        </div>
      )}
    </div>
  );
}

/** read 的 offset 起始行（入参 JSON 里的数字）。 */
function argOffset(tool: ToolBlock): number | undefined {
  try {
    const offset = (JSON.parse(tool.arguments) as { offset?: number }).offset;
    return typeof offset === "number" && offset > 0 ? offset : undefined;
  } catch {
    return undefined;
  }
}

/** 子代理进度行与工具返回的图片。 */
function ToolExtras({ tool, showProgress }: { tool: ToolBlock; showProgress: boolean }) {
  const t = useT();
  return (
    <>
      {showProgress && tool.progress && tool.progress.length > 0 && (
        <div className="space-y-1 rounded-lg bg-code px-3 py-2 text-xs">
          {tool.progress.map((row) => (
            <div key={row.agent_id} className="flex items-center gap-2">
              {row.status === "running" || row.status === "queued" ? (
                <Loader2Icon className="size-3 animate-spin text-brand" />
              ) : (
                <span
                  className={cn(
                    "size-1.5 rounded-full",
                    row.status === "failed" ? "bg-destructive" : "bg-success",
                  )}
                />
              )}
              <span className="min-w-0 flex-1 truncate">{row.title}</span>
              {row.activity && <span className="truncate text-text-tertiary">{row.activity}</span>}
              <span className="font-mono text-2xs text-text-tertiary">
                {formatDuration(row.elapsed_ms)}
              </span>
            </div>
          ))}
        </div>
      )}
      {tool.images.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {tool.images.map((image, index) => (
            <a
              // biome-ignore lint/suspicious/noArrayIndexKey: 工具结果的图片列表只读不重排，且没有稳定 id
              key={index}
              href={imageSrc(image)}
              target="_blank"
              rel="noreferrer"
              className="overflow-hidden rounded-lg bg-code"
            >
              <img
                src={imageSrc(image)}
                alt={t("act.tool_image")}
                className="max-h-48 max-w-[240px] object-contain"
              />
            </a>
          ))}
        </div>
      )}
    </>
  );
}

/** 合并行尾注：同类计数求和。 */
function groupStat(name: string, tools: ToolBlock[]): string | undefined {
  const done = tools.filter((tool) => tool.result !== undefined && !toolFailed(tool));
  if (done.length === 0) return undefined;
  if (name === "grep") {
    const total = done.reduce(
      (sum, tool) => {
        const counts = grepCounts(tool.result as string);
        return { matches: sum.matches + counts.matches, files: sum.files + counts.files };
      },
      { matches: 0, files: 0 },
    );
    return `${total.matches} matches · ${total.files} files`;
  }
  if (name === "find") {
    const files = done.reduce(
      (sum, tool) => sum + findFiles(tool.result as string).paths.length,
      0,
    );
    return `${files} files`;
  }
  return undefined;
}

function ToolGroupRow({
  name,
  tools,
  axis,
  sessionId,
}: {
  name: string;
  tools: ToolBlock[];
  axis: DetailLevel;
  sessionId: string;
}) {
  const t = useT();
  const failedCount = tools.filter(toolFailed).length;
  const [open, setOpen] = useState(axis === "full" || failedCount > 0);
  const running = tools.some((tool) => tool.status === "running" || Boolean(tool.preview));
  const targets = tools
    .map((tool) => toolHeadline(tool).target.split(/[\\/]/).pop() ?? "")
    .filter(Boolean)
    .join("  ");
  const verb = t("grp.times", { verb: toolDisplayName(name), n: tools.length });
  const stat = groupStat(name, tools);
  return (
    <div>
      <RowHeader
        atMs={tools[0]?.atMs}
        verb={verb}
        target={targets}
        failed={failedCount > 0}
        interactive
        open={open}
        sticky
        inlineSuffix
        onToggle={() => setOpen(!open)}
        suffix={
          running ? (
            <Loader2Icon className="size-3 animate-spin text-text-tertiary" />
          ) : (
            <>
              {failedCount > 0 && (
                <span className="text-2xs text-destructive">
                  {t("grp.failed", { n: failedCount })}
                </span>
              )}
              {stat && <span className="text-2xs tabular-nums text-text-tertiary">{stat}</span>}
            </>
          )
        }
      />
      {open && (
        <div className="ml-0.5 border-l border-border-subtle pl-3">
          {tools.map((tool) => (
            <ToolRow key={`${tool.id}:${axis}`} tool={tool} axis={axis} sessionId={sessionId} />
          ))}
        </div>
      )}
    </div>
  );
}

// ---------- 其他块 ----------

/** `!cmd` 手动命令与 bash 工具同一套呈现。 */
function bashAsTool(block: Extract<Block, { kind: "bash" }>): ToolBlock {
  return {
    kind: "tool",
    id: block.id,
    toolCallId: block.toolCallId ?? block.id,
    name: "bash",
    arguments: JSON.stringify({
      command: block.command,
      exclude_from_context: Boolean(block.excludeFromContext),
    }),
    status:
      block.output === undefined && !block.job?.status?.includes("exited")
        ? "running"
        : block.isError
          ? "error"
          : "completed",
    result: block.output ?? block.liveOutput,
    isError: Boolean(block.isError),
    images: [],
    children: [],
    awaitingPermission: false,
    job: block.job,
    liveOutput: block.liveOutput,
    durationMs: block.durationMs,
    atMs: block.atMs,
  };
}

/** 结果卡片独立于处理过程；全局工具档位改变时收回手动展开。 */
export function AgentResultCard({ block }: { block: Extract<Block, { kind: "agent-result" }> }) {
  const axis = usePrefs((state) => state.toolDetail);
  return <AgentReportRow key={`${block.id}:${axis}`} block={block} axis={axis} />;
}

/** 任务通知的状态尾注：停滞优先（通知发出时任务仍在运行）。 */
function jobNoticeStatus(job: JobInfo): string {
  if (job.stalled) return t("act.job_stalled");
  switch (job.status) {
    case "exited":
      return job.exit_code === null || job.exit_code === undefined
        ? t("act.job_finished")
        : t("act.job_exit", { code: job.exit_code });
    case "killed":
      return t("act.job_killed");
    case "timed_out":
      return t("act.job_timed_out");
    default:
      return t("act.job_running");
  }
}

/** 任务通知卡片独立于处理过程；全局工具档位改变时收回手动展开。 */
export function JobNoticeCard({ block }: { block: Extract<Block, { kind: "job-notice" }> }) {
  const axis = usePrefs((state) => state.toolDetail);
  return <JobNoticeRow key={`${block.id}:${axis}`} block={block} axis={axis} />;
}

function JobNoticeRow({
  block,
  axis,
}: {
  block: Extract<Block, { kind: "job-notice" }>;
  axis: DetailLevel;
}) {
  const t = useT();
  const [level, setLevel] = useState(axis);
  const head = headLines(block.body, BRIEF_BODY_ROWS);
  const lines = level === "full" ? block.body : head.lines.join("\n");
  const job = block.job;
  const failed =
    job.status === "killed" ||
    job.status === "timed_out" ||
    (job.status === "exited" && job.exit_code !== 0);
  return (
    <div>
      <RowHeader
        atMs={block.atMs}
        verb={t("act.job_notice")}
        target={job.job_id}
        failed={failed}
        emphasized
        suffix={
          <>
            <span className="text-xs">{jobNoticeStatus(job)}</span>
            <Duration ms={job.duration_ms ?? undefined} />
          </>
        }
        interactive
        open={level === "full"}
        sticky
        onToggle={() => setLevel(toggledLevel(axis, level))}
      />
      {level !== "hidden" && (
        <Body>
          <div className="rounded-lg bg-code px-3.5 py-2.5 text-sm text-muted-foreground">
            <PlainLines text={lines} />
            {level === "brief" && head.hidden > 0 && (
              <button
                type="button"
                onClick={() => setLevel("full")}
                className="mt-1 block text-2xs text-text-tertiary hover:text-foreground"
              >
                {t("act.more_lines", { n: head.hidden })}
              </button>
            )}
          </div>
        </Body>
      )}
    </div>
  );
}

function AgentReportRow({
  block,
  axis,
}: {
  block: Extract<Block, { kind: "agent-message" | "agent-result" }>;
  axis: DetailLevel;
}) {
  const t = useT();
  const [level, setLevel] = useState(axis);
  const head = headLines(block.body, BRIEF_BODY_ROWS);
  const lines = level === "full" ? block.body : head.lines.join("\n");
  const result = block.kind === "agent-result";
  const delivery = result ? undefined : block.delivery;
  return (
    <div>
      <RowHeader
        atMs={block.atMs}
        verb={result ? t("act.agent_result") : t("act.irc")}
        target={result ? block.meta.title : `◀ ${block.from}`}
        failed={result ? block.meta.status === "failed" : delivery === "failed"}
        emphasized={result}
        inlineSuffix={!result}
        suffix={
          result ? (
            <span className="text-xs">{block.meta.status}</span>
          ) : delivery ? (
            <span
              className={cn(
                "text-2xs",
                delivery === "failed"
                  ? "text-destructive"
                  : delivery === "injected"
                    ? "text-brand"
                    : "text-text-tertiary",
              )}
            >
              {delivery}
            </span>
          ) : undefined
        }
        interactive
        open={level === "full"}
        sticky
        onToggle={() => setLevel(toggledLevel(axis, level))}
      />
      {level !== "hidden" && (
        <Body>
          <div
            className={cn(
              "rounded-lg bg-code px-3.5 py-2.5 text-sm",
              result ? "text-foreground" : "text-muted-foreground",
            )}
          >
            {result ? (
              <MessageResponse mode="static">{lines}</MessageResponse>
            ) : (
              <PlainLines text={lines} />
            )}
            {level === "brief" && head.hidden > 0 && (
              <button
                type="button"
                onClick={() => setLevel("full")}
                className="mt-1 block text-2xs text-text-tertiary hover:text-foreground"
              >
                {t("act.more_lines", { n: head.hidden })}
              </button>
            )}
          </div>
        </Body>
      )}
    </div>
  );
}

function GoalRoundRow({
  block,
  axis,
}: {
  block: Extract<Block, { kind: "goal-round" }>;
  axis: DetailLevel;
}) {
  const t = useT();
  const [level, setLevel] = useState(axis);
  const head = headLines(block.body, BRIEF_BODY_ROWS);
  const lines = level === "full" ? block.body : head.lines.join("\n");
  return (
    <div>
      <RowHeader
        atMs={block.atMs}
        verb={t("act.goal_round", { n: block.round })}
        inlineSuffix
        suffix={<span className="text-2xs text-text-tertiary">{t("act.goal_auto")}</span>}
        interactive
        open={level === "full"}
        sticky
        onToggle={() => setLevel(toggledLevel(axis, level))}
      />
      {level !== "hidden" && (
        <Body>
          <div className="rounded-lg bg-code px-3.5 py-2.5 text-sm text-muted-foreground">
            <PlainLines text={lines} />
            {level === "brief" && head.hidden > 0 && (
              <button
                type="button"
                onClick={() => setLevel("full")}
                className="mt-1 block text-2xs text-text-tertiary hover:text-foreground"
              >
                {t("act.more_lines", { n: head.hidden })}
              </button>
            )}
          </div>
        </Body>
      )}
    </div>
  );
}

/** 一组连续的活动行。行的 key 带上所属层的档位：档位一变就按新档位重建，收回手动展开。 */
export function ActivityGroup({
  blocks,
  live,
  sessionId,
}: {
  blocks: Block[];
  live: boolean;
  sessionId: string;
}) {
  const t = useT();
  const thinking = usePrefs((state) => state.thinkingDetail);
  const tools = usePrefs((state) => state.toolDetail);
  const items = useMemo(() => groupActivity(blocks), [blocks]);
  return (
    <div>
      {items.map((item) => {
        if (item.kind === "group") {
          return (
            <ToolGroupRow
              key={`${item.id}:${tools}`}
              name={item.name}
              tools={item.tools}
              axis={tools}
              sessionId={sessionId}
            />
          );
        }
        const block = item.block;
        switch (block.kind) {
          case "reasoning":
            return (
              <ReasoningRow
                key={`${block.id}:${thinking}`}
                block={block}
                axis={thinking}
                live={live}
              />
            );
          case "tool":
            return (
              <ToolRow
                key={`${block.id}:${tools}`}
                tool={block}
                axis={tools}
                sessionId={sessionId}
                sticky
              />
            );
          case "bash":
            return (
              <ToolRow
                key={`${block.id}:${tools}`}
                tool={bashAsTool(block)}
                axis={tools}
                sessionId={sessionId}
                sticky
              />
            );
          case "agent-message":
            return <AgentReportRow key={`${block.id}:${tools}`} block={block} axis={tools} />;
          case "goal-round":
            return <GoalRoundRow key={`${block.id}:${tools}`} block={block} axis={tools} />;
          case "status":
            return (
              <RowHeader
                key={block.id}
                verb={t("verb.status")}
                target={block.text}
                interactive={false}
                open={false}
                onToggle={() => {}}
              />
            );
          default:
            return null;
        }
      })}
    </div>
  );
}
