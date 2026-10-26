// 转写列表：@tanstack/react-virtual 动态高度虚拟列表。
// 离底部 < 80px 时跟随新内容，否则显示「↓ 新内容」；滚到顶时向前翻页。

import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown, Brain, Copy, FileText, GitBranch, Loader2, RotateCcw } from "lucide-react";
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { Button } from "@/components/ui/button";
import { clockTime, formatDuration } from "@/lib/format";
import { type Block, finishLabel, type ToolBlock } from "@/lib/transcript-model";
import { cn } from "@/lib/utils";
import { retryLast } from "@/store/actions";
import { loadOlder, rewind } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { runAction } from "../layout/session-actions";
import { Markdown } from "./markdown";
import { ImageStrip, ToolBlockView, ToolGroupView } from "./tool-block";

const FOLLOW_THRESHOLD = 80;
const BRIEF_REASONING_LINES = 4;

/** 渲染行：普通块，或同一批次的同名工具组。 */
type Row = { key: string; block: Block } | { key: string; group: ToolBlock[] };

export function toRows(blocks: Block[]): Row[] {
  const rows: Row[] = [];
  for (const block of blocks) {
    const last = rows.at(-1);
    if (
      block.kind === "tool" &&
      block.batchId &&
      last &&
      "group" in last &&
      last.group[0]?.batchId === block.batchId &&
      last.group[0]?.name === block.name
    ) {
      last.group.push(block);
      continue;
    }
    if (
      block.kind === "tool" &&
      block.batchId &&
      last &&
      "block" in last &&
      last.block.kind === "tool" &&
      last.block.batchId === block.batchId &&
      last.block.name === block.name
    ) {
      rows[rows.length - 1] = { key: last.key, group: [last.block, block] };
      continue;
    }
    rows.push({ key: block.id, block });
  }
  return rows;
}

export function TranscriptView({ sessionId }: { sessionId: string }) {
  const blocks = useTranscript((state) => state.bySession[sessionId]?.blocks);
  const hasMore = useTranscript((state) => state.bySession[sessionId]?.hasMore ?? false);
  const loadingOlder = useTranscript((state) => state.bySession[sessionId]?.loadingOlder ?? false);
  const running = useTranscript((state) => state.bySession[sessionId]?.running ?? false);
  const rows = useMemo(() => toRows(blocks ?? []), [blocks]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [atBottom, setAtBottom] = useState(true);
  const followRef = useRef(true);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 80,
    overscan: 8,
    getItemKey: (index) => rows[index]?.key ?? index,
  });

  // 新内容到达时跟随到底部。
  useLayoutEffect(() => {
    if (followRef.current && rows.length > 0) {
      virtualizer.scrollToIndex(rows.length - 1, { align: "end" });
    }
  }, [rows, virtualizer]);

  // 切换会话时回到底部。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 只在会话切换时触发
  useEffect(() => {
    followRef.current = true;
    setAtBottom(true);
  }, [sessionId]);

  const onScroll = () => {
    const element = scrollRef.current;
    if (!element) return;
    const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
    const bottom = distance < FOLLOW_THRESHOLD;
    followRef.current = bottom;
    setAtBottom(bottom);
    if (element.scrollTop < 200 && hasMore && !loadingOlder) void loadOlder(sessionId);
  };

  const jumpToBottom = () => {
    followRef.current = true;
    setAtBottom(true);
    virtualizer.scrollToIndex(rows.length - 1, { align: "end" });
  };

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="min-h-0 flex-1 overflow-y-auto"
        role="log"
        aria-label="会话转写"
      >
        {loadingOlder ? (
          <div className="flex justify-center py-3 text-muted-foreground text-xs">
            <Loader2 className="size-4 animate-spin" aria-hidden />
            <span className="sr-only">正在加载更早的消息</span>
          </div>
        ) : null}
        {rows.length === 0 && !loadingOlder ? (
          <div className="flex h-full items-center justify-center text-muted-foreground text-sm">
            开始一段新的对话
          </div>
        ) : null}
        <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
          {virtualizer.getVirtualItems().map((item) => {
            const row = rows[item.index];
            if (!row) return null;
            return (
              <div
                key={item.key}
                data-index={item.index}
                ref={virtualizer.measureElement}
                className="absolute inset-x-0 top-0"
                style={{ transform: `translateY(${item.start}px)` }}
              >
                <div className="mx-auto max-w-[780px] px-6 py-1.5">
                  {"group" in row ? (
                    <ToolGroupView blocks={row.group} />
                  ) : (
                    <BlockView sessionId={sessionId} block={row.block} running={running} />
                  )}
                </div>
              </div>
            );
          })}
        </div>
        <div className="h-6" />
      </div>
      {!atBottom ? (
        <Button
          variant="outline"
          size="sm"
          onClick={jumpToBottom}
          className="-translate-x-1/2 absolute bottom-3 left-1/2 rounded-full shadow-sm"
        >
          <ArrowDown aria-hidden />
          新内容
        </Button>
      ) : null}
      <MismatchPill sessionId={sessionId} />
      <div aria-live="polite" className="sr-only">
        {running ? "" : "生成已结束"}
      </div>
    </div>
  );
}

