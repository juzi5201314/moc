import {
  AlertCircleIcon,
  BrainIcon,
  ChevronDownIcon,
  CopyIcon,
  FileDiffIcon,
  FileTextIcon,
  GitForkIcon,
  InfoIcon,
  Loader2Icon,
  PencilIcon,
  SearchIcon,
  TerminalIcon,
  WrenchIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import {
  Confirmation,
  ConfirmationAction,
  ConfirmationActions,
  ConfirmationRequest,
  ConfirmationTitle,
} from "@/components/ai-elements/confirmation";
import {
  Conversation,
  ConversationContent,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation";
import { MessageResponse } from "@/components/ai-elements/message";
import {
  Question,
  QuestionActions,
  QuestionInput,
  QuestionOption,
  QuestionOptions,
  QuestionPrompt,
  type QuestionValue,
} from "@/components/ai-elements/question";
import { Shimmer } from "@/components/ai-elements/shimmer";
import { Button } from "@/components/ui/button";
import { clockTime } from "@/lib/format";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import {
  activitySnippet,
  activityVerb,
  formatDuration,
  groupTurns,
  summarizeActivity,
  type Turn,
  turnDurationMs,
} from "@/lib/turn";
import type { AskParams, PermissionRequestParams } from "@/protocol/types";
import { type RightTab, useLayout } from "@/store/layout";
import { forkSession } from "@/store/lifecycle";
import { resolveAsk, resolvePermission, useTranscript } from "@/store/transcript";

interface ChatThreadProps {
  sessionId: string;
}

type OpenRightTab = (tab: { kind: RightTab["kind"]; title?: string; path?: string }) => void;

const EMPTY_PERMISSIONS: PermissionRequestParams[] = [];
const EMPTY_ASKS: AskParams[] = [];

/** 活动行首的动作图标：与 DSH 一样只保留一个 14px 单色字形，不做彩色底板。 */
const ACTIVITY_ICON = "size-3.5 shrink-0 text-[var(--text-tertiary)]";

function activityIcon(block: Block): ReactNode {
  switch (block.kind) {
    case "reasoning":
      return <BrainIcon className={ACTIVITY_ICON} />;
    case "bash":
      return <TerminalIcon className={ACTIVITY_ICON} />;
    case "plan":
      return <FileDiffIcon className={ACTIVITY_ICON} />;
    case "status":
      return <InfoIcon className={ACTIVITY_ICON} />;
    case "tool": {
      const verb = activityVerb(block);
      if (verb === "运行命令") return <TerminalIcon className={ACTIVITY_ICON} />;
      if (verb === "读取文件") return <FileTextIcon className={ACTIVITY_ICON} />;
      if (verb === "编辑文件") return <PencilIcon className={ACTIVITY_ICON} />;
      if (verb === "搜索") return <SearchIcon className={ACTIVITY_ICON} />;
      return <WrenchIcon className={ACTIVITY_ICON} />;
    }
    default:
      return <WrenchIcon className={ACTIVITY_ICON} />;
  }
}

export function ChatThread({ sessionId }: ChatThreadProps) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const permissions = useTranscript((state) => state.permissions[sessionId]) ?? EMPTY_PERMISSIONS;
  const asks = useTranscript((state) => state.asks[sessionId]) ?? EMPTY_ASKS;
  const { openRightTab } = useLayout();

  // 运行用时时钟
  const [runningSeconds, setRunningSeconds] = useState(0);

  useEffect(() => {
    if (!view?.running) {
      setRunningSeconds(0);
      return;
    }
    const timer = setInterval(() => {
      setRunningSeconds((s) => s + 1);
    }, 1000);
    return () => clearInterval(timer);
  }, [view?.running]);

  const blocks = view?.blocks ?? [];
  const turns = useMemo(() => groupTurns(blocks), [blocks]);
  const isEmpty = blocks.length === 0;

  const handleFork = () => {
    void forkSession(sessionId);
  };

  return (
    <div className="moc-chat-pane relative flex min-h-0 flex-1 overflow-hidden">
      <Conversation className="flex-1">
        <ConversationContent className="moc-chat-column gap-8 px-0 py-6">
          {isEmpty ? (
            <div className="flex h-full flex-col items-center justify-center gap-2 py-16 text-center">
              <p className="text-sm font-medium text-foreground">有什么我可以帮你的？</p>
              <p className="text-sm text-[var(--text-tertiary)]">
                选择工作区并描述你要完成的任务。
              </p>
            </div>
          ) : (
            turns.map((turn, index) => (
              <TurnView
                key={turn.id}
                turn={turn}
                isLast={index === turns.length - 1}
                running={Boolean(view?.running)}
                openRightTab={openRightTab}
                onFork={handleFork}
              />
            ))
          )}

          {/* 运行中的实时状态：品牌色微光扫过，替代原来的 emoji 状态栏 */}
          {view?.running && (
            <div className="flex items-center gap-2 py-1 text-[13px] leading-6">
              <Loader2Icon className="size-3.5 animate-spin text-primary" />
              <Shimmer className="font-medium" duration={1.6}>
                {`深度求索中，用时 ${formatLiveDuration(runningSeconds)}`}
              </Shimmer>
            </div>
          )}

          {/* 权限审批请求 (HITL) */}
          {permissions.map((req) => (
            <Confirmation
              key={req.request_id}
              approval={{ id: req.request_id }}
              state="approval-requested"
            >
              <ConfirmationTitle>
                操作权限申请：{req.title || "工具请求调用系统能力"}
              </ConfirmationTitle>
              <ConfirmationRequest>
                <div className="space-y-2 text-sm">
                  <div className="font-medium text-foreground">{req.tool_name}</div>
                  {req.body ? (
                    <pre className="overflow-x-auto whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-xs leading-5">
                      {req.body}
                    </pre>
                  ) : null}
                </div>
              </ConfirmationRequest>
              <ConfirmationActions className="flex-wrap">
                {req.options.map((option) => (
                  <ConfirmationAction
                    key={option.id}
                    onClick={() => {
                      void resolvePermission(sessionId, req.request_id, option.id);
                    }}
                    variant={option.id === "deny" ? "outline" : "default"}
                  >
                    {option.label}
                  </ConfirmationAction>
                ))}
              </ConfirmationActions>
            </Confirmation>
          ))}

          {/* 提问卡片 */}
          {asks.map((request) => (
            <AskCard key={request.request_id} sessionId={sessionId} request={request} />
          ))}
        </ConversationContent>

        <div className="pointer-events-none absolute inset-x-0 bottom-3 z-10 flex justify-center">
          <div className="moc-chat-column flex justify-end">
            <ConversationScrollButton className="pointer-events-auto size-[34px] rounded-full border-0 bg-background text-foreground shadow-[var(--elevation-card)] hover:bg-muted" />
          </div>
        </div>
      </Conversation>
    </div>
  );
}

function formatLiveDuration(totalSec: number): string {
  const mins = Math.floor(totalSec / 60);
  const secs = totalSec % 60;
  return mins > 0 ? `${mins}分${secs}秒` : `${secs}秒`;
}

/** 一轮对话：用户气泡 + 助手侧折叠结构。 */
function TurnView({
  turn,
  isLast,
  running,
  openRightTab,
  onFork,
}: {
  turn: Turn;
  isLast: boolean;
  running: boolean;
  openRightTab: OpenRightTab;
  onFork: () => void;
}) {
  return (
    <div className="flex flex-col gap-3">
      {turn.user ? <UserBubble block={turn.user} onFork={onFork} /> : null}
      <AssistantTurn
        turn={turn}
        isLast={isLast}
        running={running}
        openRightTab={openRightTab}
        onFork={onFork}
      />
    </div>
  );
}

function UserBubble({
  block,
  onFork,
}: {
  block: Extract<Block, { kind: "user" }>;
  onFork: () => void;
}) {
  const handleCopy = () => {
    void navigator.clipboard.writeText(block.text);
    toast.success("已复制提问");
  };

  return (
    <div className="flex flex-col items-end gap-1.5">
      <div className="max-w-[80%] whitespace-pre-wrap break-words rounded-[20px] bg-[var(--bubble)] px-4 py-2.5 text-sm leading-6 text-foreground">
        {block.text || "（空消息）"}
        {block.images.length > 0 ? (
          <div className="mt-2 flex flex-wrap gap-2">
            {block.images.map((image) => (
              <img
                key={
                  "Url" in image
                    ? `${block.id}-url`
                    : `${block.id}-${image.Base64.data.slice(0, 16)}`
                }
                src={
                  "Url" in image
                    ? image.Url
                    : `data:${image.Base64.media_type};base64,${image.Base64.data}`
                }
                alt="图片附件"
                className="max-h-64 max-w-full rounded-md object-contain"
              />
            ))}
          </div>
        ) : null}
      </div>
      <div className="flex items-center gap-1 text-[11px] text-[var(--text-tertiary)]">
        <span className="font-mono">{clockTime(block.atMs)}</span>
        <IconButton label="复制提问" onClick={handleCopy}>
          <CopyIcon className="size-3.5" />
        </IconButton>
        <IconButton label="从此处分叉" onClick={onFork}>
          <GitForkIcon className="size-3.5" />
        </IconButton>
      </div>
    </div>
  );
}

function AssistantTurn({
  turn,
  isLast,
  running,
  openRightTab,
  onFork,
}: {
  turn: Turn;
  isLast: boolean;
  running: boolean;
  openRightTab: OpenRightTab;
  onFork: () => void;
}) {
  const durationMs = turnDurationMs(turn);
  const isLive = running && isLast;
  const [manualOpen, setManualOpen] = useState<boolean | null>(null);
  const open = manualOpen ?? isLive;

  const deliverableFiles = useMemo(() => collectChangedFiles(turn.activity), [turn.activity]);
  const text = turn.texts.map((block) => block.text).join("\n\n");

  const handleCopy = () => {
    void navigator.clipboard.writeText(text);
    toast.success("已复制回复");
  };

  return (
    <div className="flex flex-col">
      {turn.activity.length > 0 || durationMs !== undefined || isLive ? (
        <button
          type="button"
          onClick={() => setManualOpen(!open)}
          className="flex items-center gap-1 pb-2 text-left text-sm text-[var(--text-tertiary)] transition-colors hover:text-foreground"
        >
          <span>
            {isLive ? "深度求索中" : "已完成"}
            {durationMs !== undefined && !isLive ? `，用时 ${formatDuration(durationMs)}` : ""}
          </span>
          <ChevronDownIcon
            className={`size-3.5 shrink-0 transition-transform ${open ? "rotate-180" : ""}`}
          />
        </button>
      ) : null}

      {turn.activity.length > 0 ? (
        <>
          <div className="border-t border-border/60" />
          {open ? <ActivityList activity={turn.activity} openRightTab={openRightTab} /> : null}
        </>
      ) : null}

      {turn.texts.length > 0 ? (
        <div
          className={`text-sm leading-6 text-foreground ${turn.activity.length > 0 ? "mt-3" : ""}`}
        >
          <MessageResponse>{text}</MessageResponse>
        </div>
      ) : null}

      {deliverableFiles.length > 0 ? (
        <DeliverablesCard files={deliverableFiles} openRightTab={openRightTab} />
      ) : null}

      {text || turn.footer ? (
        <div className="mt-2 flex items-center gap-1 text-[11px] text-[var(--text-tertiary)]">
          <IconButton label="复制回复" onClick={handleCopy}>
            <CopyIcon className="size-3.5" />
          </IconButton>
          <IconButton label="从此处分叉" onClick={onFork}>
            <GitForkIcon className="size-3.5" />
          </IconButton>
          <span className="ml-1 font-mono">{turn.footer ? clockTime(turn.footer.atMs) : ""}</span>
        </div>
      ) : null}
    </div>
  );
}

/** 活动分组：本轮所有思考/命令/工具的归纳标题 + 逐行列表。 */
function ActivityList({
  activity,
  openRightTab,
}: {
  activity: Block[];
  openRightTab: OpenRightTab;
}) {
  const summary = summarizeActivity(activity);
  const [open, setOpen] = useState(false);

  return (
    <div className="flex flex-col py-1">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className="group flex h-7 items-center gap-1.5 rounded-md px-2 text-left text-sm text-[var(--text-tertiary)] transition-colors hover:bg-[var(--accent)] hover:text-foreground"
      >
        <TerminalIcon className="size-3.5 shrink-0" />
        <span className="truncate">{summary || "执行记录"}</span>
        <ChevronDownIcon
          className={`size-3 opacity-0 transition-transform group-hover:opacity-100 ${open ? "rotate-180" : ""}`}
        />
      </button>
      {open ? (
        <div className="mt-0.5 flex flex-col">
          {activity.map((block) => (
            <ActivityRow key={block.id} block={block} openRightTab={openRightTab} />
          ))}
        </div>
      ) : null}
    </div>
  );
}

/** 单条活动行：一行摘要，点开才展示正文/命令/结果。 */
function ActivityRow({ block, openRightTab }: { block: Block; openRightTab: OpenRightTab }) {
  const [open, setOpen] = useState(false);
  const verb = activityVerb(block) ?? "事件";
  const snippet = activitySnippet(block);
  const tone =
    block.kind === "status"
      ? block.tone
      : block.kind === "tool" && block.isError
        ? "error"
        : "info";
  const hasDetail =
    block.kind === "reasoning" ||
    block.kind === "bash" ||
    block.kind === "tool" ||
    block.kind === "plan" ||
    block.kind === "agent-message";

  return (
    <div className="flex flex-col">
      <button
        type="button"
        onClick={() => hasDetail && setOpen((value) => !value)}
        className={`group flex h-7 items-center gap-1.5 rounded-md px-2 text-left text-[13px] transition-colors ${
          hasDetail ? "hover:bg-[var(--accent)]" : "cursor-default"
        }`}
      >
        {activityIcon(block)}
        <span className="shrink-0 font-medium text-foreground">{verb}</span>
        {snippet ? <span className="truncate text-[var(--text-tertiary)]">{snippet}</span> : null}
        {tone === "error" ? (
          <AlertCircleIcon className="size-3.5 shrink-0 text-destructive" />
        ) : block.kind === "tool" && block.status === "running" ? (
          <Loader2Icon className="size-3 shrink-0 animate-spin text-primary" />
        ) : null}
      </button>

      {open ? <ActivityDetail block={block} openRightTab={openRightTab} /> : null}
    </div>
  );
}

function ActivityDetail({ block, openRightTab }: { block: Block; openRightTab: OpenRightTab }) {
  if (block.kind === "reasoning") {
    return (
      <p className="mt-1 mb-2 pl-8 whitespace-pre-wrap text-[13px] leading-6 text-[var(--text-tertiary)]">
        {block.text}
      </p>
    );
  }

  if (block.kind === "bash") {
    return (
      <div className="mt-1 mb-2 flex flex-col gap-1.5 pl-8">
        <CodeBlock label="命令" code={block.command} />
        {block.output ? <CodeBlock label="输出" code={block.output} /> : null}
      </div>
    );
  }

  if (block.kind === "tool") {
    const tool = block as ToolBlock;
    return (
      <div className="mt-1 mb-2 flex flex-col gap-1.5 pl-8">
        {tool.arguments ? <CodeBlock label="输入参数" code={tool.arguments} /> : null}
        {tool.result ? <CodeBlock label="执行结果" code={tool.result} /> : null}
      </div>
    );
  }

  if (block.kind === "agent-message") {
    return (
      <p className="mt-1 mb-2 pl-8 whitespace-pre-wrap text-[13px] leading-6 text-[var(--text-tertiary)]">
        {block.body}
      </p>
    );
  }

  if (block.kind === "plan") {
    return (
      <div className="mt-1 mb-2 flex items-center gap-2 pl-8 text-[13px] text-[var(--text-tertiary)]">
        <span className="truncate font-mono">{block.path}</span>
        <button
          type="button"
          onClick={() => openRightTab({ kind: "plan", title: "计划审阅" })}
          className="shrink-0 text-primary hover:underline"
        >
          查看计划
        </button>
      </div>
    );
  }

  return null;
}

/** 代码块：DSH 的形态是"顶条标签 + 复制，正文吃 muted 底"，不带外描边。 */
function CodeBlock({ label, code }: { label: string; code: string }) {
  const handleCopy = () => {
    void navigator.clipboard.writeText(code);
    toast.success("已复制");
  };

  return (
    <div className="overflow-hidden rounded-md bg-muted">
      <div className="flex h-8 items-center justify-between pl-2.5 pr-1.5">
        <span className="text-[11px] text-[var(--text-tertiary)]">{label}</span>
        <IconButton label={`复制${label}`} onClick={handleCopy}>
          <CopyIcon className="size-3.5" />
        </IconButton>
      </div>
      <pre className="max-h-80 overflow-auto border-t border-border/40 px-2.5 py-2 font-mono text-xs leading-5 text-foreground">
        {code}
      </pre>
    </div>
  );
}

function IconButton({
  label,
  onClick,
  children,
}: {
  label: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={label}
      title={label}
      className="flex size-6 items-center justify-center rounded-md text-[var(--text-tertiary)] transition-colors hover:bg-[var(--accent)] hover:text-foreground"
    >
      {children}
    </button>
  );
}

type ChangedFileItem = {
  path: string;
  hunks?: { lines?: { kind: string }[] }[];
};

function collectChangedFiles(activity: Block[]): ChangedFileItem[] {
  const files: ChangedFileItem[] = [];
  for (const block of activity) {
    if (block.kind !== "tool") continue;
    for (const file of block.diff?.files ?? []) files.push(file);
  }
  return files;
}

/** 交付物卡片：对齐 DSH 的「Edited <文件> +X -Y」窄条。 */
function DeliverablesCard({
  files,
  openRightTab,
}: {
  files: ChangedFileItem[];
  openRightTab: OpenRightTab;
}) {
  const [expanded, setExpanded] = useState(false);
  const isFoldable = files.length > 3;
  const visibleFiles = isFoldable && !expanded ? files.slice(0, 3) : files;

  return (
    <div className="mt-3 flex flex-col gap-1 rounded-xl bg-muted/60 p-2">
      {visibleFiles.map((file) => {
        let added = 0;
        let deleted = 0;
        for (const hunk of file.hunks ?? []) {
          for (const line of hunk.lines ?? []) {
            if (line.kind === "insert") added++;
            if (line.kind === "delete") deleted++;
          }
        }
        return (
          <button
            key={file.path}
            type="button"
            onClick={() => openRightTab({ kind: "doc", path: file.path })}
            className="flex h-8 items-center gap-2 rounded-lg px-2 text-left text-[13px] transition-colors hover:bg-[var(--accent)]"
          >
            <FileDiffIcon className="size-3.5 shrink-0 text-[var(--text-tertiary)]" />
            <span className="truncate font-mono text-foreground">{file.path}</span>
            <span className="ml-auto shrink-0 font-mono text-[11px] text-[var(--text-tertiary)]">
              {added > 0 ? <span className="text-success">+{added}</span> : null}
              {added > 0 && deleted > 0 ? " " : null}
              {deleted > 0 ? <span className="text-destructive">-{deleted}</span> : null}
            </span>
          </button>
        );
      })}
      {isFoldable ? (
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          className="h-7 shrink-0 self-start rounded-md px-2 text-[11px] text-[var(--text-tertiary)] hover:text-foreground"
        >
          {expanded ? "收起" : `全部 ${files.length} 个文件`}
        </button>
      ) : null}
    </div>
  );
}

function AskCard({ sessionId, request }: { sessionId: string; request: AskParams }) {
  const [answers, setAnswers] = useState<Record<string, QuestionValue>>({});

  return (
    <div className="flex flex-col gap-3">
      {request.questions.map((question) => {
        const value = answers[question.id] ?? { selectedValues: [], text: "" };
        return (
          <Question
            key={question.id}
            value={value}
            selectionMode={question.multi ? "multiple" : "single"}
            onValueChange={(next) => setAnswers((current) => ({ ...current, [question.id]: next }))}
            onSubmit={async () => {
              const complete = request.questions.map((item) => {
                const answer = answers[item.id] ?? { selectedValues: [], text: "" };
                return {
                  id: item.id,
                  selected: [...answer.selectedValues],
                  ...(answer.text.trim() ? { custom: answer.text.trim() } : {}),
                };
              });
              resolveAsk(sessionId, request.request_id, { answers: complete });
            }}
          >
            <QuestionPrompt>
              {question.header ? `${question.header}：${question.question}` : question.question}
            </QuestionPrompt>
            {question.options.length > 0 ? (
              <QuestionOptions>
                {question.options.map((option) => (
                  <QuestionOption key={option.label} value={option.label}>
                    <span className="text-left">
                      <span className="block">{option.label}</span>
                      {option.description ? (
                        <span className="block text-xs font-normal opacity-70">
                          {option.description}
                        </span>
                      ) : null}
                    </span>
                  </QuestionOption>
                ))}
              </QuestionOptions>
            ) : null}
            <QuestionInput placeholder="补充说明（可选）" />
            <QuestionActions>
              <Button size="sm" type="submit" className="text-xs">
                提交回答
              </Button>
            </QuestionActions>
          </Question>
        );
      })}
    </div>
  );
}
