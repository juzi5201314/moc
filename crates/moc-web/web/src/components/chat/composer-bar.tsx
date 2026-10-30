// 输入区：多行输入 + 斜杠命令 / 资源 URI 补全 + `!` 手动命令 + 图片（选择 / 粘贴 / 拖放）
// + 权限、引擎、计划、模型与上下文占用；运行中回车按偏好插队或中断，Ctrl+Enter 排队。

import {
  ArrowUpIcon,
  BookOpenIcon,
  FileTextIcon,
  FolderIcon,
  HistoryIcon,
  ImageIcon,
  LockIcon,
  PackageIcon,
  PaperclipIcon,
  PuzzleIcon,
  SlashIcon,
  SquareIcon,
  TerminalIcon,
} from "lucide-react";
import { nanoid } from "nanoid";
import {
  type ClipboardEvent,
  type DragEvent,
  type KeyboardEvent,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { toast } from "sonner";
import {
  Attachment,
  AttachmentPreview,
  AttachmentRemove,
  Attachments,
} from "@/components/ai-elements/attachments";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { leadSelector, parseSelector } from "@/lib/models";
import { BUILTIN_COMMANDS, runSlash, type SlashCommand, slashMatches } from "@/lib/slash";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  ImageSource,
  PluginListResult,
  ResourceCandidate,
  SteerResult,
} from "@/protocol/types";
import { dispatchNext, runBash, sendPrompt } from "@/store/actions";
import {
  type Attachment as ComposerAttachment,
  type PendingItem,
  toInput,
  useComposer,
} from "@/store/composer";
import { selectModels, useConnection } from "@/store/connection";
import { reacquire } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import {
  CHIP,
  ContextRing,
  PendingList,
  PlanToggle,
  SessionOptions,
  type Suggestion,
  SuggestionMenu,
  TodoTray,
} from "./composer-parts";
import { ModelPicker } from "./model-select-popover";

const EMPTY_PENDING: PendingItem[] = [];
const MAX_HEIGHT = 320;

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function imageUrl(source: ImageSource): string {
  if ("Url" in source) return source.Url;
  return `data:${source.Base64.media_type};base64,${source.Base64.data}`;
}

function readImage(file: File): Promise<ComposerAttachment> {
  const { promise, resolve, reject } = Promise.withResolvers<ComposerAttachment>();
  const reader = new FileReader();
  reader.onload = () => {
    const match =
      typeof reader.result === "string" ? /^data:([^;]+);base64,(.*)$/s.exec(reader.result) : null;
    if (!match) {
      reject(new Error(file.name));
      return;
    }
    resolve({
      id: nanoid(),
      name: file.name || "粘贴的图片",
      source: { Base64: { media_type: match[1], data: match[2] } },
    });
  };
  reader.onerror = () => reject(new Error(file.name));
  reader.readAsDataURL(file);
  return promise;
}

/** 光标所在的空白分隔 token；以 `//`、`scheme:/` 或含 `://` 时视为资源 URI 前缀（与 TUI 一致）。 */
function resourceToken(
  text: string,
  cursor: number,
): { prefix: string; start: number; end: number } | null {
  const before = text.slice(0, cursor);
  const start =
    Math.max(before.lastIndexOf(" "), before.lastIndexOf("\n"), before.lastIndexOf("\t")) + 1;
  const tail = text.slice(cursor).search(/\s/);
  const end = tail < 0 ? text.length : cursor + tail;
  const prefix = text.slice(start, cursor);
  const isUri = prefix === "//" || prefix.endsWith(":/") || prefix.includes("://");
  return isUri ? { prefix, start, end } : null;
}

const RESOURCE_ICON: Record<string, typeof FileTextIcon> = {
  artifact: PackageIcon,
  history: HistoryIcon,
  local: FileTextIcon,
  skill: BookOpenIcon,
  namespace: FolderIcon,
};

/** 当前工作区插件注册的斜杠命令；每个会话只拉一次。 */
const pluginCommandCache = new Map<string, Promise<SlashCommand[]>>();