const BlockView = memo(function BlockView({
  sessionId,
  block,
  running,
}: {
  sessionId: string;
  block: Block;
  running: boolean;
}) {
  switch (block.kind) {
    case "user":
      return <UserBlock sessionId={sessionId} block={block} running={running} />;
    case "text":
      return <AssistantText sessionId={sessionId} block={block} running={running} />;
    case "reasoning":
      return <ReasoningBlock block={block} />;
    case "tool":
      return <ToolBlockView block={block} />;
    case "bash":
      return (
        <div className="rounded-xl border bg-panel px-3 py-2 font-mono text-xs">
          <div className="text-muted-foreground">$ {block.command}</div>
          {block.output ? (
            <pre className="mt-1 max-h-80 overflow-auto whitespace-pre-wrap break-words">
              {block.output}
            </pre>
          ) : null}
        </div>
      );
    case "agent-message":
      return (
        <div className="rounded-xl border-brand/60 border-l-2 bg-panel px-3 py-2">
          <div className="mb-1 text-muted-foreground text-xs">来自子代理 · {block.from}</div>
          <Markdown text={block.body} />
        </div>
      );
    case "status":
      return <StatusBlock sessionId={sessionId} block={block} running={running} />;
    case "plan":
      return <PlanCard sessionId={sessionId} block={block} />;
    case "footer":
      return (
        <div className="pb-2 text-muted-foreground text-xs">
          {[
            block.durationMs !== undefined ? formatDuration(block.durationMs) : null,
            block.model,
            clockTime(block.atMs),
            block.finishReason && block.finishReason !== "Stop"
              ? finishLabel(block.finishReason)
              : null,
          ]
            .filter(Boolean)
            .join(" · ")}
        </div>
      );
  }
});

function UserBlock({
  sessionId,
  block,
  running,
}: {
  sessionId: string;
  block: Extract<Block, { kind: "user" }>;
  running: boolean;
}) {
  return (
    <div className="group flex flex-col items-end gap-1 py-1">
      <div className="max-w-[85%] space-y-2 rounded-2xl bg-secondary px-4 py-2.5">
        {block.text ? <p className="whitespace-pre-wrap break-words">{block.text}</p> : null}
        <ImageStrip images={block.images} />
      </div>
      <div className="flex items-center gap-0.5 text-muted-foreground text-xs opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
        <span className="mr-1">{clockTime(block.atMs)}</span>
        <IconButton
          label="复制"
          size="icon-xs"
          onClick={() => void navigator.clipboard.writeText(block.text)}
        >
          <Copy aria-hidden />
        </IconButton>
        {block.entryId ? (
          <IconButton
            label="从这里回退"
            size="icon-xs"
            disabled={running}
            onClick={() => {
              const entryId = block.entryId;
              if (entryId) void runAction(() => rewind(sessionId, entryId, block.text));
            }}
          >
            <RotateCcw aria-hidden />
          </IconButton>
        ) : null}
      </div>
    </div>
  );
}

function AssistantText({
  sessionId,
  block,
  running,
}: {
  sessionId: string;
  block: Extract<Block, { kind: "text" }>;
  running: boolean;
}) {
  return (
    <div className="group relative">
      <Markdown text={block.text} />
      {block.entryId ? (
        <div className="absolute top-0 -right-10 opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
          <IconButton
            label="从这里回退"
            size="icon-xs"
            side="left"
            disabled={running}
            onClick={() => {
              const entryId = block.entryId;
              if (entryId) void runAction(() => rewind(sessionId, entryId));
            }}
          >
            <GitBranch aria-hidden />
          </IconButton>
        </div>
      ) : null}
    </div>
  );
}

