// Composer：待发区 + 自动增高输入框 + 斜杠/资源补全 + 底栏（工作区、模式、权限、模型、发送/停止）。
// 行为分流：空闲 Enter 发送；运行中 Enter 按 prefs.runningEnter 插队或打断；Ctrl+Enter 排队；
// `!` / `!!` 走 session/bash；`/` 开头走斜杠命令。

import { ArrowUp, FolderOpen, ImagePlus, ListChecks, Square, X } from "lucide-react";
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
import { IconButton } from "@/components/common/icon-button";
import { runAction } from "@/components/layout/session-actions";
import { MODE_LABEL, PERMISSION_LABEL } from "@/components/layout/session-header";
import { AskCard } from "@/components/requests/ask-card";
import { PermissionCard } from "@/components/requests/permission-card";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type {
  ResourceCandidate,
  SessionInfo,
  SessionMode,
  SessionPermission,
  SteerResult,
} from "@/protocol/types";
import { sendPrompt } from "@/store/actions";
import {
  type Attachment,
  EMPTY_DRAFT,
  type PendingItem,
  toInput,
  useComposer,
} from "@/store/composer";
import { dispatchNext } from "@/store/dispatch";
import { usePrefs } from "@/store/prefs";
import { useSessions, workspaceName } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import {
  BUILTIN_COMMANDS,
  type CommandSpec,
  listPlugins,
  pluginCommands,
  runSlash,
  setMode,
} from "./commands";
import { ModelPicker, useCurrentModel } from "./model-picker";
import { applyCompletion, parseBash, parseSlash, resourcePrefix, type TokenRange } from "./parse";

const SWITCHABLE: SessionMode[] = ["normal", "ask", "plan", "ptc"];
const PERMISSIONS: SessionPermission[] = ["full", "workspace", "workspace_only"];
const MAX_MENU = 8;

let keySeq = 0;
function nextKey(): string {
  keySeq += 1;
  return `p${keySeq}`;
}

function errorText(error: unknown): string {
  return error instanceof RpcError ? error.message : String(error);
}

function readImage(file: File): Promise<Attachment> {
  const { promise, resolve, reject } = Promise.withResolvers<Attachment>();
  const reader = new FileReader();
  reader.onerror = () => reject(reader.error);
  reader.onload = () => {
    const url = String(reader.result);
    const comma = url.indexOf(",");
    resolve({
      id: nextKey(),
      name: file.name || "image",
      source: { Base64: { media_type: file.type || "image/png", data: url.slice(comma + 1) } },
    });
  };
  reader.readAsDataURL(file);
  return promise;
}

type Menu =
  | { kind: "slash"; items: CommandSpec[] }
  | { kind: "resource"; items: ResourceCandidate[]; range: TokenRange };

