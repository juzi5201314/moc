import {
  CheckCircle2Icon,
  ChevronRightIcon,
  CopyIcon,
  FileDiffIcon,
  GitForkIcon,
  Loader2Icon,
} from "lucide-react";
import { useEffect, useState } from "react";
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
  ConversationEmptyState,
} from "@/components/ai-elements/conversation";
import { Message, MessageContent, MessageResponse } from "@/components/ai-elements/message";
import {
  Question,
  QuestionActions,
  QuestionInput,
  QuestionOption,
  QuestionOptions,
  QuestionPrompt,
  type QuestionValue,
} from "@/components/ai-elements/question";
import { Reasoning, ReasoningContent, ReasoningTrigger } from "@/components/ai-elements/reasoning";
import { Terminal } from "@/components/ai-elements/terminal";
import { Tool, ToolContent } from "@/components/ai-elements/tool";
import { Button } from "@/components/ui/button";
import { CollapsibleTrigger } from "@/components/ui/collapsible";
import { clockTime } from "@/lib/format";
import type { ToolBlock } from "@/lib/transcript-model";
import type { AskParams, PermissionRequestParams } from "@/protocol/types";
import { type RightTab, useLayout } from "@/store/layout";
import { forkSession } from "@/store/lifecycle";
import { resolveAsk, resolvePermission, useTranscript } from "@/store/transcript";

interface ChatThreadProps {
  sessionId: string;
}