function ReasoningBlock({ block }: { block: Extract<Block, { kind: "reasoning" }> }) {
  const tier = usePrefs((state) => state.thinkingTier);
  const [expanded, setExpanded] = useState(false);
  const effective = expanded ? "full" : tier;
  const live = block.startedAt !== undefined && block.durationMs === undefined;
  const label =
    block.durationMs !== undefined ? `思考 ${formatDuration(block.durationMs)}` : "思考";
  const body =
    effective === "full"
      ? block.text
      : effective === "brief"
        ? block.text.replace(/\n+$/, "").split("\n").slice(-BRIEF_REASONING_LINES).join("\n")
        : "";
  return (
    <div className="border-l-2 pl-3 text-muted-foreground text-sm">
      <button
        type="button"
        aria-expanded={effective === "full"}
        onClick={() => setExpanded((value) => !value)}
        className="flex items-center gap-1.5 text-xs hover:text-foreground"
      >
        {live ? (
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
        ) : (
          <Brain className="size-3.5" aria-hidden />
        )}
        {live ? "思考中…" : label}
      </button>
      {body ? <p className="mt-1 whitespace-pre-wrap break-words text-[13px]">{body}</p> : null}
    </div>
  );
}

const TONE_CLASS: Record<"info" | "warn" | "error", string> = {
  info: "text-muted-foreground",
  warn: "text-warn",
  error: "text-destructive",
};

function StatusBlock({
  sessionId,
  block,
  running,
}: {
  sessionId: string;
  block: Extract<Block, { kind: "status" }>;
  running: boolean;
}) {
  return (
    <div
      className={cn("flex items-center justify-center gap-2 py-1 text-xs", TONE_CLASS[block.tone])}
    >
      <span className="h-px w-8 bg-border" aria-hidden />
      <span className="text-center">{block.text}</span>
      {block.retryable ? (
        <Button variant="outline" size="xs" disabled={running} onClick={() => retryLast(sessionId)}>
          <RotateCcw aria-hidden />
          重试
        </Button>
      ) : null}
      <span className="h-px w-8 bg-border" aria-hidden />
    </div>
  );
}

function PlanCard({
  sessionId,
  block,
}: {
  sessionId: string;
  block: Extract<Block, { kind: "plan" }>;
}) {
  const status = useTranscript(
    (state) =>
      state.bySession[sessionId]?.plans.find((plan) => plan.plan_id === block.planId)?.status,
  );
  const set = useUi((state) => state.set);
  const STATUS_LABEL: Record<string, string> = {
    submitted: "待审阅",
    approved: "已批准",
    rejected: "已拒绝",
  };
  return (
    <div className="flex items-center gap-3 rounded-xl border bg-card px-4 py-3">
      <FileText className="size-5 shrink-0 text-brand" aria-hidden />
      <div className="min-w-0 flex-1">
        <div className="truncate font-medium">{block.title}</div>
        <div className="truncate text-muted-foreground text-xs">
          {block.path}
          {status ? ` · ${STATUS_LABEL[status] ?? status}` : ""}
        </div>
      </div>
      {status === "submitted" ? (
        <Button size="sm" onClick={() => set({ planReview: { sessionId, planId: block.planId } })}>
          审阅计划
        </Button>
      ) : null}
    </div>
  );
}

const MISMATCH_VISIBLE_MS = 8000;

function MismatchPill({ sessionId }: { sessionId: string }) {
  const mismatch = useTranscript((state) => state.bySession[sessionId]?.mismatch ?? null);
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    if (!mismatch) return;
    setVisible(true);
    const timer = window.setTimeout(() => setVisible(false), MISMATCH_VISIBLE_MS);
    return () => window.clearTimeout(timer);
  }, [mismatch]);
  if (!mismatch || !visible) return null;
  return (
    <div
      role="status"
      className="absolute top-3 right-4 rounded-full border border-brand/40 bg-brand/10 px-3 py-1 text-brand text-xs"
    >
      上游返回不一致：
      {mismatch.fields
        .map((field) => `${field.field} ${field.requested} → ${field.reported}`)
        .join("，")}
    </div>
  );
}
