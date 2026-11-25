// 对话流：用户气泡 + 助手轮次（按时间排列的思考/工具/正文片段）+ 末尾的授权与提问卡片。
// 正文把过程切成一个个「工作了」段：进行中的轮次收起已结束的段、当前段保持展开；
// 完成的轮次再整体收进外层「工作了」，只露出最后一段正文。

import { observeElementRect, useVirtualizer } from "@tanstack/react-virtual";
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
} from "lucide-react";
import { type MouseEvent, memo, type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { useStickToBottomContext } from "use-stick-to-bottom";
import { useShallow } from "zustand/react/shallow";
import {
  Attachment,
  AttachmentInfo,
  AttachmentPreview,
  Attachments,
} from "@/components/ai-elements/attachments";
import {
  Conversation,
  ConversationContent,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation";
import { MessageResponse } from "@/components/ai-elements/message";
import { Shimmer } from "@/components/ai-elements/shimmer";
import {
  ActivityGroup,
  AgentResultCard,
  imageSrc,
  JobNoticeCard,
  toggleKeepingHeader,
  turnDiffTotal,
} from "@/components/chat/activity";
import { ImagePreviewDialog } from "@/components/chat/image-preview-dialog";
import { RefText } from "@/components/chat/ref-text";
import { AskCard, PermissionCard } from "@/components/chat/requests";
import { IconButton } from "@/components/common/icon-button";
import { DiffStat } from "@/components/panels/diff-file";
import { Button } from "@/components/ui/button";
import { FileTypeIcon, RefKindIcon } from "@/lib/file-icon";
import { clockTime, formatDuration as formatElapsed } from "@/lib/format";
import { type MsgKey, t, useT } from "@/lib/i18n";
import { modelName } from "@/lib/models";
import { type Block, finishLabel } from "@/lib/transcript-model";
import {
  formatDuration,
  pinnedSegment,
  type Segment,
  summarizeActivity,
  type Turn,
  type WorkRun,
  workDurationMs,
  workParts,
} from "@/lib/turn";
import { cn } from "@/lib/utils";
import type { AskParams, ImageSource, PermissionRequestParams } from "@/protocol/types";
import { retrySession } from "@/store/actions";
import { useComposer } from "@/store/composer";
import { openFile, revealDir } from "@/store/files";
import { loadOlder, redo, rewind } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { useTurn, useTurnIds } from "@/store/turns";

const EMPTY_PERMISSIONS: PermissionRequestParams[] = [];
const EMPTY_ASKS: AskParams[] = [];
/** 用户消息超过这么多字符或行数时默认折叠。 */
const CLAMP_CHARS = 700;
const CLAMP_LINES = 14;

type TurnContext = {
  sessionId: string;
  readOnly: boolean;
  running: boolean;
  widgets: boolean;
};

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 回退后给一个「撤销」入口：core 把截掉的尾部留在内存里，`session/redo` 能原样接回。 */
function rewindTo(sessionId: string, entryId: string, restoreText?: string): void {
  rewind(sessionId, entryId, restoreText)
    .then((redoId) => {
      toast(t("th.rewound"), {
        description: t("th.rewound_desc"),
        action: redoId
          ? {
              label: t("th.undo"),
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
                    toast.success(t("th.restored"));
                  })
                  .catch((error) => toast.error(t("th.undo_failed", { error: errorText(error) }))),
            }
          : undefined,
      });
    })
    .catch((error) => toast.error(t("th.rewind_failed", { error: errorText(error) })));
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
  const view = useTranscript(
    useShallow((state) => {
      const view = state.bySession[sessionId];
      return view
        ? {
            readOnly: view.readOnly,
            running: view.running,
            hasMore: view.hasMore,
            loadingOlder: view.loadingOlder,
            mismatch: view.mismatch,
            liveStartedAt: view.live?.startedAt,
          }
        : undefined;
    }),
  );
  const permissions = useTranscript((state) => state.permissions[sessionId]) ?? EMPTY_PERMISSIONS;
  const asks = useTranscript((state) => state.asks[sessionId]) ?? EMPTY_ASKS;
  const turnIds = useTurnIds(sessionId);
  const widgets = usePrefs((state) => state.widgets);
  const context: TurnContext = useMemo(
    () => ({
      sessionId,
      readOnly: Boolean(view?.readOnly),
      running: Boolean(view?.running),
      widgets,
    }),
    [sessionId, view?.readOnly, view?.running, widgets],
  );
  if (!view) return null;

  return (
    <div className="moc-chat-pane relative flex min-h-0 flex-1 overflow-hidden">
      {/* key 按会话重挂：换会话时虚拟器与贴底状态都从零开始，不会沿用旧滚动位置。 */}
      <Conversation key={sessionId} className="flex-1">
        <TranscriptRows
          sessionId={sessionId}
          view={view}
          context={context}
          turnIds={turnIds}
          permissions={permissions}
          asks={asks}
        />
        <ConversationScrollButton className="bottom-4 z-30 size-8 border-0 bg-popover text-muted-foreground shadow-[var(--elevation-popover)] hover:bg-popover hover:text-foreground dark:bg-popover" />
      </Conversation>
    </div>
  );
}

