// 对话流：用户气泡 + 助手轮次（按时间排列的思考/工具/正文片段）+ 末尾的授权与提问卡片。
// 完成的轮次把过程收进「处理过程」，只露出最后一段正文；进行中的轮次全部展开。

import {
  AlertTriangleIcon,
  CheckIcon,
  ChevronRightIcon,
  CircleXIcon,
  CopyIcon,
  ListChecksIcon,
  Loader2Icon,
  PencilIcon,
  RotateCcwIcon,
  Undo2Icon,
} from "lucide-react";
import { type ReactNode, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { useStickToBottomContext } from "use-stick-to-bottom";
import {
  Conversation,
  ConversationContent,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation";
import { MessageResponse } from "@/components/ai-elements/message";
import { Shimmer } from "@/components/ai-elements/shimmer";
import { ActivityGroup, imageSrc } from "@/components/chat/activity";
import { AskCard, PermissionCard } from "@/components/chat/requests";
import { IconButton } from "@/components/common/icon-button";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { clockTime } from "@/lib/format";
import { modelName } from "@/lib/models";
import { type Block, finishLabel } from "@/lib/transcript-model";
import {
  formatDuration,
  groupTurns,
  type Segment,
  summarizeActivity,
  type Turn,
  turnDurationMs,
} from "@/lib/turn";
import { cn } from "@/lib/utils";
import type { AskParams, ImageSource, PermissionRequestParams } from "@/protocol/types";
import { retrySession } from "@/store/actions";
import { useComposer } from "@/store/composer";
import { useLayout } from "@/store/layout";
import { loadOlder, redo, rewind } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";

const EMPTY_PERMISSIONS: PermissionRequestParams[] = [];
const EMPTY_ASKS: AskParams[] = [];
/** 用户消息超过这么多字符或行数时默认折叠。 */
const CLAMP_CHARS = 700;
const CLAMP_LINES = 14;

type TurnContext = {
  sessionId: string;
  readOnly: boolean;
  running: boolean;
};

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 回退后给一个「撤销」入口：core 把截掉的尾部留在内存里，`session/redo` 能原样接回。 */
function rewindTo(sessionId: string, entryId: string, restoreText?: string): void {
  rewind(sessionId, entryId, restoreText)
    .then((redoId) => {
      toast("已回退到这里", {
        description: "之后的对话已移出会话",
        action: redoId
          ? {
              label: "撤销",
              onClick: () =>
                void redo(sessionId, redoId)
                  .then(() => {
                    const composer = useComposer.getState();
                    if (
                      restoreText !== undefined &&
                      composer.drafts[sessionId]?.text === restoreText
                    ) {
                      composer.clearDraft(sessionId);
                    }
                    toast.success("已恢复回退前的对话");
                  })
                  .catch((error) => toast.error(`撤销失败：${errorText(error)}`)),
            }
          : undefined,
      });
    })
    .catch((error) => toast.error(`回退失败：${errorText(error)}`));
}

function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [active]);
  return now;
}

function copy(text: string, message: string) {
  void navigator.clipboard.writeText(text);
  toast.success(message);
}