const EMPTY_PERMISSIONS: PermissionRequestParams[] = [];
const EMPTY_ASKS: AskParams[] = [];

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

  const handleCopyText = (content: string) => {
    void navigator.clipboard.writeText(content);
    toast.success("已复制到剪贴板");
  };

  const handleFork = () => {
    void forkSession(sessionId);
  };

  const blocks = view?.blocks ?? [];
  const isEmpty = blocks.length === 0;

  // 格式化用时：分秒
  const formatLiveDuration = (totalSec: number) => {
    const mins = Math.floor(totalSec / 60);
    const secs = totalSec % 60;
    return mins > 0 ? `${mins}分${secs}秒` : `${secs}秒`;
  };

  return (
    <div className="relative flex-1 flex overflow-hidden">
      <Conversation className="flex-1">
        <ConversationContent className="mx-auto max-w-4xl p-4 sm:p-6 gap-5">
          {isEmpty ? (
            <ConversationEmptyState
              title="有什么我可以帮你的？"
              description="选择工作区并描述你要完成的任务。"
            >
              {view?.info.cwd && (
                <div className="mt-2 inline-flex items-center gap-1.5 rounded-full border border-border bg-muted/40 px-3 py-1 text-xs text-muted-foreground font-mono">
                  <span>工作区: {view.info.cwd}</span>
                </div>
              )}
            </ConversationEmptyState>
          ) : (
            blocks.map((block) => {
              switch (block.kind) {
                // 1. 用户提问轮次
                case "user":
                  return (
                    <div key={block.id} className="group relative">
                      <Message from="user">
                        <MessageContent className="text-sm font-normal leading-relaxed">
                          {block.text ? <MessageResponse>{block.text}</MessageResponse> : null}
                          {block.images.length > 0 ? (
                            <div className="flex flex-wrap gap-2">
                              {block.images.map((image) => {
                                const imageKey =
                                  "Url" in image
                                    ? `${block.id}-url-${image.Url}`
                                    : `${block.id}-base64-${image.Base64.data.slice(0, 32)}`;
                                return "Url" in image ? (
                                  <img
                                    key={imageKey}
                                    src={image.Url}
                                    alt="图片附件"
                                    className="max-h-64 max-w-full rounded-md object-contain"
                                  />
                                ) : (
                                  <img
                                    key={imageKey}
                                    src={`data:${image.Base64.media_type};base64,${image.Base64.data}`}
                                    alt="图片附件"
                                    className="max-h-64 max-w-full rounded-md object-contain"
                                  />
                                );
                              })}
                            </div>
                          ) : null}
                        </MessageContent>
                      </Message>

                      {/* 悬停快捷按钮 */}
                      <div className="absolute right-0 -bottom-3 hidden group-hover:flex items-center gap-1 bg-background/90 backdrop-blur-xs border border-border rounded-md px-1.5 py-0.5 shadow-2xs text-[10px]">
                        <button
                          type="button"
                          onClick={() => handleCopyText(block.text)}
                          className="flex items-center gap-1 px-1 py-0.5 rounded hover:bg-muted text-muted-foreground hover:text-foreground"
                          title="复制提问"
                        >
                          <CopyIcon className="size-3" />
                          <span>复制</span>
                        </button>
                        <button
                          type="button"
                          onClick={handleFork}
                          className="flex items-center gap-1 px-1 py-0.5 rounded hover:bg-muted text-muted-foreground hover:text-foreground"
                          title="分叉新会话"
                        >
                          <GitForkIcon className="size-3" />
                          <span>分支</span>
                        </button>
                      </div>
                    </div>
                  );

                // 2. 助手正文内容
                case "text":
                  return (
                    <Message key={block.id} from="assistant">
                      <MessageContent>
                        <MessageResponse>{block.text}</MessageResponse>
                      </MessageContent>
                    </Message>
                  );

                // 3. 链式思考 (⚙ 思考 · <snippet>)
                case "reasoning": {
                  return (
                    <ReasoningDisclosure
                      key={block.id}
                      text={block.text}
                      durationMs={block.durationMs}
                    />
                  );
                }

                // 4. 终端执行 (> 运行命令 · <command>)
                case "bash": {
                  return (
                    <BashDisclosure key={block.id} command={block.command} output={block.output} />
                  );
                }

                // 5. 工具调用 (📄 读取 / ✏ 编辑 / 🔍 搜索 / :: 任务)
                case "tool": {
                  return (
                    <ToolDisclosure key={block.id} block={block} openRightTab={openRightTab} />
                  );
                }

                // 6. 轮次尾部统计与反馈栏
                case "footer": {
                  return (
                    <div
                      key={block.id}
                      className="flex items-center justify-between border-t border-border/40 pt-2 text-[11px] text-muted-foreground select-none"
                    >
                      <div className="flex items-center gap-1">
                        <button
                          type="button"
                          onClick={handleFork}
                          className="flex size-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                          title="从此处分叉会话"
                          aria-label="从此处分叉会话"
                        >
                          <GitForkIcon className="size-3.5" />
                        </button>
                      </div>

                      <div className="flex items-center gap-2 font-mono text-[11px] text-muted-foreground/70">
                        {block.durationMs ? (
                          <span>{(block.durationMs / 1000).toFixed(1)}s</span>
                        ) : null}
                        <span>{clockTime(block.atMs)}</span>
                      </div>
                    </div>
                  );
                }

                default:
                  return null;
              }
            })
          )}

          {/* 7. 深度求索中的实时动画状态栏 (✈ 深度求索中，用时 X分Y秒...) */}
          {view?.running && (
            <div className="flex items-center gap-2 py-2 text-xs text-primary font-medium select-none animate-pulse">
              <span className="text-sm">✈</span>
              <span>深度求索中，用时 {formatLiveDuration(runningSeconds)}...</span>
            </div>
          )}

          {/* 8. 权限审批请求 (HITL) */}
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
                <div className="space-y-2 text-xs">
                  <div className="font-medium text-foreground">{req.tool_name}</div>
                  {req.body ? (
                    <pre className="overflow-x-auto whitespace-pre-wrap rounded-md bg-muted/40 p-2 font-mono text-[11px]">
                      {req.body}
                    </pre>
                  ) : null}
                </div>
              </ConfirmationRequest>
              <ConfirmationActions className="flex-wrap">
                {req.options.map((option) => (
                  <ConfirmationAction
                    key={option.id}
                    onClick={() => resolvePermission(sessionId, req.request_id, option.id)}
                    variant={option.kind === "reject_once" ? "outline" : "default"}
                  >
                    {option.label}
                  </ConfirmationAction>
                ))}
              </ConfirmationActions>
            </Confirmation>
          ))}

          {/* 9. 提问交互请求 */}
          {asks.map((ask) => (
            <AskRequestCard key={ask.request_id} request={ask} sessionId={sessionId} />
          ))}
        </ConversationContent>
      </Conversation>
    </div>
  );
}