type Row =
  | { kind: "older"; estimate: number }
  | { kind: "turn"; turnId: string; index: number; estimate: number }
  | { kind: "mismatch"; estimate: number }
  | { kind: "perm"; request: PermissionRequestParams; estimate: number }
  | { kind: "ask"; request: AskParams; estimate: number };

function rowKey(row: Row): string {
  switch (row.kind) {
    case "older":
      return "older";
    case "turn":
      return `turn:${row.turnId}`;
    case "mismatch":
      return "mismatch";
    case "perm":
      return `perm:${row.request.request_id}`;
    case "ask":
      return `ask:${row.request.request_id}`;
  }
}

type TranscriptView = NonNullable<ReturnType<typeof useTranscript.getState>["bySession"][string]>;

/** ChatThread 选择器取出的视图子集；只含渲染行需要的字段。 */
type RowsView = {
  hasMore: boolean;
  loadingOlder: boolean;
  mismatch: TranscriptView["mismatch"];
  liveStartedAt?: number;
};

/**
 * 虚拟化转写：只挂载视口附近的行。anchorTo "end" 让向前翻页（prepend）
 * 与条目实测高度变化都按视口锚定；底部跟随仍交给 stick-to-bottom
 * （resize="smooth" 的流式追随是既定 UX），虚拟器只在精确贴底时兜底。
 */
