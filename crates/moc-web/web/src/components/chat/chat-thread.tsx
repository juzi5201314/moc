import { CopyIcon, FileDiffIcon, GitForkIcon, Loader2Icon } from "lucide-react";
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
import {
  ActivityBrowseIcon,
  ActivityChevronDownIcon,
  ActivityChevronUpIcon,
  ActivityCodeIcon,
  ActivityEditIcon,
  ActivityGlobeIcon,
  ActivityPlanIcon,
  ActivityQuestionIcon,
  ActivitySearchIcon,
  ActivitySparkleIcon,
  ActivitySubagentIcon,
  ActivityTerminalIcon,
  ActivityThinkIcon,
  ActivityWarningIcon,
} from "@/components/chat/activity-icons";
import { Button } from "@/components/ui/button";
import { clockTime } from "@/lib/format";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import {
  activityKind,
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

/**
 * 活动行首的动作图标：leading 为 16px 容器 + 右侧 6px 间距，字形随字号阶梯缩放。
 */
const LEADING =
  "relative mr-1.5 flex size-[calc(16px_+_var(--moc-n))] shrink-0 items-center justify-center";

function activityIcon(block: Block): ReactNode {
  if (block.kind === "status") return <ActivityWarningIcon />;
  switch (activityKind(block)) {
    case "thinking":
      return <ActivityThinkIcon />;
    case "commands":
      return <ActivityTerminalIcon />;
    case "code":
      return <ActivityCodeIcon />;
    case "read":
    case "readImage":
    case "webFetch":
      return <ActivityBrowseIcon />;
    case "search":
      return <ActivitySearchIcon />;
    case "webSearch":
      return <ActivityGlobeIcon />;
    case "edit":
    case "write":
      return <ActivityEditIcon />;
    case "plan":
      return <ActivityPlanIcon />;
    case "questions":
      return <ActivityQuestionIcon />;
    case "subagents":
      return <ActivitySubagentIcon />;
    default:
      return <ActivitySparkleIcon />;
  }
}

/**
 * 折叠行 leading 行为：闭合时显示活动图标、行悬停换 chevron-down；
 * 展开后恒显 chevron-up（`staticWhenOpen`）。
 */
function DisclosureLeading({
  icon,
  open,
  staticWhenOpen = false,
}: {
  icon: ReactNode;
  open: boolean;
  staticWhenOpen?: boolean;
}) {
  if (open && staticWhenOpen) {
    return (
      <span className={LEADING}>
        <ActivityChevronUpIcon />
      </span>
    );
  }
  return (
    <span className={LEADING}>
      <span className="inline-flex transition-opacity duration-100 group-hover/disclosure:opacity-0">
        {icon}
      </span>
      <span className="absolute inset-0 m-auto flex items-center justify-center opacity-0 transition-opacity duration-100 group-hover/disclosure:opacity-100">
        {open ? <ActivityChevronUpIcon /> : <ActivityChevronDownIcon />}
      </span>
    </span>
  );
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
          className="flex items-center gap-1 pb-2 text-left text-[var(--moc-secondary-size)] leading-[calc(24px_+_var(--moc-n))] text-[var(--text-tertiary)] transition-colors duration-100 hover:text-muted-foreground"
        >
          <span>
            {isLive ? "深度求索中" : "已完成"}
            {durationMs !== undefined && !isLive ? `，用时 ${formatDuration(durationMs)}` : ""}
          </span>
          <span className={`inline-flex shrink-0 transition-transform ${open ? "rotate-180" : ""}`}>
            <ActivityChevronDownIcon />
          </span>
        </button>
      ) : null}

      {turn.activity.length > 0 ? (
        <>
          <div className="border-t border-border/60" />
          {open ? (
            <ActivityList activity={turn.activity} openRightTab={openRightTab} live={isLive} />
          ) : null}
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

/** 活动分组：本轮所有思考/命令/工具的归纳标题 + 逐行列表（成员与组头同列）。 */
function ActivityList({
  activity,
  openRightTab,
  live,
}: {
  activity: Block[];
  openRightTab: OpenRightTab;
  live: boolean;
}) {
  const summary = summarizeActivity(activity, !live);
  const [open, setOpen] = useState(false);
  const firstBlock = activity[0];

  return (
    <div className="flex flex-col py-1">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className="group/disclosure flex h-[calc(24px_+_var(--moc-n))] min-w-0 items-center text-left text-[var(--moc-secondary-size)] leading-[calc(24px_+_var(--moc-n))] text-[var(--text-tertiary)] transition-colors duration-100 hover:text-muted-foreground"
      >
        <DisclosureLeading icon={activityIcon(firstBlock)} open={open} staticWhenOpen />
        <span className="min-w-0 flex-auto truncate">{summary || "执行记录"}</span>
      </button>
      {open ? (
        <div className="flex flex-col">
          {activity.map((block) => (
            <ActivityRow key={block.id} block={block} openRightTab={openRightTab} />
          ))}
        </div>
      ) : null}
    </div>
  );
}

/** 单条活动行：24px+n 行高、副档字号/400 tertiary，动词与摘要之间是 2px 圆点。 */
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
        className={`group/disclosure flex h-[calc(24px_+_var(--moc-n))] min-w-0 items-center text-left text-[var(--moc-secondary-size)] leading-[calc(24px_+_var(--moc-n))] text-[var(--text-tertiary)] transition-colors duration-100 ${
          hasDetail ? "cursor-pointer hover:text-muted-foreground" : "cursor-default"
        }`}
      >
        {hasDetail ? (
          <DisclosureLeading icon={activityIcon(block)} open={open} staticWhenOpen />
        ) : (
          <span className={LEADING}>{activityIcon(block)}</span>
        )}
        <span className="flex-none">{verb}</span>
        {snippet ? (
          <>
            <span
              aria-hidden="true"
              className="mx-2 size-0.5 flex-none rounded-[1px] bg-[var(--text-tertiary)]"
            />
            <span
              className={`min-w-0 flex-auto truncate ${tone === "error" ? "text-destructive" : ""}`}
            >
              {snippet}
            </span>
          </>
        ) : (
          <span className="min-w-0 flex-auto" />
        )}
        {block.kind === "tool" && block.status === "running" ? (
          <Loader2Icon className="ml-2 size-3 shrink-0 animate-spin text-primary" />
        ) : null}
      </button>

      {open ? <ActivityDetail block={block} openRightTab={openRightTab} /> : null}
    </div>
  );
}