export function ChatThread({ sessionId }: { sessionId: string }) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const permissions = useTranscript((state) => state.permissions[sessionId]) ?? EMPTY_PERMISSIONS;
  const asks = useTranscript((state) => state.asks[sessionId]) ?? EMPTY_ASKS;
  const blocks = view?.blocks;
  const turns = useMemo(() => groupTurns(blocks ?? []), [blocks]);
  if (!view) return null;

  const context: TurnContext = { sessionId, readOnly: view.readOnly, running: view.running };

  return (
    <div className="moc-chat-pane relative flex min-h-0 flex-1 overflow-hidden">
      <Conversation className="flex-1">
        <ConversationContent className="moc-chat-column gap-10 px-0 pt-10 pb-16">
          {view.hasMore && <LoadOlder sessionId={sessionId} loading={view.loadingOlder} />}
          {turns.map((turn, index) => (
            <TurnView
              key={turn.id}
              turn={turn}
              last={index === turns.length - 1}
              live={view.running && index === turns.length - 1}
              liveStartedAt={view.live?.startedAt}
              context={context}
            />
          ))}
          {view.mismatch && (
            <div className="flex items-center gap-2 text-xs text-warn">
              <AlertTriangleIcon className="size-3.5" />
              上游回显与请求不一致：
              {view.mismatch.fields
                .map((field) => `${field.field} ${field.requested} → ${field.reported}`)
                .join("，")}
            </div>
          )}
          {permissions.map((request) => (
            <PermissionCard key={request.request_id} sessionId={sessionId} request={request} />
          ))}
          {asks.map((request) => (
            <AskCard key={request.request_id} sessionId={sessionId} request={request} />
          ))}
        </ConversationContent>
        <ConversationScrollButton className="bottom-4 size-8 border-border bg-popover text-muted-foreground shadow-md hover:bg-popover hover:text-foreground dark:bg-popover" />
      </Conversation>
    </div>
  );
}

/** 滚到顶部附近时自动翻页，并保持当前阅读位置不跳动。 */
function LoadOlder({ sessionId, loading }: { sessionId: string; loading: boolean }) {
  const { scrollRef } = useStickToBottomContext();
  const sentinel = useRef<HTMLDivElement>(null);
  const anchor = useRef<{ height: number; top: number } | null>(null);

  const load = () => {
    const scroller = scrollRef.current;
    if (!scroller || loading) return;
    anchor.current = { height: scroller.scrollHeight, top: scroller.scrollTop };
    void loadOlder(sessionId).catch((error) => toast.error(`加载历史失败：${errorText(error)}`));
  };

  useLayoutEffect(() => {
    const scroller = scrollRef.current;
    const saved = anchor.current;
    if (loading || !scroller || !saved) return;
    scroller.scrollTop = saved.top + (scroller.scrollHeight - saved.height);
    anchor.current = null;
  });

  useEffect(() => {
    const node = sentinel.current;
    const root = scrollRef.current;
    if (!node || !root) return;
    const observer = new IntersectionObserver(
      (entries) => entries.some((entry) => entry.isIntersecting) && load(),
      { root, rootMargin: "200px 0px 0px 0px" },
    );
    observer.observe(node);
    return () => observer.disconnect();
  });

  return (
    <div ref={sentinel} className="flex justify-center">
      <Button
        variant="ghost"
        size="sm"
        className="h-7 text-xs text-text-tertiary"
        onClick={load}
        disabled={loading}
      >
        {loading && <Loader2Icon className="size-3.5 animate-spin" />}
        {loading ? "正在加载" : "加载更早的消息"}
      </Button>
    </div>
  );
}

function TurnView({
  turn,
  last,
  live,
  liveStartedAt,
  context,
}: {
  turn: Turn;
  last: boolean;
  live: boolean;
  liveStartedAt?: number;
  context: TurnContext;
}) {
  return (
    <section className="group/turn flex flex-col gap-3">
      {turn.user && <UserMessage block={turn.user} context={context} />}
      {(turn.segments.length > 0 || live) && (
        <AssistantTurn
          turn={turn}
          last={last}
          live={live}
          liveStartedAt={liveStartedAt}
          context={context}
        />
      )}
    </section>
  );
}