function TranscriptRows({
  sessionId,
  view,
  context,
  turnIds,
  permissions,
  asks,
}: {
  sessionId: string;
  view: RowsView;
  context: TurnContext;
  turnIds: readonly string[];
  permissions: readonly PermissionRequestParams[];
  asks: readonly AskParams[];
}) {
  const t = useT();
  const { scrollRef, stopScroll } = useStickToBottomContext();
  // 点开折叠的那一刻暂停虚拟器的末尾锚定，见 keepViewOnExpand。
  const [anchorEnd, setAnchorEnd] = useState(true);
  const lastIndex = turnIds.length - 1;
  const rows = useMemo<Row[]>(() => {
    const rows: Row[] = [];
    if (view.hasMore) rows.push({ kind: "older", estimate: 40 });
    turnIds.forEach((turnId, index) => {
      rows.push({ kind: "turn", turnId, index, estimate: 240 });
    });
    if (view.mismatch) rows.push({ kind: "mismatch", estimate: 36 });
    for (const request of permissions) rows.push({ kind: "perm", request, estimate: 160 });
    for (const request of asks) rows.push({ kind: "ask", request, estimate: 160 });
    return rows;
  }, [view.hasMore, view.mismatch, turnIds, permissions, asks]);

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (index) => rows[index].estimate,
    getItemKey: (index) => rowKey(rows[index]),
    anchorTo: anchorEnd ? "end" : "start",
    overscan: 5,
    gap: 32,
    paddingStart: 32,
    paddingEnd: 64,
    // jsdom 与尚未完成布局的容器都测得 0 高，此时 range=null 一行不挂；
    // 0 高时给个兜底视口，真实尺寸一到就会被覆盖。
    observeElementRect: (instance, cb) =>
      observeElementRect(instance, (rect) =>
        cb(rect.height === 0 ? { ...rect, height: 640 } : rect),
      ),
  });

  const renderRow = (row: Row): ReactNode => {
    switch (row.kind) {
      case "older":
        return <LoadOlder sessionId={sessionId} loading={view.loadingOlder} />;
      case "turn":
        return (
          <TurnView
            turnId={row.turnId}
            last={row.index === lastIndex}
            liveStartedAt={row.index === lastIndex ? view.liveStartedAt : undefined}
            context={context}
          />
        );
      case "mismatch":
        return view.mismatch ? (
          <div className="flex items-center gap-2 text-xs text-warn">
            <AlertTriangleIcon className="size-3.5" />
            {t("th.mismatch")}
            {view.mismatch.fields
              .map((field) => `${field.field} ${field.requested} → ${field.reported}`)
              .join(t("act.sep"))}
          </div>
        ) : null;
      case "perm":
        return <PermissionCard sessionId={sessionId} request={row.request} />;
      case "ask":
        return <AskCard sessionId={sessionId} request={row.request} />;
    }
  };

  // 点开折叠时内容应向下展开。贴底时有两处会把刚点的标题顶出视口：stick-to-bottom 的
  // 跟随（解除后用户回到底部自动恢复）与虚拟器的末尾锚定（只在这次展开的测量期间改锚顶部，
  // 两帧后恢复，向前翻页仍按末尾锚定）。弹出菜单的触发器（aria-haspopup）不改变正文高度。
  const anchorFrame = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (anchorFrame.current !== null) cancelAnimationFrame(anchorFrame.current);
    },
    [],
  );
  const keepViewOnExpand = (event: MouseEvent) => {
    if (
      !(event.target instanceof Element) ||
      !event.target.closest('button[aria-expanded="false"]:not([aria-haspopup])')
    ) {
      return;
    }
    stopScroll();
    setAnchorEnd(false);
    if (anchorFrame.current !== null) cancelAnimationFrame(anchorFrame.current);
    anchorFrame.current = requestAnimationFrame(() => {
      anchorFrame.current = requestAnimationFrame(() => {
        anchorFrame.current = null;
        setAnchorEnd(true);
      });
    });
  };

  return (
    <ConversationContent className="moc-chat-column gap-0 p-0" onClickCapture={keepViewOnExpand}>
      <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
        {virtualizer.getVirtualItems().map((item) => (
          <div
            key={item.key}
            data-index={item.index}
            ref={virtualizer.measureElement}
            className="absolute left-0 w-full"
            style={{ top: item.start }}
          >
            {renderRow(rows[item.index])}
          </div>
        ))}
      </div>
    </ConversationContent>
  );
}

/** 滚到顶部附近时自动翻页；prepend 的滚动锚定由虚拟器负责。 */
function LoadOlder({ sessionId, loading }: { sessionId: string; loading: boolean }) {
  const t = useT();
  const { scrollRef } = useStickToBottomContext();
  const sentinel = useRef<HTMLDivElement>(null);

  const load = () => {
    if (loading) return;
    void loadOlder(sessionId).catch((error) =>
      toast.error(t("th.history_failed", { error: errorText(error) })),
    );
  };

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
        {loading ? t("th.loading") : t("th.load_older")}
      </Button>
    </div>
  );
}

const TurnView = memo(function TurnView({
  turnId,
  last,
  liveStartedAt,
  context,
}: {
  turnId: string;
  last: boolean;
  liveStartedAt?: number;
  context: TurnContext;
}) {
  const turn = useTurn(context.sessionId, turnId);
  if (!turn) return null;
  const live = last && context.running;
  // 虚拟化后不在视口附近的行根本不挂载，contentVisibility 的占位尺寸反而干扰测量。
  return (
    <section className="flex flex-col gap-3">
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
});

const ImageThumbs = memo(function ImageThumbs({
  images,
  className,
}: {
  images: ImageSource[];
  className?: string;
}) {
  const t = useT();
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
            className="relative overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle transition-opacity hover:opacity-90"
          >
            <img
              src={imageSrc(image)}
              alt={t("th.image_attachment")}
              loading="lazy"
              decoding="async"
              className="h-24 max-w-[200px] object-cover"
            />
            {/* 正文用 `#N` 指代这张图（core 按同一顺序给模型编号） */}
            <span className="pointer-events-none absolute bottom-1 left-1 rounded bg-background/85 px-1 text-[10px] leading-4 font-medium text-foreground tabular-nums">
              #{index + 1}
            </span>
          </button>
        ))}
      </div>
      <ImagePreviewDialog
        open={preview !== null}
        src={preview}
        alt={t("th.image_preview")}
        title={t("th.image_preview")}
        onOpenChange={(open) => !open && setPreview(null)}
      />
    </>
  );
});

