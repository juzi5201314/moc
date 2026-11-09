// 执行轨迹：思考、工具、命令逐行排列。每行先是一条标题（图标 · 动作 · 对象 · 尾注），
// 正文按显示档位（隐藏 / 简略 / 完整，见 lib/tool-view.ts）露出；点标题在「完整 ↔
// 当前档位的收起形态」之间切换。全局档位一变，行按新档位重建，手动展开随之收回。

import {
  BotIcon,
  BrainIcon,
  ChevronRightIcon,
  CircleAlertIcon,
  CodeIcon,
  FileTextIcon,
  GlobeIcon,
  ImageIcon,
  InfoIcon,
  ListChecksIcon,
  Loader2Icon,
  MessageCircleQuestionIcon,
  PencilLineIcon,
  SearchIcon,
  SquareTerminalIcon,
  WrenchIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { MessageResponse } from "@/components/ai-elements/message";
import { toolBody } from "@/components/chat/tool-views";
import { DiffStat, diffStats } from "@/components/panels/diff-file";
import { Button } from "@/components/ui/button";
import { parseLinkTarget } from "@/lib/file-target";
import { formatDuration } from "@/lib/format";
import {
  BRIEF_BODY_ROWS,
  BRIEF_THINKING_ROWS,
  type DetailLevel,
  findFiles,
  grepCounts,
  groupActivity,
  headLines,
  parseSearch,
  toggledLevel,
  toolExpandable,
  toolFailed,
  toolHeadline,
} from "@/lib/tool-view";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import { type ActivityKind, activityKind, activitySnippet, activityVerb } from "@/lib/turn";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type { ImageSource, JobResult } from "@/protocol/types";
import { reanswerAsk } from "@/store/actions";
import { openFile } from "@/store/files";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";

const KIND_ICON: Record<ActivityKind, ReactNode> = {
  thinking: <BrainIcon />,
  commands: <SquareTerminalIcon />,
  code: <CodeIcon />,
  read: <FileTextIcon />,
  readImage: <ImageIcon />,
  write: <PencilLineIcon />,
  edit: <PencilLineIcon />,
  search: <SearchIcon />,
  webSearch: <GlobeIcon />,
  webFetch: <GlobeIcon />,
  subagents: <BotIcon />,
  plan: <ListChecksIcon />,
  questions: <MessageCircleQuestionIcon />,
  tools: <WrenchIcon />,
  showcase: <WrenchIcon />,
};

function iconFor(block: Block): ReactNode {
  const kind = activityKind(block);
  return kind ? KIND_ICON[kind] : <WrenchIcon />;
}

export function imageSrc(source: ImageSource): string {
  return "Url" in source
    ? source.Url
    : `data:${source.Base64.media_type};base64,${source.Base64.data}`;
}

/** 尾注里的耗时：不足 1 秒的不显示。 */
function Duration({ ms }: { ms?: number }) {
  if (ms === undefined || ms < 1000) return null;
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

/** 标题行：图标 · 动作 · 对象 · 次级范围 · 尾注；可展开时整行可点。 */
function RowHeader({
  icon,
  verb,
  target,
  scope,
  mono,
  failed,
  suffix,
  action,
  interactive,
  open,
  pulse,
  sticky,
  onToggle,
  onTargetClick,
}: {
  icon: ReactNode;
  verb: string;
  target?: string;
  onTargetClick?: () => void;
  scope?: string;
  mono?: boolean;
  failed?: boolean;
  suffix?: ReactNode;
  action?: ReactNode;
  interactive: boolean;
  open: boolean;
  pulse?: boolean;
  /** 长块正文滚过顶缘后把标题钉在滚动容器顶部，随时可点回折叠；块尾处被下一块顶走。 */
  sticky?: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      disabled={!interactive}
      onClick={onToggle}
      className={cn(
        "group/row flex h-7 w-full min-w-0 items-center gap-2 rounded-md px-1.5 text-left text-ui text-muted-foreground transition-colors",
        interactive ? "hover:text-foreground" : "cursor-default",
        // 钉住时正文从行底滚过，需要不透明背景与层级；嵌套行不钉，避免多层叠头。
        sticky && "sticky top-0 z-10 bg-background",
      )}
    >
      <span
        className={cn(
          "flex size-4 shrink-0 items-center justify-center [&_svg]:size-3.5",
          failed ? "text-destructive" : "text-text-tertiary",
        )}
      >
        {failed ? <CircleAlertIcon /> : icon}
      </span>
      <span className={cn("shrink-0", pulse && "animate-pulse")}>{verb}</span>
      <span className="flex min-w-0 flex-1 items-baseline gap-2 truncate">
        {target &&
          (onTargetClick ? (
            // biome-ignore lint/a11y/useSemanticElements: 行内截断 span 才能嵌进 title 截断布局，语义等价于 <a>
            <span
              role="link"
              tabIndex={0}
              className={cn(
                "min-w-0 truncate text-text-tertiary hover:text-foreground",
                mono && "font-mono text-xs",
              )}
              title={target}
              onClick={(event) => {
                event.stopPropagation();
                onTargetClick();
              }}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.stopPropagation();
                  onTargetClick();
                }
              }}
            >
              {target}
            </span>
          ) : (
            <span
              className={cn("min-w-0 truncate text-text-tertiary", mono && "font-mono text-xs")}
              title={target}
            >
              {target}
            </span>
          ))}
        {scope && (
          <span className="shrink-0 truncate font-mono text-2xs text-text-tertiary/70">
            {scope}
          </span>
        )}
      </span>
      {action}
      <span className="flex shrink-0 items-center gap-1.5">
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
    </button>
  );
}