function loadPluginCommands(sessionId: string): Promise<SlashCommand[]> {
  let cached = pluginCommandCache.get(sessionId);
  if (!cached) {
    cached = rpc
      .call<PluginListResult>("plugin/list", { session_id: sessionId })
      .then((result) =>
        result.plugins.flatMap((plugin) =>
          plugin.error
            ? []
            : plugin.commands.map((command) => ({
                name: command.name,
                description: command.description,
                usage: command.usage || undefined,
                plugin: plugin.name,
                needsSession: true,
              })),
        ),
      )
      .catch(() => {
        pluginCommandCache.delete(sessionId);
        return [];
      });
    pluginCommandCache.set(sessionId, cached);
  }
  return cached;
}

type ComposerBarProps = {
  sessionId: string;
  /** 欢迎页：卡片内嵌在居中列里，不套底部 dock。 */
  inline?: boolean;
};

export function ComposerBar({ sessionId, inline = false }: ComposerBarProps) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [cursor, setCursor] = useState(0);
  const [reading, setReading] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  /** Esc 关闭补全时记下当时的输入，内容变化前不再弹出。 */
  const [dismissedAt, setDismissedAt] = useState<string | null>(null);
  const [resources, setResources] = useState<{ prefix: string; items: ResourceCandidate[] }>({
    prefix: "",
    items: [],
  });
  const [pluginCommands, setPluginCommands] = useState<SlashCommand[]>([]);

  const view = useTranscript((state) => state.bySession[sessionId]);
  const focusCount = useUi((state) => state.focusComposer);
  const draft = useComposer((state) => state.drafts[sessionId]);
  const pending = useComposer((state) => state.pending[sessionId] ?? EMPTY_PENDING);
  const runningEnter = usePrefs((state) => state.runningEnter);
  const models = useConnection(selectModels);

  const text = draft?.text ?? "";
  const images = draft?.images ?? [];
  const isDraft = sessionId === "draft-session";
  const running = Boolean(view?.running);
  const readOnly = Boolean(view?.readOnly);
  const bashMode = text.startsWith("!");
  const model = parseSelector(leadSelector(view?.info.model ?? ""), models).model;
  // 模型没声明 inputs 时不拦，交给 core 校验。
  const acceptsImages = !model?.inputs || model.inputs.includes("image");

  const todos = useMemo(() => (view?.todos ?? []).flatMap((phase) => phase.items), [view?.todos]);
  const context = view?.context;

  // ---------- 补全 ----------

  const token = resourceToken(text, cursor);
  const tokenPrefix = token?.prefix ?? "";
  const commands = useMemo(() => {
    const builtin = new Set(BUILTIN_COMMANDS.map((command) => command.name));
    return [...BUILTIN_COMMANDS, ...pluginCommands.filter((command) => !builtin.has(command.name))];
  }, [pluginCommands]);
  const slashItems = slashMatches(text, commands);
  const slashMode = text.startsWith("/");

  useEffect(() => {
    if (isDraft || !slashMode) return;
    let alive = true;
    void loadPluginCommands(sessionId).then((list) => alive && setPluginCommands(list));
    return () => {
      alive = false;
    };
  }, [isDraft, sessionId, slashMode]);

  useEffect(() => {
    if (!tokenPrefix || isDraft) return;
    let alive = true;
    const timer = window.setTimeout(() => {
      rpc
        .call<{ candidates: ResourceCandidate[] }>("resource/complete", {
          session_id: sessionId,
          prefix: tokenPrefix,
        })
        .then((result) => alive && setResources({ prefix: tokenPrefix, items: result.candidates }))
        .catch(() => alive && setResources({ prefix: tokenPrefix, items: [] }));
    }, 120);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [isDraft, sessionId, tokenPrefix]);

  const resourceItems = token && resources.prefix === tokenPrefix ? resources.items : [];
  const menu: { heading: string; items: Suggestion[] } | null =
    dismissedAt === text
      ? null
      : slashItems.length > 0
        ? {
            heading: "命令",
            items: slashItems.map((command) => ({
              key: `/${command.name}`,
              title: `/${command.name}`,
              hint: command.usage,
              detail: command.plugin
                ? `${command.description} · ${command.plugin}`
                : command.description,
              icon: command.plugin ? <PuzzleIcon /> : <SlashIcon />,
            })),
          }
        : resourceItems.length > 0
          ? {
              heading: "资源",
              items: resourceItems.map((candidate) => {
                const Icon = RESOURCE_ICON[candidate.kind] ?? FileTextIcon;
                return {
                  key: candidate.uri,
                  title: candidate.label || candidate.uri,
                  detail: candidate.description,
                  icon: <Icon />,
                };
              }),
            }
          : null;
  const menuSize = menu?.items.length ?? 0;

  // biome-ignore lint/correctness/useExhaustiveDependencies: 候选集合变化（数量或类别）时把高亮复位到首项
  useEffect(() => {
    setActiveIndex(0);
  }, [menuSize, menu?.heading]);

  // ---------- 输入框 ----------

  useEffect(() => {
    if (focusCount > 0) textareaRef.current?.focus();
  }, [focusCount]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: 以 text 作为重测高度的信号
  useEffect(() => {
    const element = textareaRef.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, MAX_HEIGHT)}px`;
  }, [text]);

  // 插队被 core 吸收后从待发区移除。
  useEffect(
    () =>
      rpc.on("session/update", ({ session_id: updated, update }) => {
        if (updated !== sessionId || update.type !== "steering_applied") return;
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

  const setText = (value: string, caret?: number) => {
    useComposer.getState().setText(sessionId, value);
    if (caret !== undefined) {
      setCursor(caret);
      requestAnimationFrame(() => {
        const element = textareaRef.current;
        if (!element) return;
        element.focus();
        element.setSelectionRange(caret, caret);
      });
    }
  };

  const pick = (index: number) => {
    if (!menu) return;
    if (menu.heading === "命令") {
      const command = slashItems[index];
      if (!command) return;
      if (command.usage) setText(`/${command.name} `, command.name.length + 2);
      else if (runSlash(sessionId, `/${command.name}`, commands)) setCursor(0);
      return;
    }
    const candidate = resourceItems[index];
    if (!candidate || !token) return;
    const suffix = candidate.append_space ? " " : "";
    const next = `${text.slice(0, token.start)}${candidate.uri}${suffix}${text.slice(token.end)}`;
    setText(next, token.start + candidate.uri.length + suffix.length);
  };

  // ---------- 图片 ----------

  const addFiles = async (files: File[]) => {
    const supported = files.filter((file) => file.type.startsWith("image/"));
    if (supported.length < files.length) toast.error("只支持图片附件");
    if (supported.length === 0) return;
    if (!acceptsImages) {
      toast.error("当前模型不支持图片输入");
      return;
    }
    setReading(true);
    try {
      const results = await Promise.allSettled(supported.map(readImage));
      for (const result of results) {
        if (result.status === "fulfilled") useComposer.getState().addImage(sessionId, result.value);
      }
      const failed = results.filter((result) => result.status === "rejected").length;
      if (failed > 0) toast.error(`${failed} 张图片读取失败`);
    } finally {
      setReading(false);
    }
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = [...event.clipboardData.files];
    if (files.length === 0) return;
    event.preventDefault();
    void addFiles(files);
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    setDragging(false);
    const files = [...event.dataTransfer.files];
    if (files.length === 0) return;
    event.preventDefault();
    void addFiles(files);
  };

  // ---------- 发送 ----------

  const canSend = (text.trim().length > 0 || images.length > 0) && !readOnly && !reading;

  const send = (forceQueue = false) => {
    const trimmed = text.trim();
    if (!canSend) return;

    if (bashMode && images.length === 0) {
      const exclude = trimmed.startsWith("!!");
      const command = trimmed.slice(exclude ? 2 : 1).trim();
      if (!command) return;
      if (running) {
        toast("正在生成，命令请稍后再执行");
        return;
      }
      useComposer.getState().clearDraft(sessionId);
      void runBash(sessionId, command, exclude);
      return;
    }
    if (images.length === 0 && runSlash(sessionId, trimmed, commands)) return;

    const input = toInput(trimmed, images);
    useComposer.getState().clearDraft(sessionId);
    if (!running) {
      void sendPrompt(sessionId, input);
      return;
    }

    const queue = forceQueue || runningEnter === "interrupt";
    const item: PendingItem = {
      key: nanoid(),
      kind: queue ? "queue" : "steer",
      text: trimmed,
      images,
    };
    useComposer.getState().pushPending(sessionId, item);
    if (queue) {
      dispatchNext(sessionId);
      if (!forceQueue) {
        void rpc
          .call("session/cancel", { session_id: sessionId })
          .catch((error) => toast.error(`中断失败，消息仍在队列中：${errorText(error)}`));
      }
      return;
    }
    void rpc
      .call<SteerResult>("session/steer", { session_id: sessionId, input })
      .then((result) => {
        const composer = useComposer.getState();
        if (!result.accepted) {
          composer.updatePending(sessionId, item.key, { kind: "queue" });
          dispatchNext(sessionId);
          toast("本轮已不能补充，消息转入队列");
        } else if (result.id !== undefined) {
          composer.updatePending(sessionId, item.key, { steerId: result.id });
        }
      })
      .catch((error) => {
        const composer = useComposer.getState();
        if (composer.pending[sessionId]?.some((entry) => entry.key === item.key)) {
          composer.removePending(sessionId, item.key);
          const current = composer.drafts[sessionId];
          if (!current?.text && !current?.images.length)
            composer.restore(sessionId, { text: trimmed, images });
        }
        toast.error(`补充发送失败：${errorText(error)}`);
      });
  };

  const stop = () =>
    void rpc
      .call("session/cancel", { session_id: sessionId })
      .catch((error) => toast.error(`停止失败：${errorText(error)}`));

  const removePending = async (item: PendingItem) => {
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
      else toast("补充已进入上下文，无法撤回");
    } catch (error) {
      toast.error(`撤回失败：${errorText(error)}`);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.nativeEvent.isComposing) return;
    if (menu && menuSize > 0) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        setActiveIndex((index) => (index + step + menuSize) % menuSize);
        return;
      }
      if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey)) {
        event.preventDefault();
        pick(activeIndex);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setDismissedAt(text);
        return;
      }
    }
    if (event.key === "Escape" && running && !text) {
      event.preventDefault();
      stop();
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      send(event.ctrlKey || event.metaKey);
    }
  };

  const syncCursor = () => setCursor(textareaRef.current?.selectionStart ?? 0);

  // ---------- 渲染 ----------

  if (readOnly) {
    const holder = view?.holder;
    return (
      <ComposerDock inline={inline}>
        <div className="flex min-h-14 items-center gap-3 rounded-2xl bg-muted px-4 py-3 text-[13.5px] text-muted-foreground ring-1 ring-inset ring-border-subtle">
          <LockIcon className="size-4 shrink-0" />
          <span className="min-w-0 flex-1">
            只读打开
            {holder ? `：会话正由 ${holder.frontend}（pid ${holder.pid}）写入` : ""}
            ，可以查看但不能发送。
          </span>
          {!view?.info.parent_session_id && (
            <Button
              size="sm"
              variant="outline"
              className="h-7 rounded-lg"
              onClick={() =>
                void reacquire(sessionId).catch((error) =>
                  toast.error(`获取写权限失败：${errorText(error)}`),
                )
              }
            >
              获取写权限
            </Button>
          )}
        </div>
      </ComposerDock>
    );
  }

  const placeholder = running
    ? runningEnter === "interrupt"
      ? "回车中断当前生成并发送，Ctrl+Enter 排队"
      : "回车补充到当前轮，Ctrl+Enter 排队"
    : inline
      ? "描述任务，/ 查看命令，! 执行命令"
      : "继续对话，/ 查看命令，! 执行命令";

  return (
    <ComposerDock inline={inline}>
      <div className="relative">
        {menu && (
          <SuggestionMenu
            heading={menu.heading}
            items={menu.items}
            active={activeIndex}
            onPick={pick}
            onHover={setActiveIndex}
          />
        )}

        {/* biome-ignore lint/a11y/noStaticElementInteractions: 拖放区只承接文件，键盘用户走附件按钮 */}
        {/* biome-ignore lint/a11y/useKeyWithClickEvents: 点击空白处聚焦输入框只是鼠标便利，键盘本就在输入框内 */}
        <div
          onDragOver={(event) => {
            if (!event.dataTransfer.types.includes("Files")) return;
            event.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={onDrop}
          onClick={(event) => {
            if (event.target === event.currentTarget) textareaRef.current?.focus();
          }}
          className={cn(
            "relative rounded-[22px] bg-card shadow-[var(--elevation-composer)] transition-[box-shadow]",
            "focus-within:ring-1 focus-within:ring-inset focus-within:ring-border",
            bashMode && "ring-1 ring-inset ring-warn/50",
            dragging && "ring-2 ring-brand/40",
          )}
        >
          {(todos.length > 0 || pending.length > 0) && (
            <div className="rounded-t-[21px] bg-muted/60 px-2 py-1">
              {todos.length > 0 && <TodoTray items={todos} />}
              {pending.length > 0 && (
                <PendingList items={pending} onRemove={(item) => void removePending(item)} />
              )}
            </div>
          )}

          {bashMode && (
            <div className="flex items-center gap-1.5 px-4 pt-2.5 text-[11.5px] font-medium text-warn">
              <TerminalIcon className="size-3.5" />
              {text.startsWith("!!") ? "执行命令 · 输出不进入上下文" : "执行命令 · 输出写入上下文"}
            </div>
          )}

          {images.length > 0 && (
            <Attachments variant="inline" className="px-3 pt-3">
              {images.map((image) => (
                <Attachment
                  key={image.id}
                  data={{
                    id: image.id,
                    type: "file",
                    filename: image.name,
                    mediaType:
                      "Base64" in image.source ? image.source.Base64.media_type : "image/*",
                    url: imageUrl(image.source),
                  }}
                  onRemove={() => useComposer.getState().removeImage(sessionId, image.id)}
                >
                  <AttachmentPreview />
                  <AttachmentRemove label={`移除 ${image.name}`} />
                </Attachment>
              ))}
            </Attachments>
          )}

          <textarea
            ref={textareaRef}
            value={text}
            rows={1}
            onChange={(event) => {
              setText(event.target.value);
              setCursor(event.target.selectionStart);
            }}
            onSelect={syncCursor}
            onKeyDown={onKeyDown}
            onPaste={onPaste}
            placeholder={placeholder}
            aria-label="输入消息"
            className={cn(
              "block w-full resize-none bg-transparent px-5 pt-4 pb-1 text-[15px] leading-6 text-foreground outline-none placeholder:text-text-tertiary",
              bashMode && "font-mono text-[13.5px]",
              inline ? "min-h-[96px]" : "min-h-[64px]",
            )}
            style={{ maxHeight: MAX_HEIGHT }}
          />

          <div className="flex h-11 items-center gap-1 px-3 pb-1">
            <input
              ref={fileInputRef}
              type="file"
              accept="image/*"
              multiple
              hidden
              onChange={(event) => {
                const files = [...(event.target.files ?? [])];
                event.target.value = "";
                void addFiles(files);
              }}
            />
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  onClick={() => fileInputRef.current?.click()}
                  disabled={!acceptsImages || reading}
                  className={cn(CHIP, "w-8 justify-center px-0")}
                  aria-label="添加图片"
                >
                  {acceptsImages ? (
                    <PaperclipIcon className="size-4" />
                  ) : (
                    <ImageIcon className="size-4" />
                  )}
                </button>
              </TooltipTrigger>
              <TooltipContent side="top">
                {acceptsImages ? "添加图片，也可以直接粘贴或拖入" : "当前模型不支持图片输入"}
              </TooltipContent>
            </Tooltip>
            <SessionOptions sessionId={sessionId} />
            <PlanToggle sessionId={sessionId} />

            <div className="ml-auto flex min-w-0 items-center gap-1">
              {context && context.context_window > 0 && (
                <ContextRing used={context.total_tokens} window={context.context_window} />
              )}
              <ModelPicker sessionId={sessionId} />
              {running && !canSend ? (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      onClick={stop}
                      className="ml-1 flex size-8 items-center justify-center rounded-full bg-foreground text-background transition-transform active:scale-95"
                      aria-label="停止生成"
                    >
                      <SquareIcon className="size-3 fill-current" />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent side="top">停止 · Esc</TooltipContent>
                </Tooltip>
              ) : (
                <button
                  type="button"
                  onClick={() => send()}
                  disabled={!canSend}
                  className="ml-1 flex size-8 items-center justify-center rounded-full bg-foreground text-background transition-transform active:scale-95"
                  aria-label={running ? "补充到当前轮" : "发送"}
                >
                  <ArrowUpIcon className="size-4" strokeWidth={2.4} />
                </button>
              )}
            </div>
          </div>
        </div>
      </div>
    </ComposerDock>
  );
}

function ComposerDock({ inline, children }: { inline: boolean; children: React.ReactNode }) {
  if (inline) return <div className="w-full">{children}</div>;
  return (
    <div className="moc-chat-pane relative shrink-0 bg-background">
      {/* 渐隐遮罩：转写滚到输入框下方时不硬切 */}
      <div className="pointer-events-none absolute inset-x-0 -top-8 h-8 bg-gradient-to-t from-background to-transparent" />
      <div className="moc-chat-column pt-1 pb-5">{children}</div>
    </div>
  );
}