function UserMessage({
  block,
  context,
}: {
  block: Extract<Block, { kind: "user" }>;
  context: TurnContext;
}) {
  const t = useT();
  const long = block.text.length > CLAMP_CHARS || block.text.split("\n").length > CLAMP_LINES;
  const [expanded, setExpanded] = useState(false);
  const canEdit = Boolean(block.entryId) && !context.readOnly && !context.running;

  return (
    <div className="group/user flex flex-col items-end gap-1">
      <ImageThumbs images={block.images} className="justify-end" />
      {block.attachments && block.attachments.length > 0 && (
        <Attachments variant="inline" className="justify-end">
          {block.attachments.map((attachment) => (
            <Attachment
              // FileUIPart 要求 url；路径引用没有可预览 URL，空串让预览落到图标分支。
              data={{
                id: attachment.path,
                type: "file",
                url: "",
                filename: attachment.ref ?? attachment.path,
                mediaType: "text/plain",
              }}
              key={attachment.path}
              onClick={() =>
                attachment.kind === "dir"
                  ? // 目录没有文件页签（read_path 拒绝），改为在文件树里展开定位。
                    revealDir(context.sessionId, attachment.path)
                  : openFile(context.sessionId, { kind: "fs", path: attachment.path })
              }
              title={attachment.path}
            >
              <AttachmentPreview
                fallbackIcon={
                  attachment.kind === "dir" ? (
                    <RefKindIcon kind="dir" className="size-3" />
                  ) : (
                    <FileTypeIcon name={attachment.path} className="size-3" />
                  )
                }
              />
              <AttachmentInfo />
            </Attachment>
          ))}
        </Attachments>
      )}
      {block.text && (
        <div className="relative max-w-[92%] rounded-2xl bg-bubble px-3.5 py-2 text-chat text-foreground sm:max-w-[80%]">
          <RefText
            className={cn(
              long &&
                !expanded &&
                "max-h-[280px] overflow-hidden [mask-image:linear-gradient(to_bottom,black_75%,transparent)]",
            )}
            sessionId={context.sessionId}
            text={block.text}
            images={block.images}
            attachments={block.attachments ?? []}
          />
          {long && (
            <button
              type="button"
              onClick={() => setExpanded((value) => !value)}
              className="mt-1 text-xs font-medium text-muted-foreground hover:text-foreground"
            >
              {expanded ? t("th.collapse") : t("th.expand")}
            </button>
          )}
        </div>
      )}
      <div className="flex items-center gap-0.5 opacity-0 transition-opacity group-hover/user:opacity-100 focus-within:opacity-100 pointer-coarse:opacity-100">
        <span className="mr-1 text-2xs tabular-nums text-text-tertiary">
          {clockTime(block.atMs)}
        </span>
        <IconButton label={t("th.copy")} size="xs" onClick={() => copy(block.text, t("th.copied"))}>
          <CopyIcon />
        </IconButton>
        {canEdit && block.entryId && (
          <IconButton
            label={t("th.rewind_edit")}
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
  if (!lastBlock) return t("verb.thinking_ing");
  if (lastBlock.kind === "reasoning" && lastBlock.durationMs === undefined)
    return t("verb.thinking_ing");
  const tail = turn.segments.at(-1);
  if (tail?.kind === "text") return t("th.replying");
  return summarizeActivity(turn.activity, false);
}

/**
 * 「工作了」折叠：标题是时长与改动数，展开后列出其中的活动。外层展开时内层再嵌一级，
 * 钉住位置与行时间槽的缩进都让出外层的一级。
 */
function WorkFold({
  durationMs,
  blocks,
  nested = false,
  children,
}: {
  durationMs?: number;
  blocks: readonly Block[];
  nested?: boolean;
  children: ReactNode;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const header = useRef<HTMLDivElement>(null);
  const changes = useMemo(() => turnDiffTotal(blocks), [blocks]);
  return (
    <div>
      {/* 展开后滚动时钉在顶端，随时一键收起整段过程，不必滚回开头。 */}
      <div
        ref={header}
        className={cn("sticky bg-background", nested ? "top-7 z-[15] scroll-mt-7" : "top-0 z-20")}
      >
        <button
          type="button"
          onClick={() =>
            toggleKeepingHeader(header.current, open, () => setOpen((value) => !value))
          }
          aria-expanded={open}
          className="group/proc flex h-7 max-w-full items-center gap-1.5 rounded-md text-ui text-muted-foreground transition-colors hover:text-foreground"
        >
          <ChevronRightIcon
            className={cn(
              "size-3.5 shrink-0 motion-safe:transition-transform",
              open && "rotate-90",
            )}
          />
          <span className="shrink-0">
            {durationMs !== undefined
              ? t("th.duration", { n: formatDuration(durationMs) })
              : t("th.process")}
          </span>
          {/* 与「工作了」同字体同字号才共用基线；改动数是附注，降一档不透明度免得压过标题。 */}
          {(changes.added > 0 || changes.removed > 0) && (
            <DiffStat {...changes} className="font-sans text-ui opacity-75" />
          )}
        </button>
      </div>
      {open && (
        <div
          className={cn(
            "mt-1 ml-[7px] flex flex-col gap-2 border-l border-border-subtle pl-3",
            nested
              ? "[--moc-row-indent:2.5rem] [--moc-sticky-top:3.5rem]"
              : "[--moc-row-indent:1.25rem] [--moc-sticky-top:1.75rem]",
          )}
        >
          {children}
        </div>
      )}
    </div>
  );
}

/** 进行中轮次的状态行；每秒的计时刷新限制在这一行，不带动整轮重渲染。 */
function LiveStatus({
  sessionId,
  turn,
  startedAt,
}: {
  sessionId: string;
  turn: Turn;
  startedAt?: number;
}) {
  const t = useT();
  const now = useNow(true);
  const mcp = useTranscript((state) => state.bySession[sessionId]?.live?.mcpConnecting);
  return (
    <div className="flex h-7 items-center gap-2 text-sm">
      <span className="moc-dots flex gap-[3px]">
        <span className="size-1.5 rounded-full bg-text-tertiary" />
        <span className="size-1.5 rounded-full bg-text-tertiary" />
        <span className="size-1.5 rounded-full bg-text-tertiary" />
      </span>
      <Shimmer as="span" duration={1.8} className="font-medium">
        {mcp?.length ? t("th.mcp_connecting", { servers: mcp.join(", ") }) : liveLabel(turn)}
      </Shimmer>
      {startedAt !== undefined && (
        <span className="text-xs tabular-nums text-text-tertiary">
          {formatDuration(Math.max(0, now - startedAt))}
        </span>
      )}
    </div>
  );
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
  const t = useT();
  const segments = turn.segments;
  const parts = useMemo(() => workParts(turn), [turn]);
  const userShell = turn.activity[0]?.kind === "bash" && !turn.user;
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
      !userShell &&
      index !== finalIndex &&
      (segment.kind === "activity" || segment.kind === "text"),
  ).length;
  // 两个空文本块拼接后也有分隔换行，保持原复制入口的显示条件。
  const hasCopyText = turn.texts.length > 1 || turn.texts.some((block) => block.text.length > 0);
  const finalBlock =
    finalIndex >= 0
      ? (segments[finalIndex] as Extract<Segment, { kind: "text" }>).block
      : undefined;
  const durationMs = workDurationMs(turn, finalBlock);
  // 纯文本回复没有外层「Worked」折叠行，在回复尾注补展示本次耗时（与 TUI 对齐）。
  const replyDurationMs =
    processCount === 0
      ? (turn.footer?.durationMs ??
        (turn.footer && turn.user && turn.footer.atMs > turn.user.atMs
          ? turn.footer.atMs - turn.user.atMs
          : undefined))
      : undefined;
  const finish = turn.footer?.finishReason;
  const finishText = finishLabel(finish);
  const abnormal =
    !live &&
    last &&
    (segments.some((segment) => segment.kind === "notice" && segment.block.tone === "error") ||
      Boolean(finishText) ||
      !turn.footer);
  const commandOnly =
    turn.texts.length === 0 &&
    turn.activity.length > 0 &&
    turn.activity.every(
      (block) => block.kind === "bash" || (block.kind === "status" && block.localCommand),
    );
  const canRetry = abnormal && !commandOnly && !context.readOnly && !context.running;

  const renderSegment = (segment: Segment): ReactNode => {
    switch (segment.kind) {
      case "activity":
        return (
          <ActivityGroup
            key={segment.id}
            blocks={segment.blocks}
            live={live}
            sessionId={context.sessionId}
          />
        );
      case "text": {
        const isAnimating = live && segment === segments.at(-1);
        return (
          <MessageResponse
            key={segment.id}
            allowWidgets={context.widgets}
            sessionId={context.sessionId}
            isAnimating={isAnimating}
            mode={isAnimating ? "streaming" : "static"}
          >
            {segment.block.text}
          </MessageResponse>
        );
      }
      case "plan":
        return <PlanCard key={segment.id} sessionId={context.sessionId} block={segment.block} />;
      case "agent-result":
        return <AgentResultCard key={segment.id} block={segment.block} />;
      case "job-notice":
        return <JobNoticeCard key={segment.id} block={segment.block} />;
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

  const activityOf = (run: WorkRun): ReactNode[] =>
    run.segments.filter((segment) => segment.kind === "activity").map(renderSegment);
  const foldRun = (run: WorkRun, nested: boolean): ReactNode => (
    <WorkFold key={run.id} durationMs={run.durationMs} blocks={run.blocks} nested={nested}>
      {activityOf(run)}
    </WorkFold>
  );
  // 外层收起的是最后一段正文之外的全部内容；常显片段提到外层之外，内层只剩活动与中间正文。
  // 排除仅含常显片段的空工作段，避免在无中间正文时破坏 innerFlat、平白多套一层折叠。
  const inner = parts.filter((part) =>
    part.kind === "text" ? finalIndex < 0 || part !== segments[finalIndex] : part.blocks.length > 0,
  );
  const innerFlat = inner.length === 1 && inner[0].kind === "work";

  return (
    <div className="group/assistant flex flex-col gap-4">
      {live ? (
        // 已被正文收尾的段收起，常显片段紧随其标题；当前段还在进行，保持展开。
        parts.flatMap((part, index) => {
          if (part.kind === "text") return [renderSegment(part)];
          if (index === parts.length - 1 || part.blocks.length === 0) {
            return part.segments.map(renderSegment);
          }
          return [foldRun(part, false), ...part.segments.filter(pinnedSegment).map(renderSegment)];
        })
      ) : (
        <>
          {processCount > 0 && (
            <WorkFold durationMs={durationMs} blocks={turn.activity}>
              {innerFlat
                ? activityOf(inner[0] as WorkRun)
                : inner.map((part) =>
                    part.kind === "text"
                      ? renderSegment(part)
                      : part.blocks.length > 0 && foldRun(part, true),
                  )}
            </WorkFold>
          )}
          {segments.map((segment, index) =>
            index === finalIndex || pinnedSegment(segment) || processCount === 0
              ? renderSegment(segment)
              : null,
          )}
        </>
      )}

      {live && <LiveStatus sessionId={context.sessionId} turn={turn} startedAt={liveStartedAt} />}

      {!live && (hasCopyText || turn.footer || canRetry) && (
        <div className="flex items-center gap-0.5 opacity-0 transition-opacity group-hover/assistant:opacity-100 focus-within:opacity-100 pointer-coarse:opacity-100">
          {hasCopyText && (
            <IconButton
              label={t("th.copy_reply")}
              size="xs"
              onClick={() =>
                copy(turn.texts.map((block) => block.text).join("\n\n"), t("th.copied_reply"))
              }
            >
              <CopyIcon />
            </IconButton>
          )}
          {canRetry && (
            <IconButton
              label={t("th.retry")}
              size="xs"
              onClick={() => void retrySession(context.sessionId)}
            >
              <RotateCcwIcon />
            </IconButton>
          )}
          <span className="ml-1.5 flex items-baseline gap-1.5 text-2xs text-text-tertiary">
            {turn.footer?.model && <span>{modelName(turn.footer.model)}</span>}
            {finishText && <span className="text-warn">{finishText}</span>}
            {replyDurationMs !== undefined && (
              <span className="tabular-nums">{formatElapsed(replyDurationMs)}</span>
            )}
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
  const t = useT();
  const now = useNow(Boolean(block.retryWait));
  let text = block.text;
  if (block.retryWait) {
    const { deadline, startedAt } = block.retryWait;
    const seconds = (Math.max(0, deadline - Math.max(now, startedAt)) / 1000).toFixed(1);
    const duration = /\d+(?:\.\d+)?(?= 秒后自动重试|s \(attempt )/;
    text = duration.test(text)
      ? text.replace(duration, seconds)
      : `${text}\n${t("th.retry_in", { n: seconds })}`;
  }
  if (block.localCommand) {
    return (
      <div
        role="status"
        className={cn(
          "whitespace-pre-wrap break-words text-sm leading-relaxed",
          block.tone === "error" ? "text-destructive" : "text-text-secondary",
        )}
      >
        {text}
      </div>
    );
  }
  const error = block.tone === "error";
  return (
    <div
      className={cn(
        "flex items-start gap-2.5 rounded-lg border px-3.5 py-2.5 text-sm leading-5",
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
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{text}</span>
      {onRetry && (
        <Button
          size="sm"
          variant="outline"
          className="-my-0.5 h-7 shrink-0 bg-card text-xs"
          onClick={onRetry}
        >
          <RotateCcwIcon className="size-3.5" />
          {t("th.retry_short")}
        </Button>
      )}
    </div>
  );
}

const PLAN_STATUS: Record<string, { label: MsgKey; tone: string }> = {
  draft: { label: "planst.draft", tone: "text-text-tertiary" },
  submitted: { label: "planst.submitted", tone: "text-brand" },
  revised: { label: "planst.revised", tone: "text-warn" },
  superseded: { label: "planst.superseded", tone: "text-text-tertiary" },
  approved: { label: "planst.approved", tone: "text-success" },
  rejected: { label: "planst.rejected", tone: "text-destructive" },
};

function PlanCard({
  sessionId,
  block,
}: {
  sessionId: string;
  block: Extract<Block, { kind: "plan" }>;
}) {
  const t = useT();
  const plan = useTranscript((state) =>
    state.bySession[sessionId]?.plans.find((item) => item.plan_id === block.planId),
  );
  const status = PLAN_STATUS[plan?.status ?? "draft"];
  const pending = plan?.status === "submitted";
  return (
    <div className="flex items-center gap-3 rounded-xl bg-card px-3.5 py-3 shadow-[var(--elevation-composer-inset)]">
      <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground">
        <ListChecksIcon className="size-4" />
      </span>
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-medium">{plan?.title ?? block.title}</div>
        <div className="flex items-center gap-1.5 text-xs text-text-tertiary">
          <span>v{plan?.version ?? block.version}</span>
          <span>·</span>
          <span className={status.tone}>{t(status.label)}</span>
        </div>
      </div>
      <Button
        size="sm"
        variant={pending ? "default" : "outline"}
        className="h-8"
        onClick={() => {
          if (plan) openFile(sessionId, { kind: "resource", uri: plan.path });
        }}
      >
        {pending ? t("planst.review") : t("planst.view")}
        {plan?.status === "approved" && <CheckIcon className="size-3.5" />}
      </Button>
    </div>
  );
}
