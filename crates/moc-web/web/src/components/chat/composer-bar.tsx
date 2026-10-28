import {
  ArrowUpIcon,
  CheckCircle2Icon,
  CheckSquareIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  CircleIcon,
  DatabaseIcon,
  GaugeIcon,
  Loader2Icon,
  PlusIcon,
  RotateCcwIcon,
  ShieldCheckIcon,
  SquareIcon,
  XIcon,
} from "lucide-react";
import { nanoid } from "nanoid";
import { useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  Attachment,
  AttachmentInfo,
  AttachmentPreview,
  AttachmentRemove,
  Attachments,
} from "@/components/ai-elements/attachments";
import { ModelSelectPopover } from "@/components/chat/model-select-popover";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { rpc } from "@/protocol/client";
import type { ImageSource, SessionPermission, SteerResult, TodoItem } from "@/protocol/types";
import { retryLast, sendPrompt } from "@/store/actions";
import { type Attachment as ComposerAttachment, toInput, useComposer } from "@/store/composer";
import { dispatchNext } from "@/store/dispatch";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

interface ComposerBarProps {
  sessionId: string;
}

const PERMISSIONS: { id: SessionPermission; label: string; desc: string }[] = [
  { id: "full", label: "完全权限", desc: "允许工具读写系统与执行命令" },
  { id: "workspace", label: "工作区写入", desc: "仅允许修改当前工作区文件" },
  { id: "workspace_only", label: "只读保护", desc: "禁止工具自动修改任何磁盘文件" },
];

const EMPTY_PENDING: ReturnType<typeof useComposer.getState>["pending"][string] = [];

const SLASH_COMMANDS = [
  { command: "/retry", description: "重试上一次提交" },
  { command: "/clear", description: "清空当前草稿" },
];

function imageDataUrl(source: ImageSource): string {
  if ("Url" in source) return source.Url;
  return `data:${source.Base64.media_type};base64,${source.Base64.data}`;
}

function readImage(file: File): Promise<ComposerAttachment> {
  return new Promise((resolve, reject) => {
    if (!file.type.startsWith("image/")) {
      reject(new Error("仅支持图片附件"));
      return;
    }
    const reader = new FileReader();
    reader.onload = () => {
      if (typeof reader.result !== "string") {
        reject(new Error(`无法读取图片：${file.name}`));
        return;
      }
      const match = /^data:([^;]+);base64,(.*)$/s.exec(reader.result);
      if (!match) {
        reject(new Error(`无法读取图片：${file.name}`));
        return;
      }
      resolve({
        id: nanoid(),
        name: file.name,
        source: { Base64: { media_type: match[1], data: match[2] } },
      });
    };
    reader.onerror = () => reject(new Error(`无法读取图片：${file.name}`));
    reader.readAsDataURL(file);
  });
}