function ActivityDetail({ block, openRightTab }: { block: Block; openRightTab: OpenRightTab }) {
  if (block.kind === "reasoning") {
    return (
      <p className="py-1 pl-[calc(22px_+_var(--moc-n))] whitespace-pre-wrap text-[var(--moc-secondary-size)] leading-[calc(24px_+_var(--moc-n))] text-[var(--text-tertiary)]">
        {block.text}
      </p>
    );
  }

  if (block.kind === "bash") {
    return <CommandCard command={block.command} output={block.output} isError={block.isError} />;
  }

  if (block.kind === "tool") {
    const tool = block as ToolBlock;
    const { command, cwd, code } = commandArgs(tool.arguments);
    if (command || code) {
      return (
        <CommandCard
          command={command ?? tool.name}
          cwd={cwd}
          code={code}
          output={tool.result}
          isError={tool.isError}
          running={tool.status === "running"}
        />
      );
    }
    if (!tool.arguments && !tool.result) return null;
    return <IoCard input={tool.arguments} output={tool.result} isError={tool.isError} />;
  }

  if (block.kind === "agent-message") {
    return (
      <p className="py-1 pl-[calc(22px_+_var(--moc-n))] whitespace-pre-wrap text-[var(--moc-secondary-size)] leading-[calc(24px_+_var(--moc-n))] text-[var(--text-tertiary)]">
        {block.body}
      </p>
    );
  }

  if (block.kind === "plan") {
    return (
      <div className="flex items-center gap-2 py-1 pl-[calc(22px_+_var(--moc-n))] text-[var(--moc-secondary-size)] text-[var(--text-tertiary)]">
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

/** 从工具参数里取命令行、工作目录与代码体，决定详情是否走终端卡片形态。 */
function commandArgs(raw: string): { command?: string; cwd?: string; code?: string } {
  try {
    const args = JSON.parse(raw) as Record<string, unknown>;
    return {
      command: typeof args.command === "string" ? args.command : undefined,
      cwd: typeof args.cwd === "string" ? args.cwd : undefined,
      code: typeof args.code === "string" ? args.code : undefined,
    };
  } catch {
    return {};
  }
}

/**
 * 命令详情：终端卡片——30px 状态点 gutter、18px 行高、11px 等宽字、
 * muted 底 + 发丝描边、大圆角；头部承载命令与 run_code 代码体
 * （超 150px 自滚动），输出以发丝线分隔、限高 224px。
 */
function CommandCard({
  command,
  cwd,
  code,
  output,
  isError = false,
  running = false,
}: {
  command: string;
  cwd?: string;
  code?: string;
  output?: string;
  isError?: boolean;
  running?: boolean;
}) {
  const handleCopy = () => {
    void navigator.clipboard.writeText(code ?? command);
    toast.success("已复制");
  };
  const prompt = cwd?.split(/[\\/]/).filter(Boolean).pop();

  return (
    <div className="relative my-1 ml-1 overflow-hidden rounded-[var(--radius-lg)] border-[0.5px] border-border bg-muted pl-[30px] font-mono text-[var(--moc-code-size)] leading-[18px] text-foreground">
      <div
        className={`-ml-[30px] flex max-h-[150px] items-start gap-3 overflow-y-auto py-[9px] pr-3.5 pl-[30px] ${
          output ? "border-b-[0.5px] border-border" : ""
        }`}
      >
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="relative flex min-w-0 items-baseline gap-2">
            <span className="absolute top-[9px] -left-[22px] -translate-y-1/2">
              <span
                className={`block size-1.5 rounded-full ${
                  running ? "animate-pulse bg-primary" : isError ? "bg-destructive" : "bg-success"
                }`}
              />
            </span>
            {prompt ? (
              <span className="flex-none text-[var(--text-tertiary)]">{prompt}</span>
            ) : null}
            <span className="block min-w-0 flex-auto overflow-x-auto whitespace-pre-wrap [overflow-wrap:anywhere]">
              {command}
            </span>
          </div>
          {code ? <pre className="whitespace-pre-wrap [overflow-wrap:anywhere]">{code}</pre> : null}
        </div>
        <button
          type="button"
          onClick={handleCopy}
          aria-label="复制命令"
          className="sticky top-0 flex-none text-[11px] leading-[18px] text-muted-foreground transition-colors hover:text-foreground"
        >
          复制
        </button>
      </div>
      {output ? (
        <pre className="max-h-[224px] overflow-auto py-3 pr-3.5 whitespace-pre-wrap [overflow-wrap:anywhere]">
          {output}
        </pre>
      ) : null}
    </div>
  );
}

/**
 * 非命令类工具的详情卡：单卡片内的「输入 | 输出」两栏段——
 * 左侧 caption 色标签列（粘性），右侧正文，段间 0.5px 发丝线。
 */
function IoCard({
  input,
  output,
  isError = false,
}: {
  input?: string;
  output?: string;
  isError?: boolean;
}) {
  return (
    <div className="my-1 ml-1 flex flex-col rounded-[var(--radius-lg)] border-[0.5px] border-border bg-muted font-mono text-[var(--moc-code-size)] leading-[calc(16px_+_var(--moc-code-n))]">
      {input ? (
        <div className="grid max-h-[150px] grid-cols-[max-content_1fr] gap-x-3.5 overflow-y-auto px-4 py-3">
          <span className="sticky top-0 self-start text-[var(--text-tertiary)]">输入</span>
          <span className="min-w-0 whitespace-pre-wrap break-words text-muted-foreground">
            {prettyJson(input)}
          </span>
        </div>
      ) : null}
      {input && output ? <div className="h-0 border-t-[0.5px] border-border" /> : null}
      {output ? (
        <div className="grid max-h-[150px] grid-cols-[max-content_1fr] gap-x-3.5 overflow-y-auto px-4 py-3">
          <span className="sticky top-0 self-start text-[var(--text-tertiary)]">输出</span>
          <span
            className={`min-w-0 whitespace-pre-wrap break-words ${
              isError ? "text-destructive" : "text-muted-foreground"
            }`}
          >
            {output}
          </span>
        </div>
      ) : null}
    </div>
  );
}

/** 参数文本是合法 JSON 时美化缩进，否则原样（历史回放可能被截断成非法 JSON）。 */
function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
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