export function Composer({ sessionId }: { sessionId: string }) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const draft = useComposer((state) => state.drafts[sessionId] ?? EMPTY_DRAFT);
  const pending = useComposer((state) => state.pending[sessionId]);
  const runningEnter = usePrefs((state) => state.runningEnter);
  const focusTick = useUi((state) => state.focusComposer);
  const current = useCurrentModel(sessionId);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const composing = useRef(false);
  const [menu, setMenu] = useState<Menu | null>(null);
  const [menuIndex, setMenuIndex] = useState(0);
  const [plugins, setPlugins] = useState<CommandSpec[]>([]);
  const [clmOpen, setClmOpen] = useState(false);
  const resourceSeq = useRef(0);

  const running = view?.running ?? false;
  const readOnly = view?.readOnly ?? true;
  const info = view?.info;
  const acceptsImages = current.model?.inputs?.includes("image") ?? false;

  useEffect(() => {
    let alive = true;
    listPlugins(sessionId)
      .then((list) => alive && setPlugins(pluginCommands(list)))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [sessionId]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: focusTick 变化即聚焦
  useEffect(() => {
    textarea.current?.focus();
  }, [focusTick, sessionId]);

  // 自动增高：最多 40vh（由 CSS max-height 限制）。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 文本变化后重新测量
  useEffect(() => {
    const element = textarea.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${element.scrollHeight}px`;
  }, [draft.text]);

  const todoProgress = useMemo(() => {
    const items = view?.todos.flatMap((phase) => phase.items) ?? [];
    if (items.length === 0) return null;
    const done = items.filter(
      (item) => item.status === "completed" || item.status === "abandoned",
    ).length;
    return done < items.length ? { done, total: items.length } : null;
  }, [view?.todos]);

  const store = useComposer.getState;

  const updateMenu = (text: string, cursor: number) => {
    const slash = /^\/(\S*)$/.exec(text.slice(0, cursor));
    if (slash && !text.startsWith("//")) {
      const needle = (slash[1] ?? "").toLowerCase();
      const items = [...BUILTIN_COMMANDS, ...plugins]
        .filter((command) => command.name.toLowerCase().startsWith(needle))
        .slice(0, MAX_MENU);
      setMenu(items.length ? { kind: "slash", items } : null);
      setMenuIndex(0);
      return;
    }
    const resource = resourcePrefix(text, cursor);
    if (!resource) {
      setMenu(null);
      return;
    }
    const seq = ++resourceSeq.current;
    rpc
      .call<{ candidates: ResourceCandidate[] }>("resource/complete", {
        session_id: sessionId,
        prefix: resource.prefix,
      })
      .then((result) => {
        if (seq !== resourceSeq.current) return;
        const items = result.candidates.slice(0, MAX_MENU);
        setMenu(items.length ? { kind: "resource", items, range: resource.range } : null);
        setMenuIndex(0);
      })
      .catch(() => {
        if (seq === resourceSeq.current) setMenu(null);
      });
  };

  const setText = (text: string, cursor?: number) => {
    store().setText(sessionId, text);
    const position = cursor ?? textarea.current?.selectionStart ?? text.length;
    updateMenu(text, position);
    if (cursor !== undefined) {
      requestAnimationFrame(() => textarea.current?.setSelectionRange(cursor, cursor));
    }
  };

  const pickMenu = (index: number) => {
    if (!menu) return;
    if (menu.kind === "slash") {
      const command = menu.items[index];
      if (!command) return;
      const text = `/${command.name} `;
      setMenu(null);
      setText(text, text.length);
      return;
    }
    const candidate = menu.items[index];
    if (!candidate) return;
    const next = applyCompletion(draft.text, menu.range, candidate.uri, candidate.append_space);
    setMenu(null);
    store().setText(sessionId, next.text);
    requestAnimationFrame(() => textarea.current?.setSelectionRange(next.cursor, next.cursor));
  };

  const addImages = async (files: File[]) => {
    const images = files.filter((file) => file.type.startsWith("image/"));
    if (images.length === 0) return false;
    if (!acceptsImages) {
      toast.error("当前模型不支持图片");
      return true;
    }
    for (const file of images) {
      try {
        store().addImage(sessionId, await readImage(file));
      } catch (error) {
        toast.error(`读取图片失败：${errorText(error)}`);
      }
    }
    return true;
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = [...event.clipboardData.files];
    if (files.some((file) => file.type.startsWith("image/"))) {
      event.preventDefault();
      void addImages(files);
    }
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    const files = [...event.dataTransfer.files];
    if (files.length === 0) return;
    event.preventDefault();
    void addImages(files);
  };

  const cancel = () => {
    void rpc.call("session/cancel", { session_id: sessionId }).catch(() => undefined);
  };

  const clearInput = () => {
    store().clearDraft(sessionId);
    setMenu(null);
  };

  const steer = async (item: PendingItem) => {
    store().pushPending(sessionId, item);
    try {
      const result = await rpc.call<SteerResult>("session/steer", {
        session_id: sessionId,
        input: toInput(item.text, item.images),
      });
      if (result.accepted && result.id !== undefined) {
        store().updatePending(sessionId, item.key, { steerId: result.id });
      } else {
        // 没被接受（提示刚好结束）：转成队列项，结束后自动发送。
        store().updatePending(sessionId, item.key, { kind: "queue" });
        dispatchNext(sessionId);
      }
    } catch (error) {
      store().removePending(sessionId, item.key);
      store().restore(sessionId, { text: item.text, images: item.images });
      toast.error(`插队失败：${errorText(error)}`);
    }
  };

  const submit = async (queue: boolean) => {
    if (readOnly) return;
    const text = draft.text;
    const trimmed = text.trim();
    const images = draft.images;
    if (!trimmed && images.length === 0) return;

    const slash = images.length === 0 ? parseSlash(trimmed) : null;
    if (slash) {
      clearInput();
      const known = await runSlash(sessionId, slash.name, slash.args, {
        confirmClm: () => setClmOpen(true),
        plugins,
      });
      if (!known) {
        store().restore(sessionId, { text, images: [] });
        toast.error(`未知命令：/${slash.name}`);
      }
      return;
    }

    const bash = images.length === 0 ? parseBash(trimmed) : null;
    if (bash) {
      if (running) {
        toast.error("运行中不能执行 bash 命令");
        return;
      }
      clearInput();
      void runAction(() => rpc.call("session/bash", { session_id: sessionId, command: bash }));
      return;
    }

    const item: PendingItem = { key: nextKey(), kind: "queue", text: trimmed, images };
    if (queue) {
      clearInput();
      store().pushPending(sessionId, item);
      dispatchNext(sessionId);
      return;
    }
    if (!running) {
      clearInput();
      void sendPrompt(sessionId, toInput(trimmed, images));
      return;
    }
    clearInput();
    if (runningEnter === "steer") {
      void steer({ ...item, kind: "steer" });
    } else {
      store().pushPending(sessionId, item, true);
      cancel();
    }
  };

  const recallLatest = () => {
    const list = store().pending[sessionId] ?? [];
    const latest = list.at(-1);
    if (latest) void recall(latest);
  };

  const recall = async (item: PendingItem) => {
    if (item.kind === "steer" && item.steerId !== undefined) {
      try {
        const result = await rpc.call<{ removed: boolean }>("session/unsteer", {
          session_id: sessionId,
          id: item.steerId,
        });
        if (!result.removed) {
          toast("已写入会话，无法撤回");
          return;
        }
      } catch (error) {
        toast.error(`撤回失败：${errorText(error)}`);
        return;
      }
    }
    store().removePending(sessionId, item.key);
    store().restore(sessionId, { text: item.text, images: item.images });
    textarea.current?.focus();
  };

  const discard = async (item: PendingItem) => {
    if (item.kind === "steer" && item.steerId !== undefined) {
      const result = await rpc
        .call<{ removed: boolean }>("session/unsteer", { session_id: sessionId, id: item.steerId })
        .catch(() => ({ removed: false }));
      if (!result.removed) {
        toast("已写入会话，无法撤回");
        return;
      }
    }
    store().removePending(sessionId, item.key);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (composing.current || event.nativeEvent.isComposing) return;
    if (menu) {
      if (event.key === "ArrowDown") {
        event.preventDefault();
        setMenuIndex((index) => (index + 1) % menu.items.length);
        return;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        setMenuIndex((index) => (index - 1 + menu.items.length) % menu.items.length);
        return;
      }
      if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey && !event.ctrlKey)) {
        event.preventDefault();
        pickMenu(menuIndex);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setMenu(null);
        return;
      }
    }
    if (event.key === "ArrowUp" && event.altKey) {
      event.preventDefault();
      recallLatest();
      return;
    }
    if (event.key === "Escape" && running) {
      event.preventDefault();
      cancel();
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit(event.ctrlKey || event.metaKey);
    }
  };

  if (!view || !info) return null;

  return (
    <div className="mx-auto w-full max-w-[820px] px-4 pb-4">
      <PermissionCard sessionId={sessionId} />
      <AskCard sessionId={sessionId} />
      {pending?.length ? (
        <ul aria-label="待发消息" className="mb-2 space-y-1">
          {pending.map((item) => (
            <li
              key={item.key}
              className="flex items-center gap-2 rounded-lg border bg-panel px-3 py-1.5 text-sm"
            >
              <span
                className={cn(
                  "rounded px-1.5 py-0.5 text-xs",
                  item.kind === "steer"
                    ? "bg-brand/15 text-brand"
                    : "bg-muted text-muted-foreground",
                )}
              >
                {item.kind === "steer" ? "插队" : "队列"}
              </span>
              <button
                type="button"
                onClick={() => void recall(item)}
                title="撤回到输入框"
                className="min-w-0 flex-1 truncate text-left hover:underline"
              >
                {item.text || `${item.images.length} 张图片`}
              </button>
              <IconButton label="丢弃" size="icon-xs" onClick={() => void discard(item)}>
                <X aria-hidden />
              </IconButton>
            </li>
          ))}
        </ul>
      ) : null}

      {/* biome-ignore lint/a11y/noStaticElementInteractions: 拖入图片是鼠标增强；键盘用户可粘贴或点图片按钮 */}
      <div
        onDragOver={(event) => {
          if (event.dataTransfer.types.includes("Files")) event.preventDefault();
        }}
        onDrop={onDrop}
        className={cn(
          "relative rounded-2xl border bg-card shadow-sm transition-colors focus-within:border-ring/60",
          running && "border-brand/50",
        )}
      >
        {menu ? (
          <div
            role="listbox"
            aria-label={menu.kind === "slash" ? "斜杠命令" : "资源"}
            className="absolute inset-x-2 bottom-full z-20 mb-2 overflow-hidden rounded-xl border bg-popover p-1 shadow-lg"
          >
            {menu.kind === "slash"
              ? menu.items.map((command, index) => (
                  <button
                    key={`${command.plugin ?? ""}/${command.name}`}
                    type="button"
                    role="option"
                    aria-selected={index === menuIndex}
                    onMouseDown={(event) => {
                      event.preventDefault();
                      pickMenu(index);
                    }}
                    className={cn(
                      "flex w-full items-baseline gap-3 rounded-md px-2 py-1.5 text-left text-sm",
                      index === menuIndex && "bg-accent",
                    )}
                  >
                    <span className="font-mono">{command.usage}</span>
                    <span className="truncate text-muted-foreground text-xs">
                      {command.description}
                      {command.plugin ? ` · ${command.plugin}` : ""}
                    </span>
                  </button>
                ))
              : menu.items.map((candidate, index) => (
                  <button
                    key={candidate.uri}
                    type="button"
                    role="option"
                    aria-selected={index === menuIndex}
                    onMouseDown={(event) => {
                      event.preventDefault();
                      pickMenu(index);
                    }}
                    className={cn(
                      "flex w-full items-baseline gap-3 rounded-md px-2 py-1.5 text-left text-sm",
                      index === menuIndex && "bg-accent",
                    )}
                  >
                    <span className="truncate font-mono">{candidate.label || candidate.uri}</span>
                    <span className="truncate text-muted-foreground text-xs">
                      {candidate.description}
                    </span>
                  </button>
                ))}
          </div>
        ) : null}

        {todoProgress ? (
          <button
            type="button"
            onClick={() => usePrefs.getState().setRightPanel({ open: true, tab: "todo" })}
            className="absolute -top-3 left-4 flex items-center gap-1 rounded-full border bg-card px-2 py-0.5 text-muted-foreground text-xs hover:text-foreground"
          >
            <ListChecks className="size-3" aria-hidden />
            {todoProgress.done}/{todoProgress.total}
          </button>
        ) : null}

        {draft.images.length ? (
          <div className="flex flex-wrap gap-2 px-3 pt-3">
            {draft.images.map((image) => (
              <span
                key={image.id}
                className="flex items-center gap-1 rounded-md border bg-muted px-2 py-0.5 text-xs"
              >
                {image.name}
                <button
                  type="button"
                  aria-label={`移除 ${image.name}`}
                  onClick={() => store().removeImage(sessionId, image.id)}
                  className="text-muted-foreground hover:text-foreground"
                >
                  <X className="size-3" aria-hidden />
                </button>
              </span>
            ))}
          </div>
        ) : null}

        <textarea
          ref={textarea}
          aria-label="输入消息"
          rows={1}
          disabled={readOnly}
          value={draft.text}
          placeholder={
            readOnly
              ? "只读会话"
              : running
                ? runningEnter === "steer"
                  ? "Enter 插队补充，Ctrl+Enter 排队，Esc 停止"
                  : "Enter 打断并发送，Ctrl+Enter 排队，Esc 停止"
                : "输入消息，/ 命令，! 执行 bash，// 引用资源"
          }
          onChange={(event) => setText(event.target.value, undefined)}
          onSelect={(event) => {
            if (menu?.kind === "resource")
              updateMenu(draft.text, event.currentTarget.selectionStart);
          }}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
          onCompositionStart={() => {
            composing.current = true;
          }}
          onCompositionEnd={() => {
            composing.current = false;
          }}
          onBlur={() => setMenu(null)}
          className="block max-h-[40vh] min-h-11 w-full resize-none bg-transparent px-4 pt-3 pb-1 text-sm outline-none placeholder:text-muted-foreground disabled:cursor-not-allowed"
        />

        <div className="flex flex-wrap items-center gap-1 px-2 pb-2">
          <span
            title={info.cwd}
            className="flex max-w-40 items-center gap-1 truncate rounded-md px-2 py-1 text-muted-foreground text-xs"
          >
            <FolderOpen className="size-3.5 shrink-0" aria-hidden />
            <span className="truncate">{workspaceName(info.cwd)}</span>
          </span>
          <ModeSelect info={info} disabled={readOnly} onClm={() => setClmOpen(true)} />
          <PermissionSelect info={info} disabled={readOnly} />
          <ModelPicker sessionId={sessionId} disabled={readOnly} />
          <div className="ml-auto flex items-center gap-1">
            {running ? <span className="mr-1 text-muted-foreground text-xs">Esc 停止</span> : null}
            <label
              className={cn(
                "inline-flex size-8 cursor-pointer items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground",
                (!acceptsImages || readOnly) && "pointer-events-none opacity-40",
              )}
              title={acceptsImages ? "添加图片" : "当前模型不支持图片"}
            >
              <ImagePlus className="size-4" aria-hidden />
              <span className="sr-only">添加图片</span>
              <input
                type="file"
                accept="image/*"
                multiple
                className="sr-only"
                disabled={!acceptsImages || readOnly}
                onChange={(event) => {
                  void addImages([...(event.target.files ?? [])]);
                  event.target.value = "";
                }}
              />
            </label>
            {running ? (
              <IconButton label="停止" shortcut="Esc" variant="secondary" onClick={cancel}>
                <Square className="fill-current" aria-hidden />
              </IconButton>
            ) : null}
            <IconButton
              label={running ? "补充" : "发送"}
              shortcut="Enter"
              variant="default"
              disabled={readOnly || (!draft.text.trim() && draft.images.length === 0)}
              onClick={() => void submit(false)}
              className="rounded-full"
            >
              <ArrowUp aria-hidden />
            </IconButton>
          </div>
        </div>
      </div>

      <AlertDialog open={clmOpen} onOpenChange={setClmOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>启用 CLM 模式？</AlertDialogTitle>
            <AlertDialogDescription>启用后本会话不可再切换模式。</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              onClick={() =>
                void runAction(() => setMode(sessionId, "clm_experimental"), "已启用 CLM")
              }
            >
              启用
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}

function ModeSelect({
  info,
  disabled,
  onClm,
}: {
  info: SessionInfo;
  disabled: boolean;
  onClm: () => void;
}) {
  const locked = info.mode === "clm_experimental";
  return (
    <Select
      value={info.mode}
      disabled={disabled || locked}
      onValueChange={(value) => {
        if (value === "clm_experimental") onClm();
        else void runAction(() => setMode(info.id, value as SessionMode));
      }}
    >
      <SelectTrigger
        size="sm"
        aria-label="会话模式"
        className="h-7 border-none px-2 text-xs shadow-none"
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {(locked ? (["clm_experimental"] as SessionMode[]) : SWITCHABLE).map((mode) => (
          <SelectItem key={mode} value={mode}>
            {MODE_LABEL[mode]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function PermissionSelect({ info, disabled }: { info: SessionInfo; disabled: boolean }) {
  return (
    <Select
      value={info.permission}
      disabled={disabled}
      onValueChange={(value) =>
        void runAction(async () => {
          const next = await rpc.call<SessionInfo>("session/set_permission", {
            session_id: info.id,
            permission: value,
          });
          useTranscript.getState().mutate(info.id, (view) => {
            view.info = next;
          });
          useSessions.getState().upsert(next);
        })
      }
    >
      <SelectTrigger
        size="sm"
        aria-label="权限"
        className="h-7 border-none px-2 text-xs shadow-none"
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {PERMISSIONS.map((permission) => (
          <SelectItem key={permission} value={permission}>
            {PERMISSION_LABEL[permission]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