function ImageThumbs({ images, className }: { images: ImageSource[]; className?: string }) {
  const [preview, setPreview] = useState<string | null>(null);
  if (images.length === 0) return null;
  return (
    <>
      <div className={cn("flex flex-wrap gap-2", className)}>
        {images.map((image, index) => (
          <button
            // biome-ignore lint/suspicious/noArrayIndexKey: 消息附件只读不重排，且没有稳定 id
            key={index}
            type="button"
            onClick={() => setPreview(imageSrc(image))}
            className="overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle transition-opacity hover:opacity-90"
          >
            <img src={imageSrc(image)} alt="图片附件" className="h-24 max-w-[200px] object-cover" />
          </button>
        ))}
      </div>
      <Dialog open={preview !== null} onOpenChange={(open) => !open && setPreview(null)}>
        <DialogContent className="max-w-[min(92vw,1100px)] border-0 bg-transparent p-0 shadow-none sm:max-w-[min(92vw,1100px)]">
          <DialogTitle className="sr-only">图片预览</DialogTitle>
          {preview && (
            <img
              src={preview}
              alt="图片预览"
              className="max-h-[86vh] w-full rounded-xl object-contain"
            />
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

function UserMessage({
  block,
  context,
}: {
  block: Extract<Block, { kind: "user" }>;
  context: TurnContext;
}) {
  const long = block.text.length > CLAMP_CHARS || block.text.split("\n").length > CLAMP_LINES;
  const [expanded, setExpanded] = useState(false);
  const canEdit = Boolean(block.entryId) && !context.readOnly && !context.running;

  return (
    <div className="group/user flex flex-col items-end gap-1">
      <ImageThumbs images={block.images} className="justify-end" />
      {block.text && (
        <div className="relative max-w-[85%] rounded-[20px] bg-bubble px-4.5 py-3 text-[15px] leading-[1.65] text-foreground">
          <div
            className={cn(
              "whitespace-pre-wrap break-words",
              long &&
                !expanded &&
                "max-h-[280px] overflow-hidden [mask-image:linear-gradient(to_bottom,black_75%,transparent)]",
            )}
          >
            {block.text}
          </div>
          {long && (
            <button
              type="button"
              onClick={() => setExpanded((value) => !value)}
              className="mt-1 text-xs font-medium text-muted-foreground hover:text-foreground"
            >
              {expanded ? "收起" : "展开全部"}
            </button>
          )}
        </div>
      )}
      <div className="flex items-center gap-0.5 opacity-0 transition-opacity group-hover/user:opacity-100 focus-within:opacity-100">
        <span className="mr-1 text-[11px] tabular-nums text-text-tertiary">
          {clockTime(block.atMs)}
        </span>
        <IconButton label="复制" size="xs" onClick={() => copy(block.text, "已复制")}>
          <CopyIcon />
        </IconButton>
        {canEdit && block.entryId && (
          <IconButton
            label="回退到这里编辑"
            size="xs"
            onClick={() => rewindTo(context.sessionId, block.entryId as string, block.text)}
          >
            <PencilIcon />
          </IconButton>
        )}
      </div>
    </div>
  );
}

function liveLabel(turn: Turn): string {
  const lastBlock = turn.activity.at(-1);
  if (!lastBlock) return "思考中";
  if (lastBlock.kind === "reasoning" && lastBlock.durationMs === undefined) return "思考中";
  const tail = turn.segments.at(-1);
  if (tail?.kind === "text") return "正在回复";
  return summarizeActivity(turn.activity, false);
}

function AssistantTurn({
  turn,
  last,
  live,
  liveStartedAt,
  context,
}: {
  turn: Turn;
  last: boolean;
  live: boolean;
  liveStartedAt?: number;
  context: TurnContext;
}) {
  const now = useNow(live);
  const [processOpen, setProcessOpen] = useState(false);
  const segments = turn.segments;
  let finalIndex = -1;
  for (let index = segments.length - 1; index >= 0; index--) {
    if (segments[index].kind === "text") {
      finalIndex = index;
      break;
    }
    if (segments[index].kind === "activity") break;
  }
  const processCount = segments.filter(
    (segment, index) =>
      index !== finalIndex && (segment.kind === "activity" || segment.kind === "text"),
  ).length;
  const durationMs = turnDurationMs(turn);
  const finalText = segments
    .filter((segment): segment is Extract<Segment, { kind: "text" }> => segment.kind === "text")
    .map((segment) => segment.block.text)
    .join("\n\n");
  const finalBlock =
    finalIndex >= 0
      ? (segments[finalIndex] as Extract<Segment, { kind: "text" }>).block
      : undefined;
  const finish = turn.footer?.finishReason;
  const finishText = finishLabel(finish);
  const abnormal =
    !live &&
    last &&
    (segments.some((segment) => segment.kind === "notice" && segment.block.tone === "error") ||
      Boolean(finishText) ||
      !turn.footer);
  const canRetry = abnormal && !context.readOnly && !context.running;

  const renderSegment = (segment: Segment, index: number): ReactNode => {
    switch (segment.kind) {
      case "activity":
        return <ActivityGroup key={segment.id} blocks={segment.blocks} live={live} />;
      case "text":
        return (
          <MessageResponse key={segment.id} isAnimating={live && index === segments.length - 1}>
            {segment.block.text}
          </MessageResponse>
        );
      case "plan":
        return <PlanCard key={segment.id} sessionId={context.sessionId} block={segment.block} />;
      case "notice":
        return (
          <NoticeCard
            key={segment.id}
            block={segment.block}
            onRetry={
              canRetry && segment.block.tone === "error"
                ? () => void retrySession(context.sessionId)
                : undefined
            }
          />
        );
    }
  };

  return (
    <div className="flex flex-col gap-4">
      {live ? (
        segments.map(renderSegment)
      ) : (
        <>
          {processCount > 0 && (
            <div>
              <button
                type="button"
                onClick={() => setProcessOpen((value) => !value)}
                className="group/proc -ml-1.5 flex h-7 items-center gap-1.5 rounded-md px-1.5 text-[13.5px] text-text-tertiary transition-colors hover:bg-surface-hover hover:text-muted-foreground"
              >
                <span>
                  {durationMs !== undefined ? `处理用时 ${formatDuration(durationMs)}` : "处理过程"}
                </span>
                <span className="text-text-tertiary/60">·</span>
                <span className="max-w-[420px] truncate">{summarizeActivity(turn.activity)}</span>
                <ChevronRightIcon
                  className={cn("size-3.5 transition-transform", processOpen && "rotate-90")}
                />
              </button>
              {processOpen && (
                <div className="mt-2 flex flex-col gap-3 border-l border-border pl-4">
                  {segments.map((segment, index) =>
                    index === finalIndex || segment.kind === "plan" || segment.kind === "notice"
                      ? null
                      : renderSegment(segment, index),
                  )}
                </div>
              )}
            </div>
          )}
          {segments.map((segment, index) =>
            index === finalIndex || segment.kind === "plan" || segment.kind === "notice"
              ? renderSegment(segment, index)
              : processCount === 0
                ? renderSegment(segment, index)
                : null,
          )}
        </>
      )}

      {live && (
        <div className="flex h-7 items-center gap-2 text-[13.5px]">
          <span className="moc-dots flex gap-[3px]">
            <span className="size-1.5 rounded-full bg-text-tertiary" />
            <span className="size-1.5 rounded-full bg-text-tertiary" />
            <span className="size-1.5 rounded-full bg-text-tertiary" />
          </span>
          <Shimmer as="span" duration={1.8} className="font-medium">
            {liveLabel(turn)}
          </Shimmer>
          {liveStartedAt !== undefined && (
            <span className="text-xs tabular-nums text-text-tertiary">
              {formatDuration(Math.max(0, now - liveStartedAt))}
            </span>
          )}
        </div>
      )}

      {!live && (finalText || turn.footer || canRetry) && (
        <div
          className={cn(
            "flex items-center gap-0.5 transition-opacity",
            last ? "opacity-100" : "opacity-0 group-hover/turn:opacity-100 hover:opacity-100",
          )}
        >
          {finalText && (
            <IconButton label="复制回复" size="xs" onClick={() => copy(finalText, "已复制回复")}>
              <CopyIcon />
            </IconButton>
          )}
          {finalBlock?.entryId && (
            <IconButton
              label="回退到这里"
              size="xs"
              onClick={() => rewindTo(context.sessionId, finalBlock.entryId as string)}
            >
              <Undo2Icon />
            </IconButton>
          )}
          {canRetry && (
            <IconButton
              label="继续 / 重试"
              size="xs"
              onClick={() => void retrySession(context.sessionId)}
            >
              <RotateCcwIcon />
            </IconButton>
          )}
          <span className="ml-1.5 flex items-center gap-1.5 text-[11px] text-text-tertiary">
            {turn.footer?.model && (
              <span className="font-mono">{modelName(turn.footer.model)}</span>
            )}
            {finishText && <span className="text-warn">{finishText}</span>}
            {turn.footer && <span className="tabular-nums">{clockTime(turn.footer.atMs)}</span>}
          </span>
        </div>
      )}
    </div>
  );
}

function NoticeCard({
  block,
  onRetry,
}: {
  block: Extract<Block, { kind: "status" }>;
  onRetry?: () => void;
}) {
  const error = block.tone === "error";
  return (
    <div
      className={cn(
        "flex items-start gap-2.5 rounded-lg border px-3.5 py-2.5 text-[13.5px] leading-5",
        error
          ? "border-destructive/25 bg-destructive/6 text-destructive"
          : "border-warn/25 bg-warn/8 text-warn",
      )}
    >
      {error ? (
        <CircleXIcon className="mt-0.5 size-4 shrink-0" />
      ) : (
        <AlertTriangleIcon className="mt-0.5 size-4 shrink-0" />
      )}
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{block.text}</span>
      {onRetry && (
        <Button
          size="sm"
          variant="outline"
          className="-my-0.5 h-7 shrink-0 bg-card text-xs"
          onClick={onRetry}
        >
          <RotateCcwIcon className="size-3.5" />
          重试
        </Button>
      )}
    </div>
  );
}

const PLAN_STATUS: Record<string, { label: string; tone: string }> = {
  draft: { label: "起草中", tone: "text-text-tertiary" },
  submitted: { label: "待审阅", tone: "text-brand" },
  revised: { label: "等待修订", tone: "text-warn" },
  superseded: { label: "已被取代", tone: "text-text-tertiary" },
  approved: { label: "已批准", tone: "text-success" },
  rejected: { label: "已拒绝", tone: "text-destructive" },
};

function PlanCard({
  sessionId,
  block,
}: {
  sessionId: string;
  block: Extract<Block, { kind: "plan" }>;
}) {
  const plan = useTranscript((state) =>
    state.bySession[sessionId]?.plans.find((item) => item.plan_id === block.planId),
  );
  const status = PLAN_STATUS[plan?.status ?? "draft"];
  const pending = plan?.status === "submitted";
  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-xl bg-card px-3.5 py-3 ring-1 ring-inset",
        pending ? "ring-brand/35" : "ring-border-subtle",
      )}
    >
      <span className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-muted text-muted-foreground">
        <ListChecksIcon className="size-4" />
      </span>
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13.5px] font-medium">{plan?.title ?? block.title}</div>
        <div className="flex items-center gap-1.5 text-xs text-text-tertiary">
          <span>v{plan?.version ?? block.version}</span>
          <span>·</span>
          <span className={status.tone}>{status.label}</span>
        </div>
      </div>
      <Button
        size="sm"
        variant={pending ? "default" : "outline"}
        className="h-8"
        onClick={() => useLayout.getState().openInspector("plan")}
      >
        {pending ? "审阅计划" : "查看计划"}
        {plan?.status === "approved" && <CheckIcon className="size-3.5" />}
      </Button>
    </div>
  );
}