function AskRequestCard({ request, sessionId }: { request: AskParams; sessionId: string }) {
  const [answers, setAnswers] = useState<Record<string, QuestionValue>>(() =>
    Object.fromEntries(
      request.questions.map((question) => [question.id, { selectedValues: [], text: "" }]),
    ),
  );

  return (
    <div className="space-y-3">
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

/** 思考折叠卡片（AI Elements Reasoning）。 */
function ReasoningDisclosure({ text, durationMs }: { text: string; durationMs?: number }) {
  const [open, setOpen] = useState(false);
  const snippet = text.slice(0, 40).replace(/\n/g, " ") || "正在梳理思路...";
  const durationText = durationMs ? `（${Math.round(durationMs / 1000)}s）` : "";

  return (
    <Reasoning
      open={open}
      onOpenChange={setOpen}
      defaultOpen={false}
      isStreaming={false}
      duration={durationMs ? Math.round(durationMs / 1000) : undefined}
      className="mb-2 rounded-lg border border-border/70 bg-muted/20 px-3 py-2"
    >
      <ReasoningTrigger className="text-xs">
        <span className="font-medium text-foreground/80">思考</span>
        <span className="truncate text-muted-foreground">· {snippet}</span>
        {durationText ? (
          <span className="ml-auto font-mono text-[10px]">{durationText}</span>
        ) : null}
      </ReasoningTrigger>
      <ReasoningContent className="mt-2 whitespace-pre-wrap font-mono text-xs leading-relaxed">
        {text}
      </ReasoningContent>
    </Reasoning>
  );
}

/** 终端执行卡片（AI Elements Terminal）。 */
function BashDisclosure({ command, output }: { command: string; output?: string }) {
  return (
    <div className="space-y-1.5">
      <div className="font-mono text-[11px] text-muted-foreground">$ {command}</div>
      <Terminal output={output || "（无输出）"} className="rounded-lg border border-border/70" />
    </div>
  );
}

type ChangedFileItem = {
  path: string;
  hunks?: { lines?: { kind: string }[] }[];
};

type RightTabTarget = {
  kind: RightTab["kind"];
  path?: string;
  title?: string;
};

/** 工具调用卡片 (📄 读取 / ✏ 编辑 / 🔍 搜索 / :: 任务) + 变更交付物 */
function ToolDisclosure({
  block,
  openRightTab,
}: {
  block: ToolBlock;
  openRightTab: (tab: RightTabTarget) => void;
}) {
  const [open, setOpen] = useState(false);
  const diffFiles = block.diff?.files ?? [];

  // 解析工具调用描述
  let title = "工具调用";
  let icon = "⚙";
  let paramSummary = "";

  try {
    const args = JSON.parse(block.arguments || "{}");
    if (block.name.includes("read") || block.name === "read_file") {
      title = "读取";
      icon = "📄";
      paramSummary = args.path || args.file_path || "";
    } else if (
      block.name.includes("edit") ||
      block.name.includes("write") ||
      block.name === "edit_file"
    ) {
      title = "编辑";
      icon = "✏";
      paramSummary = args.path || args.file_path || "";
    } else if (block.name.includes("todo") || block.name.includes("task")) {
      title = "更新任务清单";
      icon = "::";
      paramSummary = "已更新";
    } else if (block.name.includes("search") || block.name.includes("grep")) {
      title = "搜索";
      icon = "🔍";
      paramSummary = args.query || args.pattern || args.path || "";
    } else {
      title = block.name;
      paramSummary = Object.values(args).join(", ");
    }
  } catch {
    paramSummary = block.arguments.slice(0, 30);
  }

  return (
    <div className="space-y-2">
      <Tool
        open={open}
        onOpenChange={setOpen}
        className="mb-0 rounded-lg border border-border/70 bg-muted/20 text-xs"
      >
        <CollapsibleTrigger className="flex h-9 w-full items-center justify-between gap-3 px-3 text-left text-muted-foreground hover:text-foreground">
          <span className="flex min-w-0 items-center gap-1.5 truncate">
            <ChevronRightIcon
              className={`size-3 shrink-0 transition-transform ${open ? "rotate-90" : ""}`}
            />
            <span className="font-medium text-foreground/80">
              {icon} {title}
            </span>
            <span className="text-muted-foreground/60">·</span>
            <span className="truncate font-mono text-muted-foreground">{paramSummary}</span>
          </span>
          {block.status === "running" ? (
            <Loader2Icon className="size-3 shrink-0 animate-spin text-primary" />
          ) : block.isError ? (
            <span className="text-[10px] text-destructive">失败</span>
          ) : (
            <CheckCircle2Icon className="size-3 shrink-0 text-emerald-500" />
          )}
        </CollapsibleTrigger>
        <ToolContent className="space-y-2 border-t border-border/40 bg-background/50 p-3 font-mono text-[11px]">
          {block.arguments ? (
            <div>
              <span className="font-semibold text-muted-foreground">输入参数</span>
              <pre className="mt-1 overflow-x-auto whitespace-pre-wrap rounded bg-muted/40 p-2 text-xs">
                {block.arguments}
              </pre>
            </div>
          ) : null}
          {block.result ? (
            <div>
              <span className="font-semibold text-muted-foreground">执行结果</span>
              <pre className="mt-1 overflow-x-auto whitespace-pre-wrap rounded bg-muted/40 p-2 text-xs">
                {block.result}
              </pre>
            </div>
          ) : null}
        </ToolContent>
      </Tool>

      {diffFiles.length > 0 && <DeliverablesCard files={diffFiles} openRightTab={openRightTab} />}
    </div>
  );
}

/** Deliverables 交付物卡片 (已编辑 X 个文件 +X -Y) */
function DeliverablesCard({
  files,
  openRightTab,
}: {
  files: ChangedFileItem[];
  openRightTab: (tab: RightTabTarget) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const totalFiles = files.length;
  const isFoldable = totalFiles > 4;
  const visibleFiles = isFoldable && !expanded ? files.slice(0, 4) : files;

  // 统计增删总行数
  let totalAdd = 0;
  let totalDel = 0;
  for (const f of files) {
    for (const h of f.hunks ?? []) {
      for (const l of h.lines ?? []) {
        if (l.kind === "insert") totalAdd++;
        if (l.kind === "delete") totalDel++;
      }
    }
  }

  return (
    <div className="rounded-lg border border-border bg-card p-3 text-xs space-y-2 shadow-2xs">
      <div className="flex items-center justify-between font-medium">
        <div className="flex items-center gap-1.5 text-foreground">
          <FileDiffIcon className="size-3.5 text-primary" />
          <span>已编辑 {totalFiles} 个文件</span>
          {(totalAdd > 0 || totalDel > 0) && (
            <span className="text-[11px] font-mono">
              <span className="text-emerald-500">+{totalAdd}</span>{" "}
              <span className="text-rose-500">-{totalDel}</span>
            </span>
          )}
        </div>
      </div>

      <ul className="divide-y divide-border/30 rounded border border-border/50 bg-muted/10 overflow-hidden">
        {visibleFiles.map((file) => (
          <li
            key={file.path}
            onClick={() => openRightTab({ kind: "doc", path: file.path })}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                openRightTab({ kind: "doc", path: file.path });
              }
            }}
            className="flex items-center justify-between px-2.5 py-1.5 hover:bg-muted/40 cursor-pointer transition-colors"
          >
            <span className="font-mono text-muted-foreground truncate max-w-[70%]">
              {file.path}
            </span>
            <span className="text-[10px] text-primary hover:underline">查看差异</span>
          </li>
        ))}
      </ul>

      {isFoldable && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="text-[11px] text-primary hover:underline flex items-center gap-1 pt-1"
        >
          <span>{expanded ? "收起" : `全部 ${totalFiles} 个文件 ⌵`}</span>
        </button>
      )}
    </div>
  );
}