function Body({ children }: { children: ReactNode }) {
  return <div className="space-y-1.5 pt-0.5 pb-2 pl-7">{children}</div>;
}

// ---------- 思考 ----------

/** 每秒 token 数：与 TUI 的 `thinking_tokens_per_second` 同一套四舍五入。 */
function tokenRate(tokens: number, ms: number): number {
  return Math.floor((tokens * 1000 + Math.floor(ms / 2)) / ms);
}

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
      style={{ maxHeight: `calc(${BRIEF_THINKING_ROWS} * 1.65 * 13.5px)` }}
    >
      <ThinkingProse text={text} animating />
    </div>
  );
}

function ThinkingProse({ text, animating }: { text: string; animating?: boolean }) {
  return (
    <div className="[--moc-prose-color:var(--muted-foreground)] [--moc-prose-leading:1.65] [--moc-prose-size:13.5px]">
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
  const [level, setLevel] = useState(axis);
  const streaming = live && block.durationMs === undefined;
  const hasText = block.text.trim().length > 0;
  const rate =
    block.tokens && block.durationMs ? tokenRate(block.tokens, block.durationMs) : undefined;
  // 完整档展开全文；简略档只在流式中露出末尾，结束后收成标题行；隐藏档只有标题。
  const body = !hasText ? null : level === "full" ? (
    <ThinkingProse text={block.text} animating={streaming} />
  ) : level === "brief" && streaming ? (
    <ThinkingTail text={block.text} />
  ) : null;
  return (
    <div>
      <RowHeader
        icon={<BrainIcon />}
        verb={streaming ? "思考中" : "思考"}
        target={body || level === "hidden" ? undefined : activitySnippet(block, 160)}
        pulse={streaming}
        interactive={hasText}
        open={level === "full"}
        sticky={hasText}
        onToggle={() => setLevel(toggledLevel(axis, level))}
        suffix={
          <>
            {rate !== undefined && (
              <span className="text-2xs tabular-nums text-text-tertiary">{rate} t/s</span>
            )}
            <Duration ms={block.durationMs} />
          </>
        }
      />
      {body && <div className="border-l border-border pt-0.5 pb-2 ml-[13px] pl-[13px]">{body}</div>}
    </div>
  );
}

// ---------- 工具 ----------

function toolSuffix(
  tool: ToolBlock,
  stat?: string,
  statError?: boolean,
  now = Date.now(),
): ReactNode {
  if (tool.status === "running") {
    const elapsed = tool.job?.started_at_ms ? now - tool.job.started_at_ms : undefined;
    return (
      <>
        {tool.awaitingPermission ? (
          <span className="text-2xs text-warn">等待授权</span>
        ) : (
          <Loader2Icon className="size-3 animate-spin text-text-tertiary" />
        )}
        <Duration ms={elapsed} />
      </>
    );
  }
  if (tool.status === "denied") return <span className="text-2xs text-warn">已拒绝</span>;
  if (tool.status === "cancelled")
    return <span className="text-2xs text-text-tertiary">已取消</span>;
  const files = tool.diff?.files ?? [];
  const total = files.reduce(
    (sum, file) => {
      const stats = diffStats(file);
      return { added: sum.added + stats.added, removed: sum.removed + stats.removed };
    },
    { added: 0, removed: 0 },
  );
  return (
    <>
      {stat && (
        <span
          className={cn(
            "text-2xs tabular-nums",
            statError ? "text-destructive" : "text-text-tertiary",
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
  const [level, setLevel] = useState(axis);
  const now = useNow(tool.status === "running" && Boolean(tool.job));
  const headline = toolHeadline(tool);
  const failed = toolFailed(tool);
  const interactive =
    toolExpandable(tool) &&
    Boolean(tool.result || tool.liveOutput || tool.diff || tool.arguments.trim());
  const toggle = () => setLevel(toggledLevel(axis, level));
  const body = toolBody(tool, level, () => setLevel("full"));
  // read / edit / write 的目标路径可点击：在文件分页打开（read 带 offset 行号）。
  const targetClick = (() => {
    if (tool.name !== "read" && tool.name !== "edit" && tool.name !== "write") return undefined;
    const target = headline.target;
    if (!target) return undefined;
    return () => {
      const cwd = useTranscript.getState().bySession[sessionId]?.info.cwd ?? "";
      const parsed = parseLinkTarget(target, cwd);
      const offset = tool.name === "read" ? argOffset(tool) : undefined;
      if (parsed) openFile(sessionId, parsed.target, { line: parsed.line ?? offset });
    };
  })();
  const extras =
    (level !== "hidden" || !toolExpandable(tool)) &&
    (tool.images.length > 0 || Boolean(tool.progress?.length));
  const canBackground = tool.job?.origin === "foreground" && tool.job.status === "running";
  const background = () => {
    if (!tool.job) return;
    void rpc
      .call<JobResult>("job/background", { session_id: sessionId, job_id: tool.job.job_id })
      .catch((error) =>
        toast.error(`转后台失败：${error instanceof Error ? error.message : String(error)}`),
      );
  };
  // 已收场的提问可以重答：core 截回未回答状态后按原参数重放提问卡片。
  const canReask = tool.name === "ask" && tool.status !== "running";
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
      转后台
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
      重新作答
    </Button>
  ) : undefined;
  return (
    <div>
      <RowHeader
        icon={iconFor(tool)}
        verb={headline.verb ?? activityVerb(tool) ?? "调用工具"}
        target={headline.target}
        scope={headline.scope}
        mono={headline.mono}
        failed={failed}
        suffix={toolSuffix(tool, headline.stat, headline.statError, now)}
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
          {extras && <ToolExtras tool={tool} />}
        </Body>
      )}
      {tool.children.length > 0 && (
        <div className="ml-[13px] border-l border-border pl-2.5">
          {tool.children.map((child) => (
            <ToolRow key={`${child.id}:${axis}`} tool={child} axis={axis} sessionId={sessionId} />
          ))}
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
function ToolExtras({ tool }: { tool: ToolBlock }) {
  return (
    <>
      {tool.progress && tool.progress.length > 0 && (
        <div className="space-y-1 rounded-xl bg-code px-3 py-2 text-xs">
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
              className="overflow-hidden rounded-xl bg-code"
            >
              <img
                src={imageSrc(image)}
                alt="工具返回的图片"
                className="max-h-48 max-w-[240px] object-contain"
              />
            </a>
          ))}
        </div>
      )}
    </>
  );
}

/** 合并行的动作文案：`读取 4 个文件`。 */
const GROUP_VERB: Record<string, (n: number) => string> = {
  read: (n) => `读取 ${n} 个文件`,
  grep: (n) => `搜索代码 ${n} 次`,
  find: (n) => `查找文件 ${n} 次`,
  web_search: (n) => `搜索网页 ${n} 次`,
  web_fetch: (n) => `访问 ${n} 个网页`,
};

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
  if (name === "web_search") {
    const hits = done.reduce(
      (sum, tool) => sum + parseSearch(tool.result as string).hits.length,
      0,
    );
    return `${hits} results`;
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
  const failedCount = tools.filter(toolFailed).length;
  const [open, setOpen] = useState(axis === "full" || failedCount > 0);
  const running = tools.some((tool) => tool.status === "running");
  const targets = tools
    .map((tool) => toolHeadline(tool).target.split(/[\\/]/).pop() ?? "")
    .filter(Boolean)
    .join("  ");
  const verb =
    GROUP_VERB[name]?.(tools.length) ?? `${activityVerb(tools[0]) ?? "调用工具"} ×${tools.length}`;
  const stat = groupStat(name, tools);
  return (
    <div>
      <RowHeader
        icon={iconFor(tools[0])}
        verb={verb}
        target={targets}
        mono
        failed={failedCount > 0}
        interactive
        open={open}
        sticky
        onToggle={() => setOpen(!open)}
        suffix={
          running ? (
            <Loader2Icon className="size-3 animate-spin text-text-tertiary" />
          ) : (
            <>
              {failedCount > 0 && (
                <span className="text-2xs text-destructive">{failedCount} 失败</span>
              )}
              {stat && <span className="text-2xs tabular-nums text-text-tertiary">{stat}</span>}
            </>
          )
        }
      />
      {open && (
        <div className="ml-[13px] border-l border-border pl-2.5">
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
    arguments: JSON.stringify({ command: block.command }),
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
  };
}

function AgentMessageRow({
  block,
  axis,
}: {
  block: Extract<Block, { kind: "agent-message" }>;
  axis: DetailLevel;
}) {
  const [level, setLevel] = useState(axis);
  const head = headLines(block.body, BRIEF_BODY_ROWS);
  const lines = level === "full" ? block.body : head.lines.join("\n");
  return (
    <div>
      <RowHeader
        icon={<BotIcon />}
        verb="子代理消息"
        target={block.from}
        interactive
        open={level === "full"}
        sticky
        onToggle={() => setLevel(toggledLevel(axis, level))}
      />
      {level !== "hidden" && (
        <Body>
          <div className="rounded-xl bg-code px-3.5 py-2.5 text-sm whitespace-pre-wrap text-muted-foreground">
            {lines}
            {level === "brief" && head.hidden > 0 && (
              <button
                type="button"
                onClick={() => setLevel("full")}
                className="mt-1 block text-2xs text-text-tertiary hover:text-foreground"
              >
                ⋯ 还有 {head.hidden} 行
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
  const [level, setLevel] = useState(axis);
  const head = headLines(block.body, BRIEF_BODY_ROWS);
  const lines = level === "full" ? block.body : head.lines.join("\n");
  return (
    <div>
      <RowHeader
        icon={<BotIcon />}
        verb={`目标续跑 · 第 ${block.round} 轮`}
        interactive
        open={level === "full"}
        sticky
        onToggle={() => setLevel(toggledLevel(axis, level))}
      />
      {level !== "hidden" && (
        <Body>
          <div className="rounded-xl bg-code px-3.5 py-2.5 text-sm whitespace-pre-wrap text-muted-foreground">
            {lines}
            {level === "brief" && head.hidden > 0 && (
              <button
                type="button"
                onClick={() => setLevel("full")}
                className="mt-1 block text-2xs text-text-tertiary hover:text-foreground"
              >
                ⋯ 还有 {head.hidden} 行
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
  const thinking = usePrefs((state) => state.thinkingDetail);
  const tools = usePrefs((state) => state.toolDetail);
  const items = useMemo(() => groupActivity(blocks), [blocks]);
  return (
    <div className="-mx-1.5">
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
            return <AgentMessageRow key={`${block.id}:${tools}`} block={block} axis={tools} />;
          case "goal-round":
            return <GoalRoundRow key={`${block.id}:${tools}`} block={block} axis={tools} />;
          case "status":
            return (
              <RowHeader
                key={block.id}
                icon={<InfoIcon />}
                verb="状态更新"
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