export function ComposerBar({ sessionId }: ComposerBarProps) {
  const [taskExpanded, setTaskExpanded] = useState(false);
  const [isReadingImages, setIsReadingImages] = useState(false);

  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const view = useTranscript((state) => state.bySession[sessionId]);
  const focusCount = useUi((state) => state.focusComposer);
  const draft = useComposer((state) => state.drafts[sessionId]);
  const pending = useComposer((state) => state.pending[sessionId] ?? EMPTY_PENDING);
  const lastInput = useComposer((state) => state.lastInput[sessionId]);
  const runningEnter = usePrefs((state) => state.runningEnter);
  const text = draft?.text ?? "";
  const images = draft?.images ?? [];

  const currentModelId = view?.info.model ?? "";
  const currentPermission = view?.info.permission ?? "full";
  const currentPermissionItem =
    PERMISSIONS.find((p) => p.id === currentPermission) ?? PERMISSIONS[0];

  const isRunning = Boolean(view?.running);
  const isReadOnly = Boolean(view?.readOnly);

  // 监听外部聚焦请求
  useEffect(() => {
    if (focusCount > 0) {
      textareaRef.current?.focus();
    }
  }, [focusCount]);

  // 任务清单数据 (从 TodoPhase 折叠提取各项 TodoItem)
  const allTodoItems = useMemo(
    (): TodoItem[] => (view?.todos ?? []).flatMap((phase) => phase.items),
    [view?.todos],
  );
  const todoDoneCount = allTodoItems.filter((t: TodoItem) => t.status === "completed").length;
  const todoActiveCount = allTodoItems.filter((t: TodoItem) => t.status === "in_progress").length;
  const todoPendingCount = allTodoItems.length - todoDoneCount - todoActiveCount;

  // 上下文 meter 只展示已从 session/context 获取的真实数据。
  const context = view?.context;
  const contextPercent =
    context && context.context_window > 0
      ? Math.min(100, Math.round((context.total_tokens / context.context_window) * 100))
      : null;

  // 轮次与步数统计
  const turnsCount = view?.blocks.filter((b) => b.kind === "user").length ?? 0;
  const stepsCount = view?.blocks.length ?? 0;
  const slashSuggestions =
    text.startsWith("/") && !text.includes(" ")
      ? SLASH_COMMANDS.filter(({ command }) => command.startsWith(text))
      : [];

  useEffect(
    () =>
      rpc.on("session/update", ({ session_id: updateSessionId, update }) => {
        if (updateSessionId !== sessionId || update.type !== "steering_applied") return;
        const item = useComposer
          .getState()
          .pending[sessionId]?.find(
            (entry) =>
              entry.kind === "steer" &&
              (entry.steerId === update.id ||
                (entry.steerId === undefined && entry.text === update.text)),
          );
        if (item) useComposer.getState().removePending(sessionId, item.key);
      }),
    [sessionId],
  );

  const handleSelectPermission = async (perm: SessionPermission) => {
    if (sessionId === "draft-session") {
      useTranscript.getState().mutate(sessionId, (v) => {
        v.info.permission = perm;
      });
      return;
    }
    try {
      await rpc.call("session/set_permission", {
        session_id: sessionId,
        permission: perm,
        clear_grants: false,
      });
      toast.success(`已切换权限为: ${PERMISSIONS.find((p) => p.id === perm)?.label}`);
    } catch (err) {
      toast.error(`切换权限失败: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  const executeSlash = (command: string): boolean => {
    if (command === "/clear") {
      useComposer.getState().clearDraft(sessionId);
      return true;
    }
    if (command === "/retry") {
      if (!lastInput) {
        toast.info("当前会话还没有可重试的输入");
        return true;
      }
      useComposer.getState().clearDraft(sessionId);
      retryLast(sessionId);
      return true;
    }
    return false;
  };

  const handleSend = (forceQueue = false) => {
    const trimmed = text.trim();
    if ((!trimmed && images.length === 0) || isReadOnly || isReadingImages) return;
    if (executeSlash(trimmed)) return;

    const input = toInput(trimmed, images);
    if (!isRunning) {
      useComposer.getState().clearDraft(sessionId);
      void sendPrompt(sessionId, input);
      return;
    }

    const queue = forceQueue || runningEnter === "interrupt";
    useComposer.getState().clearDraft(sessionId);
    const item = {
      key: nanoid(),
      kind: queue ? ("queue" as const) : ("steer" as const),
      text: trimmed,
      images,
    };
    useComposer.getState().pushPending(sessionId, item);

    if (queue) {
      dispatchNext(sessionId);
      if (!forceQueue && runningEnter === "interrupt") {
        void rpc.call("session/cancel", { session_id: sessionId }).catch((err: unknown) => {
          toast.error(
            `中断失败，消息仍在待发区: ${err instanceof Error ? err.message : String(err)}`,
          );
        });
      }
      return;
    }

    void rpc
      .call<SteerResult>("session/steer", { session_id: sessionId, input })
      .then((result) => {
        if (!result.accepted) {
          useComposer.getState().updatePending(sessionId, item.key, { kind: "queue" });
          dispatchNext(sessionId);
          toast("补充未被接受，已转入待发区");
          return;
        }
        if (result.id !== undefined) {
          useComposer.getState().updatePending(sessionId, item.key, { steerId: result.id });
        }
      })
      .catch((err: unknown) => {
        const composer = useComposer.getState();
        const stillPending = composer.pending[sessionId]?.some((entry) => entry.key === item.key);
        if (stillPending) {
          composer.removePending(sessionId, item.key);
          const currentDraft = composer.drafts[sessionId];
          if (!currentDraft?.text && !currentDraft?.images.length) {
            composer.restore(sessionId, { text: trimmed, images });
          }
        }
        toast.error(`补充发送失败: ${err instanceof Error ? err.message : String(err)}`);
      });
  };

  const handleStop = async () => {
    try {
      await rpc.call("session/cancel", { session_id: sessionId });
      toast("已停止生成");
    } catch (err) {
      toast.error(`停止失败: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      handleSend(e.ctrlKey || e.metaKey);
    }
  };

  const handlePickFiles = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files;
    if (!files || files.length === 0) return;
    const selected = [...files];
    e.target.value = "";
    const unsupported = selected.filter((file) => !file.type.startsWith("image/"));
    const supported = selected.filter((file) => file.type.startsWith("image/"));
    if (unsupported.length > 0) toast.error("附件暂不支持非图片文件，请选择图片");
    if (supported.length === 0) return;
    setIsReadingImages(true);
    try {
      const results = await Promise.allSettled(supported.map(readImage));
      const added = results.flatMap((result) =>
        result.status === "fulfilled" ? [result.value] : [],
      );
      for (const image of added) useComposer.getState().addImage(sessionId, image);
      const failed = results.length - added.length;
      if (failed > 0) toast.error(`${failed} 张图片读取失败`);
    } finally {
      setIsReadingImages(false);
    }
  };

  const handleRemovePending = async (item: (typeof pending)[number]) => {
    if (item.kind === "queue") {
      useComposer.getState().removePending(sessionId, item.key);
      return;
    }
    if (item.steerId === undefined) return;
    try {
      const result = await rpc.call<{ removed: boolean }>("session/unsteer", {
        session_id: sessionId,
        id: item.steerId,
      });
      if (result.removed) useComposer.getState().removePending(sessionId, item.key);
      else toast("补充已写入会话，无法撤回");
    } catch (err) {
      toast.error(`撤回失败: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  return (
    <div className="relative border-t border-border/40 bg-background/95 px-4 pt-2 pb-3 backdrop-blur-xs select-none">
      <div className="mx-auto max-w-4xl space-y-2">
        {/* 1. 任务清单折叠条 (对齐 dsh := 任务 X已完成 ^) */}
        {allTodoItems.length > 0 && (
          <div className="rounded-t-lg border border-b-0 border-border bg-muted/40 text-xs overflow-hidden">
            <button
              type="button"
              onClick={() => setTaskExpanded((v) => !v)}
              className="flex h-8 w-full items-center justify-between px-3 text-muted-foreground hover:text-foreground transition-colors"
            >
              <div className="flex items-center gap-2">
                <CheckSquareIcon className="size-3.5 text-primary" />
                <span className="font-medium text-foreground">任务</span>
                <span className="text-[11px] text-muted-foreground">
                  {todoDoneCount} 已完成
                  {todoActiveCount > 0 ? ` · ${todoActiveCount} 进行中` : ""}
                  {todoPendingCount > 0 ? ` · ${todoPendingCount} 待办` : ""}
                </span>
              </div>
              <span className="text-muted-foreground">
                {taskExpanded ? (
                  <ChevronDownIcon className="size-3.5" />
                ) : (
                  <ChevronUpIcon className="size-3.5" />
                )}
              </span>
            </button>

            {taskExpanded && (
              <ul className="max-h-40 overflow-y-auto divide-y divide-border/30 border-t border-border/50 bg-background/60 p-1.5 space-y-0.5">
                {allTodoItems.map((item) => (
                  <li
                    key={item.task}
                    className="flex items-center gap-2 px-2 py-1 text-xs text-foreground/90"
                  >
                    {item.status === "completed" ? (
                      <CheckCircle2Icon className="size-3.5 text-emerald-500 shrink-0" />
                    ) : item.status === "in_progress" ? (
                      <Loader2Icon className="size-3.5 text-primary shrink-0 animate-spin" />
                    ) : (
                      <CircleIcon className="size-3 text-muted-foreground/40 shrink-0" />
                    )}
                    <span
                      className={`truncate ${
                        item.status === "completed" ? "line-through text-muted-foreground" : ""
                      }`}
                    >
                      {item.task}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}

        {pending.length > 0 && (
          <ul className="space-y-1 rounded-md border border-border bg-muted/30 p-2">
            {pending.map((item) => (
              <li key={item.key} className="flex min-w-0 items-center gap-2 text-xs">
                <span className="shrink-0 text-[10px] font-medium uppercase text-muted-foreground">
                  {item.kind === "steer" ? "补充" : "排队"}
                </span>
                <span className="min-w-0 flex-1 truncate text-foreground/90">
                  {item.text || (item.images.length > 0 ? "图片" : "")}
                  {item.images.length > 0 ? ` · ${item.images.length} 张图片` : ""}
                </span>
                <button
                  type="button"
                  onClick={() => void handleRemovePending(item)}
                  disabled={item.kind === "steer" && item.steerId === undefined}
                  className="flex size-6 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-muted hover:text-foreground disabled:opacity-40"
                  aria-label={item.kind === "steer" ? "撤回补充消息" : "移除排队消息"}
                  title={item.kind === "steer" ? "撤回补充消息" : "移除排队消息"}
                >
                  <XIcon className="size-3.5" />
                </button>
              </li>
            ))}
          </ul>
        )}

        {/* 2. 主输入框 */}
        <div className="relative rounded-xl border border-border bg-background shadow-xs focus-within:ring-1 focus-within:ring-primary/40 focus-within:border-primary/60 transition-all">
          {slashSuggestions.length > 0 && (
            <div className="absolute bottom-full left-2 z-20 mb-2 w-64 overflow-hidden rounded-md border border-border bg-popover p-1 shadow-md">
              {slashSuggestions.map(({ command, description }) => (
                <button
                  key={command}
                  type="button"
                  onClick={() => executeSlash(command)}
                  className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-xs hover:bg-muted"
                >
                  {command === "/retry" ? (
                    <RotateCcwIcon className="size-3.5 text-muted-foreground" />
                  ) : (
                    <XIcon className="size-3.5 text-muted-foreground" />
                  )}
                  <span className="font-mono">{command}</span>
                  <span className="ml-auto truncate text-muted-foreground">{description}</span>
                </button>
              ))}
            </div>
          )}

          {images.length > 0 && (
            <Attachments variant="inline" className="px-3 pt-2">
              {images.map((image) => (
                <Attachment
                  key={image.id}
                  data={{
                    id: image.id,
                    type: "file",
                    filename: image.name,
                    mediaType:
                      "Base64" in image.source ? image.source.Base64.media_type : "image/*",
                    url: imageDataUrl(image.source),
                  }}
                  onRemove={() => useComposer.getState().removeImage(sessionId, image.id)}
                >
                  <AttachmentPreview />
                  <AttachmentInfo />
                  <AttachmentRemove label={`移除 ${image.name}`} />
                </Attachment>
              ))}
            </Attachments>
          )}

          <textarea
            ref={textareaRef}
            rows={3}
            value={text}
            onChange={(e) => useComposer.getState().setText(sessionId, e.target.value)}
            onKeyDown={handleKeyDown}
            disabled={isReadOnly}
            placeholder="发消息或创建任务，输入 / 查看指令"
            className="w-full resize-none bg-transparent px-3.5 pt-3 pb-2 text-sm outline-none placeholder:text-muted-foreground/60 leading-relaxed font-normal"
          />

          {/* 底部内嵌工具条 */}
          <div className="flex h-10 items-center justify-between px-2.5 pb-1">
            <div className="flex items-center gap-1.5">
              <input ref={fileInputRef} type="file" multiple hidden onChange={handlePickFiles} />
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={() => fileInputRef.current?.click()}
                    className="flex size-7 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
                    aria-label="添加附件或文件"
                  >
                    <PlusIcon className="size-4" />
                  </button>
                </TooltipTrigger>
                <TooltipContent side="top">添加附件或文件</TooltipContent>
              </Tooltip>

              {/* 权限选择器 (完全权限 ⌵) */}
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    className="inline-flex h-7 items-center gap-1.5 rounded-md px-2 py-1 text-xs font-medium text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
                  >
                    <ShieldCheckIcon className="size-3.5 text-emerald-500" />
                    <span>{currentPermissionItem.label}</span>
                    <ChevronDownIcon className="size-3 opacity-60" />
                  </button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="start" side="top" className="w-56 text-xs">
                  <DropdownMenuLabel className="text-[11px] font-medium text-muted-foreground">
                    权限预设级别
                  </DropdownMenuLabel>
                  {PERMISSIONS.map((p) => (
                    <DropdownMenuItem
                      key={p.id}
                      onClick={() => void handleSelectPermission(p.id)}
                      className="flex flex-col items-start py-1.5"
                    >
                      <div className="flex items-center justify-between w-full">
                        <span className="font-medium">{p.label}</span>
                        {currentPermission === p.id && (
                          <span className="size-1.5 rounded-full bg-primary" />
                        )}
                      </div>
                      <span className="text-[10px] text-muted-foreground">{p.desc}</span>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
              </DropdownMenu>
            </div>

            {/* 右侧：模型选择框 (带 Max 徽标) + 圆形蓝色发送/停止按钮 */}
            <div className="flex items-center gap-2">
              <ModelSelectPopover sessionId={sessionId} currentModelId={currentModelId} />

              {isRunning ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={handleStop}
                      className="flex size-8 items-center justify-center rounded-full bg-primary text-primary-foreground shadow-xs hover:bg-primary/90 transition-transform active:scale-95"
                      aria-label="停止当前生成"
                    >
                      <SquareIcon className="size-3.5 fill-current" />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent side="top">停止 (Stop)</TooltipContent>
                </Tooltip>
              ) : (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={() => handleSend()}
                      disabled={
                        (!text.trim() && images.length === 0) || isReadOnly || isReadingImages
                      }
                      className="flex size-8 items-center justify-center rounded-full bg-primary text-primary-foreground shadow-xs hover:bg-primary/90 transition-transform active:scale-95 disabled:opacity-40 disabled:cursor-not-allowed"
                      aria-label="发送消息"
                    >
                      <ArrowUpIcon className="size-4 stroke-[2.5]" />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent side="top">发送 (Enter)</TooltipContent>
                </Tooltip>
              )}
            </div>
          </div>
        </div>

        {/* 3. 会话状态与真实上下文用量 */}
        <div className="flex items-center justify-between px-1 text-[11px] text-muted-foreground">
          <span className="inline-flex items-center gap-1.5">
            <GaugeIcon className="size-3 opacity-70" />
            <span>
              {turnsCount > 0 ? `${turnsCount} 轮 ${stepsCount} 步` : "未开始"}
              {isRunning ? " · 处理中" : ""}
            </span>
          </span>

          {context && contextPercent !== null && (
            <div className="flex min-w-0 items-center gap-2">
              <DatabaseIcon className="size-3 shrink-0 opacity-70" />
              <span className="font-mono text-[10px]">
                {(context.total_tokens / 1000).toFixed(1)}k /{" "}
                {(context.context_window / 1000).toFixed(0)}k tok
              </span>
              <meter
                className={
                  contextPercent > 80 ? "h-1.5 w-16 accent-amber-500" : "h-1.5 w-16 accent-primary"
                }
                min={0}
                max={100}
                value={contextPercent}
                aria-label="上下文用量"
              />
              <span className="font-mono text-[10px]">{contextPercent}%</span>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
