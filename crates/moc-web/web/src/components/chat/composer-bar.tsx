// 输入区：多行输入 + 斜杠命令 / 资源 URI 补全 + `!` 手动命令 + 图片与文件（选择 / 粘贴 / 拖放 /
// 服务端选取）+ 权限、引擎、计划、模型与上下文占用；运行中回车按偏好插队或中断，Ctrl+Enter 排队。
// 有效引用（命令、`@` 路径、资源 URI、`#N` 图片、`#name` 文件）在输入框里渲染成整体控件。

import type { EditorView } from "@codemirror/view";
import {
  ArrowUpIcon,
  BookOpenIcon,
  FileTextIcon,
  FolderIcon,
  HistoryIcon,
  ListPlusIcon,
  Loader2Icon,
  LockIcon,
  PackageIcon,
  SquareIcon,
} from "lucide-react";
import { nanoid } from "nanoid";
import { type DragEvent, useEffect, useId, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  Attachment,
  AttachmentInfo,
  AttachmentPreview,
  AttachmentRemove,
  Attachments,
} from "@/components/ai-elements/attachments";
import { ImagePreviewDialog } from "@/components/chat/image-preview-dialog";
import { Button } from "@/components/ui/button";
import { ButtonGroup } from "@/components/ui/button-group";
import { Popover, PopoverAnchor } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  buildCommandEntries,
  buildSubmenuItems,
  type CommandContext,
  firstEnabledIndex,
  parseCommandChip,
  resolveStage2,
  type SubmenuAction,
  stepEnabledIndex,
} from "@/lib/command-params";
import { t, useT } from "@/lib/i18n";
import { leadSelector, parseSelector } from "@/lib/models";
import { useRefInfo } from "@/lib/ref-inspect";
import { insertToken, remapImageRefs, scanRefs, uniqueRefName } from "@/lib/refs";
import { fromMeta, runSlash, type SlashCommand } from "@/lib/slash";
import { cn, formatBytes } from "@/lib/utils";
import { uploadSessionFile } from "@/lib/web-api";
import { rpc } from "@/protocol/client";
import type { ImageSource, PluginInfo, ResourceCandidate, SteerResult } from "@/protocol/types";
import { dispatchNext, runBash, sendPrompt } from "@/store/actions";
import {
  absorbRefs,
  type Attachment as ComposerAttachment,
  type ComposerFile,
  type PendingItem,
  toInput,
  useComposer,
} from "@/store/composer";
import { selectCommands, selectFeatures, selectModels, useConnection } from "@/store/connection";
import { reacquire, togglePlanMode } from "@/store/lifecycle";
import { useMcp } from "@/store/mcp";
import { usePrefs } from "@/store/prefs";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { AddContextMenu, type AttachKind } from "./add-context-menu";
import { CommandChipEditor } from "./command-chip-editor";
import { CommandArgMenu, CommandMenu, CommandParamHint } from "./command-menu";
import { ComposerEditor, type ComposerEditorHandle, type EditorChip } from "./composer-editor";
import {
  ComposerStatus,
  ContextRing,
  PendingList,
  PlanToggle,
  SessionOptions,
  ShellModeNotice,
  type Suggestion,
  SuggestionMenu,
} from "./composer-parts";
import { ModelPicker } from "./model-select-popover";

const EMPTY_PENDING: PendingItem[] = [];
const EMPTY_FILES: ComposerFile[] = [];
const MAX_HEIGHT = 320;
/** 与服务端 `/uploads` 的体积上限一致。 */
const UPLOAD_MAX_BYTES = 512 * 1024 * 1024;
/** 输入框内复制时随文本带上的引用载荷：图片与文件附件，粘贴到任意输入框都指向原内容。 */
const REFS_MIME = "application/x-moc-refs";
type RefsPayload = { text: string; images: ComposerAttachment[]; files: ComposerFile[] };
/** 输入以 `/plan` 加空白开头即视为确认命令；其后的文本留作 plan 模式下的草稿。 */
const PLAN_PREFIX = /^\/plan\s+/;

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
      name: file.name || t("cb.pasted_image"),
      source: { Base64: { media_type: match[1], data: match[2] } },
    });
  };
  reader.onerror = () => reject(new Error(file.name));
  reader.readAsDataURL(file);
  return promise;
}

/** 光标所在的空白分隔 token；以 `//`、`scheme:/` 或含 `://` 时视为资源 URI 前缀；
 * `@` 开头（非 `@@`）视为工作区文件引用（与 TUI 一致）。`!` bash 模式下 @ 不补全。 */
function resourceToken(
  text: string,
  cursor: number,
): { prefix: string; start: number; end: number } | null {
  let start = Math.min(cursor, text.length);
  while (start > 0 && !" \n\t".includes(text[start - 1])) start--;
  let end = Math.min(cursor, text.length);
  while (end < text.length && !/\s/.test(text[end])) end++;
  const prefix = text.slice(start, cursor);
  const isUri = prefix === "//" || prefix.endsWith(":/") || prefix.includes("://");
  if (isUri) return { prefix, start, end };
  const isAttachment = prefix.startsWith("@") && !prefix.startsWith("@@") && !text.startsWith("!");
  return isAttachment ? { prefix, start, end } : null;
}

const RESOURCE_ICON: Record<string, typeof FileTextIcon> = {
  artifact: PackageIcon,
  history: HistoryIcon,
  local: FileTextIcon,
  skill: BookOpenIcon,
  namespace: FolderIcon,
  file: FileTextIcon,
  dir: FolderIcon,
};

/**
 * 资源候选 → 补全条目。工作区路径拆成文件名与所在目录：文件名完整显示并高亮命中字符，
 * 目录从左侧省略；详情区给出完整路径，超长路径在那里折行可见。
 */
function resourceSuggestion(candidate: ResourceCandidate): Suggestion {
  const Icon = RESOURCE_ICON[candidate.kind] ?? FileTextIcon;
  const label = candidate.label || candidate.uri;
  const matches = candidate.matches ?? [];
  const base = {
    key: candidate.uri,
    detail: candidate.description || undefined,
    icon: <Icon />,
  };
  if (candidate.kind !== "file" && candidate.kind !== "dir")
    return { ...base, title: label, titleMatches: matches };
  const slash = label.lastIndexOf("/");
  // 命中下标按 Unicode 标量计，换算时用码点数而不是 UTF-16 下标
  const offset = Array.from(label.slice(0, slash + 1)).length;
  return {
    ...base,
    title: `${label.slice(slash + 1)}${candidate.kind === "dir" ? "/" : ""}`,
    titleMatches: matches.filter((index) => index >= offset).map((index) => index - offset),
    subtitle: slash > 0 ? label.slice(0, slash) : undefined,
    subtitleMatches: matches.filter((index) => index < offset - 1),
    signature: label,
  };
}

/** 插件清单 → 斜杠命令；加载失败的插件不贡献命令。 */
function pluginSlashCommands(plugins: PluginInfo[] | null | undefined): SlashCommand[] {
  return (plugins ?? []).flatMap((plugin) =>
    plugin.error
      ? []
      : (plugin.commands ?? []).map((command) => ({
          name: command.name,
          description: command.description,
          usage: command.usage || undefined,
          params: command.params?.length ? command.params : undefined,
          plugin: plugin.name,
          needsSession: true,
        })),
  );
}

type ComposerBarProps = {
  sessionId: string;
  /** 欢迎页：卡片内嵌在居中列里，不套底部 dock。 */
  inline?: boolean;
};

export function ComposerBar({ sessionId, inline = false }: ComposerBarProps) {
  const t = useT();
  const editorRef = useRef<ComposerEditorHandle>(null);
  const editorBoxRef = useRef<HTMLDivElement>(null);
  const suggestionsId = useId();
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [cursor, setCursorState] = useState(0);
  /** 同步镜像：连续插入多个引用时不必等重渲染拿最新光标。 */
  const cursorRef = useRef(0);
  const setCursor = (value: number) => {
    cursorRef.current = value;
    setCursorState(value);
  };
  /** 逐字键入中的光标位置：覆盖它的引用还在输入，不成形为控件；移动光标或非键入改动即清除。 */
  const [typingAt, setTypingAt] = useState<number | null>(null);
  const [previewImage, setPreviewImage] = useState<{ src: string; name: string } | null>(null);
  const [reading, setReading] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [activeIndex, setActiveIndex] = useState(0);
  /** 命令浮层键盘焦点所在面板：list=一级命令列表，sub=右侧二级菜单。 */
  const [focusPane, setFocusPane] = useState<"list" | "sub">("list");
  const [subActiveIndex, setSubActiveIndex] = useState(-1);
  /** 混合命令从二级菜单点选自由文本入口时记录输入串，阶段二跳过子命令列表直入文本引导。 */
  const [preferHintFor, setPreferHintFor] = useState<string | null>(null);
  /** 必填参数为空时按下回车触发的温和强调态，下次编辑即清除。 */
  const [missingParamError, setMissingParamError] = useState(false);
  /** Esc 关闭补全或叶子填充后记下当时的输入，内容变化前不再弹出。 */
  const [dismissedAt, setDismissedAt] = useState<string | null>(null);
  /** 点开命令胶囊编辑非自然语言参数的浮层开关。 */
  const [chipEditorOpen, setChipEditorOpen] = useState(false);
  const chipAnchorRef = useRef<HTMLElement | null>(null);
  const [resources, setResources] = useState<{
    prefix: string;
    kind?: AttachKind;
    items: ResourceCandidate[];
  }>({ prefix: "", items: [] });
  /** 「+」菜单插入的 `@` 带类型过滤；只对插入位置上的那个 token 生效。 */
  const [attach, setAttach] = useState<{ start: number; kind: AttachKind } | null>(null);
  const plugins = useTranscript((state) => state.bySession[sessionId]?.plugins);

  const info = useTranscript((state) => state.bySession[sessionId]?.info);
  const draftPermission = useSessions((state) => state.draftSession?.permission);
  const promptRunning = useTranscript((state) => Boolean(state.bySession[sessionId]?.running));
  const skillsWorking = useUi((state) => state.skillsWorking !== null);
  const running = promptRunning || skillsWorking;
  const readOnly = useTranscript((state) => Boolean(state.bySession[sessionId]?.readOnly));
  const phases = useTranscript((state) => state.bySession[sessionId]?.todos);
  const context = useTranscript((state) => state.bySession[sessionId]?.context);
  const holder = useTranscript((state) => state.bySession[sessionId]?.holder);
  const focusCount = useUi((state) => state.focusComposer);
  const draft = useComposer((state) => state.drafts[sessionId]);
  const pending = useComposer((state) => state.pending[sessionId] ?? EMPTY_PENDING);
  const runningEnter = usePrefs((state) => state.runningEnter);
  const queueOnEnter = usePrefs((state) => state.queueOnEnter);
  const models = useConnection(selectModels);
  const builtinMetas = useConnection(selectCommands);
  const features = useConnection(selectFeatures);

  const text = draft?.text ?? "";
  const images = draft?.images ?? [];
  const files = draft?.files ?? EMPTY_FILES;
  const isDraft = sessionId === "draft-session";
  const bashMode = text.startsWith("!");
  const model = parseSelector(leadSelector(info?.model ?? ""), models).model;
  // 模型没声明 inputs 时不拦，交给 core 校验。
  const acceptsImages = !model?.inputs || model.inputs.includes("image");

  const todos = useMemo(() => (phases ?? []).flatMap((phase) => phase.items), [phases]);
  const cwd = useTranscript((state) => state.bySession[sessionId]?.info.cwd);
  const draftCwd = useSessions((state) => state.draftSession?.cwd);
  const defaultCwd = useConnection((state) => state.config?.cwd);
  const effectiveCwd = (isDraft ? draftCwd : cwd) ?? defaultCwd;
  const mcpSnapshot = useMcp((state) => (effectiveCwd ? state.byCwd[effectiveCwd] : undefined));

  // ---------- 补全 ----------

  const token = useMemo(() => resourceToken(text, cursor), [text, cursor]);
  const tokenPrefix = token?.prefix ?? "";
  const attachKind =
    attach && token?.start === attach.start && tokenPrefix.startsWith("@")
      ? attach.kind
      : undefined;
  // 插件清单由 refreshSideData 在会话打开时统一拉取；这里只做投影。
  const pluginCommands = useMemo(() => pluginSlashCommands(plugins), [plugins]);
  const commands = useMemo(() => {
    const builtinCommands = builtinMetas.map(fromMeta);
    const builtin = new Set(builtinCommands.map((command) => command.name));
    return [...builtinCommands, ...pluginCommands.filter((command) => !builtin.has(command.name))];
  }, [builtinMetas, pluginCommands]);
  const commandCtx = useMemo<CommandContext>(
    () => ({
      isDraft,
      promptRunning,
      skillsWorking,
      autonomy: info?.autonomy,
      permission: isDraft ? draftPermission : info?.permission,
      mcpServers: mcpSnapshot?.servers,
    }),
    [
      isDraft,
      promptRunning,
      skillsWorking,
      info?.autonomy,
      info?.permission,
      draftPermission,
      mcpSnapshot?.servers,
    ],
  );
  const commandEntries = useMemo(
    () => (dismissedAt === text ? [] : buildCommandEntries(text, commands, features, commandCtx)),
    [dismissedAt, text, commands, features, commandCtx],
  );

  // ---------- 引用控件 ----------

  const parsedCommand = useMemo(
    () => (bashMode ? null : parseCommandChip(text, commands, commandCtx, typingAt)),
    [bashMode, text, commands, commandCtx, typingAt],
  );
  const command = parsedCommand ? { start: parsedCommand.start, end: parsedCommand.end } : null;
  const fileNames = useMemo(() => files.map((file) => file.name), [files]);
  const spans = useMemo(
    () =>
      bashMode
        ? []
        : scanRefs(text, { command, imageCount: images.length, fileNames }).filter(
            (span) => typingAt === null || typingAt < span.from || typingAt > span.to,
          ),
    [bashMode, text, command, images.length, fileNames, typingAt],
  );
  const resourceTokens = useMemo(
    () => [...new Set(spans.filter((span) => span.kind === "resource").map((span) => span.token))],
    [spans],
  );
  const refInfo = useRefInfo(isDraft ? null : sessionId, resourceTokens);
  const chips: EditorChip[] = spans.flatMap((span): EditorChip[] => {
    const at = { from: span.from, to: span.to };
    if (span.kind === "command" && parsedCommand) {
      const chipArgs = parsedCommand.chipArgs.map((arg) => arg.value);
      const lastDesc =
        parsedCommand.chipArgs.at(-1)?.matchedChoice?.description ||
        parsedCommand.command.description;
      const detail = parsedCommand.editable
        ? lastDesc
          ? `${lastDesc} · ${t("cmd.click_to_edit")}`
          : t("cmd.click_to_edit")
        : parsedCommand.command.description;
      return [
        {
          ...at,
          chip: {
            kind: "command",
            label: parsedCommand.typedName,
            args: chipArgs.length > 0 ? chipArgs : undefined,
            detail,
            interactive: parsedCommand.editable,
            active: chipEditorOpen,
          },
        },
      ];
    }
    if (span.kind === "image") {
      const image = images[Number(span.token.slice(1)) - 1];
      return image
        ? [
            {
              ...at,
              chip: {
                kind: "image",
                label: span.token,
                detail: image.name,
                thumb: imageUrl(image.source),
              },
            },
          ]
        : [];
    }
    if (span.kind === "file-ref") {
      const file = files.find((entry) => entry.name === span.token.slice(1));
      if (!file) return [];
      const where = file.path ?? (file.uploading ? t("cb.uploading") : t("cb.file_pending"));
      return [
        {
          ...at,
          chip: {
            kind: "file-ref",
            label: file.name,
            detail: `${where} · ${formatBytes(file.size)}`,
          },
        },
      ];
    }
    const info = refInfo(span.token);
    return info ? [{ ...at, chip: info }] : [];
  });
  // 指纹不含缩略图 data URL（可能几 MB），图片身份由 id 表达。
  const chipsKey = `${chips
    .map(
      ({ from, to, chip }) =>
        `${from}:${to}:${chip.kind}:${chip.label}:${chip.args?.join("\0") ?? ""}:${chip.detail ?? ""}:${chip.interactive ? 1 : 0}:${chip.active ? 1 : 0}`,
    )
    .join("\n")}|${images.map((image) => image.id).join(",")}`;
  // biome-ignore lint/correctness/useExhaustiveDependencies: chipsKey 是 chips 的内容指纹，避免每次渲染都重建装饰
  const stableChips = useMemo(() => chips, [chipsKey]);

  const canEditCommandChip = Boolean(parsedCommand?.editable && !bashMode);
  useEffect(() => {
    if (!canEditCommandChip) setChipEditorOpen(false);
  }, [canEditCommandChip]);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 切换会话时收起命令胶囊编辑浮层
  useEffect(() => {
    setChipEditorOpen(false);
  }, [sessionId]);

  useEffect(() => {
    if (!tokenPrefix || isDraft) return;
    let alive = true;
    const timer = window.setTimeout(() => {
      rpc
        .call<{ candidates: ResourceCandidate[] }>("resource/complete", {
          session_id: sessionId,
          prefix: tokenPrefix,
          kind: attachKind,
        })
        .then(
          (result) =>
            alive &&
            setResources({ prefix: tokenPrefix, kind: attachKind, items: result.candidates }),
        )
        .catch(() => alive && setResources({ prefix: tokenPrefix, kind: attachKind, items: [] }));
    }, 120);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [isDraft, sessionId, tokenPrefix, attachKind]);

  const resourceItems =
    token && resources.prefix === tokenPrefix && resources.kind === attachKind
      ? resources.items
      : [];
  const activeResourceSuggestions = useMemo(
    () => (dismissedAt === text ? [] : resourceItems.map(resourceSuggestion)),
    [dismissedAt, text, resourceItems],
  );
  // 资源补全（`@路径` / `skill://`）在命令参数内优先弹出，不与阶段二参数浮层冲突；
  // 若命令胶囊已收拢完整非自然语言参数且无缺失必填项（如 `/export ./notes.md`），不叠放多余引导条。
  const stage2 = useMemo(() => {
    if (
      chipEditorOpen ||
      dismissedAt === text ||
      commandEntries.length > 0 ||
      activeResourceSuggestions.length > 0 ||
      token !== null
    ) {
      return null;
    }
    const resolved = resolveStage2(
      text,
      cursor,
      commands,
      features,
      commandCtx,
      preferHintFor === text,
    );
    if (
      resolved?.kind === "hint" &&
      !resolved.missingRequired &&
      typingAt === null &&
      parsedCommand &&
      parsedCommand.chipArgs.length > 0 &&
      parsedCommand.end === text.length
    ) {
      return null;
    }
    return resolved;
  }, [
    chipEditorOpen,
    dismissedAt,
    text,
    commandEntries.length,
    activeResourceSuggestions.length,
    token,
    cursor,
    commands,
    features,
    commandCtx,
    preferHintFor,
    typingAt,
    parsedCommand,
  ]);

  const activeOverlay = chipEditorOpen
    ? null
    : commandEntries.length > 0
      ? "commands"
      : activeResourceSuggestions.length > 0
        ? "resources"
        : stage2?.kind === "choices"
          ? "args"
          : stage2?.kind === "hint"
            ? "hint"
            : null;
  const activeCommandEntry =
    activeOverlay === "commands"
      ? commandEntries[Math.min(Math.max(activeIndex, 0), commandEntries.length - 1)]
      : undefined;
  const activeSubmenu = useMemo(
    () =>
      activeCommandEntry
        ? buildSubmenuItems(activeCommandEntry.command, commandCtx)
        : { sections: [], flat: [] },
    [activeCommandEntry, commandCtx],
  );
  const ghostHint =
    !bashMode && !chipEditorOpen && activeResourceSuggestions.length === 0 && token === null
      ? stage2?.ghostPlaceholder
      : undefined;

  // 当前工作区尚未拉过 MCP 快照时，高亮 `/mcp`、进入 `/mcp ` 或点开 `/mcp` 胶囊编辑时按需刷新一次。
  const needsMcpServers =
    !mcpSnapshot &&
    Boolean(effectiveCwd) &&
    (activeCommandEntry?.command.name === "mcp" ||
      stage2?.command.name === "mcp" ||
      (chipEditorOpen && parsedCommand?.command.name === "mcp"));
  useEffect(() => {
    if (!needsMcpServers || !effectiveCwd) return;
    void useMcp
      .getState()
      .refresh(isDraft ? { cwd: effectiveCwd } : { sessionId, cwd: effectiveCwd })
      .catch(() => undefined);
  }, [needsMcpServers, effectiveCwd, isDraft, sessionId]);

  const commandListKey =
    activeOverlay === "commands"
      ? commandEntries
          .map((entry) => `${entry.command.name}:${entry.disabledReason ?? ""}`)
          .join("|")
      : activeOverlay === "args" && stage2?.kind === "choices"
        ? stage2.items.map((item) => item.key).join("|")
        : activeOverlay === "resources"
          ? String(activeResourceSuggestions.length)
          : "";

  // biome-ignore lint/correctness/useExhaustiveDependencies: 候选集合变化时复位高亮与二级面板焦点
  useEffect(() => {
    setFocusPane("list");
    setSubActiveIndex(-1);
    setActiveIndex(activeOverlay === "commands" ? firstEnabledIndex(commandEntries) : 0);
  }, [activeOverlay, commandListKey]);

  // ---------- 输入框 ----------

  useEffect(() => {
    if (focusCount > 0) editorRef.current?.focus();
  }, [focusCount]);

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
    if (value !== text) {
      setDismissedAt(null);
      setMissingParamError(false);
      if (preferHintFor !== value) setPreferHintFor(null);
    }
    useComposer.getState().setText(sessionId, value);
    if (caret !== undefined) {
      setCursor(caret);
      setTypingAt(null);
      requestAnimationFrame(() => editorRef.current?.setCaret(caret));
    }
  };

  /** 填入命令或参数文本到输入框（不自动执行），光标落至末尾并按需关闭或续展补全。 */
  const fillCommandText = (value: string, closeMenu: boolean, preferHint = false) => {
    useComposer.getState().setText(sessionId, value);
    setDismissedAt(closeMenu ? value : null);
    setPreferHintFor(preferHint ? value : null);
    setFocusPane("list");
    setSubActiveIndex(-1);
    setMissingParamError(false);
    setCursor(value.length);
    setTypingAt(null);
    requestAnimationFrame(() => editorRef.current?.setCaret(value.length));
  };

  /** `/plan` 一经确认（补全选中或其后键入空白）即切换 plan 叠加态，命令本身不留在输入框。 */
  const consumePlan = (rest: string): boolean => {
    if (running) {
      toast(skillsWorking ? t("slash.skills_busy") : t("slash.running", { name: "plan" }));
      return false;
    }
    setText(rest, rest.length);
    void togglePlanMode(sessionId, !info?.plan_active).catch((error) =>
      toast.error(t("slash.plan_failed", { error: errorText(error) })),
    );
    return true;
  };

  const pickCommand = (index: number) => {
    const entry = commandEntries[index];
    if (!entry) return;
    if (entry.disabledReason) {
      toast(entry.disabledReason);
      return;
    }
    const command = entry.command;
    if (!command.plugin && command.name === "plan") {
      consumePlan("");
      return;
    }
    if (command.params?.length || command.usage) {
      fillCommandText(`/${command.name} `, false);
      return;
    }
    if (runSlash(sessionId, `/${command.name}`, commands, features)) setCursor(0);
  };

  const pickSubAction = (action: SubmenuAction) => {
    if (activeCommandEntry?.disabledReason) {
      toast(activeCommandEntry.disabledReason);
      return;
    }
    fillCommandText(action.fillText, action.closeMenu, Boolean(action.preferHint));
  };

  const pickArgChoice = (index: number) => {
    if (stage2?.kind !== "choices") return;
    const item = stage2.items[index];
    if (!item) return;
    fillCommandText(item.fillText, item.closeMenu);
  };

  const pickResource = (index: number) => {
    const candidate = resourceItems[index];
    if (!candidate || !token) return;
    const suffix = candidate.append_space ? " " : "";
    const next = `${text.slice(0, token.start)}${candidate.uri}${suffix}${text.slice(token.end)}`;
    setText(next, token.start + candidate.uri.length + suffix.length);
  };

  /** 「+」菜单：在光标处插入补全前缀，前后补空白使其成为独立 token；`/` 只在空输入框里插入。 */
  const insertTrigger = (prefix: string, kind?: AttachKind) => {
    if (prefix === "/") {
      setText("/", 1);
      return;
    }
    const at = Math.min(cursor, text.length);
    const before = at > 0 && !/\s/.test(text[at - 1]) ? " " : "";
    const after = at < text.length && !/\s/.test(text[at]) ? " " : "";
    const start = at + before.length;
    setAttach(kind ? { start, kind } : null);
    setText(
      `${text.slice(0, at)}${before}${prefix}${after}${text.slice(at)}`,
      start + prefix.length,
    );
  };

  // ---------- 图片与文件 ----------

  /** 在光标处插入一个引用 token（`#N` / `#name`），基于 store 里最新的文本，可连续调用。 */
  const insertRef = (token: string) => {
    const current = useComposer.getState().drafts[sessionId]?.text ?? "";
    const next = insertToken(current, Math.min(cursorRef.current, current.length), token);
    setText(next.text, next.caret);
  };

  /** 按路径引用的文件进草稿：引用名去重后在光标处插入 `#name`。 */
  const addFileRef = (file: Omit<ComposerFile, "id" | "name">, fileName: string): string => {
    const taken = (useComposer.getState().drafts[sessionId]?.files ?? []).map(
      (entry) => entry.name,
    );
    const name = uniqueRefName(fileName, taken);
    const id = nanoid();
    useComposer.getState().addFile(sessionId, { ...file, id, name });
    insertRef(`#${name}`);
    return id;
  };

  /** 图片（模型支持时）作为图片输入并插入 `#N`；其余文件上传到会话目录、按路径引用。
   * 草稿会话还没有会话目录，文件留在浏览器里，发送时建出会话后再上传。 */
  const addFiles = async (selected: File[]) => {
    const pictures = acceptsImages ? selected.filter((file) => file.type.startsWith("image/")) : [];
    for (const file of selected) {
      if (pictures.includes(file)) continue;
      if (file.size > UPLOAD_MAX_BYTES) {
        toast.error(t("cb.too_large", { name: file.name }));
        continue;
      }
      if (isDraft) {
        addFileRef({ size: file.size, blob: file }, file.name);
        continue;
      }
      const id = addFileRef({ size: file.size, uploading: true }, file.name);
      uploadSessionFile(sessionId, file)
        .then((stored) =>
          useComposer
            .getState()
            .updateFile(sessionId, id, { path: stored.path, size: stored.size, uploading: false }),
        )
        .catch((error) => {
          useComposer.getState().removeFile(sessionId, id);
          toast.error(t("cb.upload_failed", { name: file.name, error: errorText(error) }));
        });
    }
    if (pictures.length === 0) return;
    setReading(true);
    try {
      const results = await Promise.allSettled(pictures.map(readImage));
      for (const result of results) {
        if (result.status !== "fulfilled") continue;
        useComposer.getState().addImage(sessionId, result.value);
        insertRef(`#${useComposer.getState().drafts[sessionId]?.images.length ?? 1}`);
      }
      const failed = results.filter((result) => result.status === "rejected").length;
      if (failed > 0) toast.error(t("cb.read_failed", { n: failed }));
    } finally {
      setReading(false);
    }
  };

  /** 服务端文件选取器：直接引用 core 所在机器上的文件，不复制。 */
  const browseServer = () =>
    useUi.getState().set({
      folderPicker: {
        open: true,
        start: (isDraft ? draftCwd : cwd) || undefined,
        onPickFiles: (entries) => {
          for (const entry of entries)
            addFileRef({ size: entry.size ?? 0, path: entry.path }, entry.name);
        },
      },
    });

  const onPaste = (event: ClipboardEvent, view: EditorView): boolean => {
    const data = event.clipboardData;
    if (!data) return false;
    const raw = data.getData(REFS_MIME);
    if (raw) {
      const payload = JSON.parse(raw) as RefsPayload;
      const composer = useComposer.getState();
      const current = composer.drafts[sessionId] ?? { text: "", images: [], files: [] };
      const absorbed = absorbRefs(current, payload);
      composer.restore(sessionId, { ...current, images: absorbed.images, files: absorbed.files });
      view.dispatch(view.state.replaceSelection(absorbed.text), {
        userEvent: "input.paste",
        scrollIntoView: true,
      });
      event.preventDefault();
      return true;
    }
    const pasted = [...data.files];
    if (pasted.length === 0) return false;
    void addFiles(pasted);
    // 图文混贴时保留文字原生插入；纯文件或文件管理器带上的同名文本（如 macOS Finder 复制文件）则拦截，避免把文件名贴进输入框。
    const plain = data.getData("text/plain").replace(/\r\n/g, "\n").trim();
    if (plain && plain !== pasted.map((file) => file.name).join("\n")) return false;
    event.preventDefault();
    return true;
  };

  /** 复制带 `#N` / `#name` 引用的片段时附上被引用的附件，粘贴回任一输入框仍指向原内容。 */
  const onCopy = (event: ClipboardEvent, view: EditorView, cut: boolean): boolean => {
    const { from, to } = view.state.selection.main;
    const slice = view.state.sliceDoc(from, to);
    const refs = scanRefs(slice, { imageCount: images.length, fileNames }).filter(
      (span) => span.kind === "image" || span.kind === "file-ref",
    );
    if (refs.length === 0 || !event.clipboardData) return false;
    const picked: ComposerAttachment[] = [];
    const order = new Map<number, number>();
    for (const span of refs) {
      const n = Number(span.token.slice(1));
      if (span.kind !== "image" || order.has(n)) continue;
      picked.push(images[n - 1]);
      order.set(n, picked.length);
    }
    const names = new Set(
      refs.filter((span) => span.kind === "file-ref").map((span) => span.token.slice(1)),
    );
    const payload: RefsPayload = {
      text: remapImageRefs(slice, images.length, (n) => order.get(n) ?? n),
      images: picked,
      // 还没上传的浏览器文件无法跨输入框传递，只带有路径的文件。
      files: files
        .filter((file) => names.has(file.name) && file.path)
        .map(({ id, name, size, path }) => ({ id, name, size, path })),
    };
    event.clipboardData.setData("text/plain", slice);
    event.clipboardData.setData(REFS_MIME, JSON.stringify(payload));
    event.preventDefault();
    if (cut) view.dispatch(view.state.replaceSelection(""), { userEvent: "delete.cut" });
    return true;
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    setDragging(false);
    const dropped = [...event.dataTransfer.files];
    if (dropped.length === 0) return;
    event.preventDefault();
    void addFiles(dropped);
  };

  // ---------- 发送 ----------

  const attached = images.length > 0 || files.length > 0;
  // 运行中投递的插队/排队项必须是完整输入：还在上传或尚未上传的文件先挡住。
  const canSend =
    (text.trim().length > 0 || attached) &&
    !readOnly &&
    !reading &&
    !skillsWorking &&
    !files.some((file) => file.uploading || (running && !file.path));

  /** 运行中的投递方式：`queue` 排队，`active` 按「运行中回车」补充或中断；`alternate` 即 Ctrl+Enter。 */
  const send = (
    alternate = false,
    mode: "queue" | "active" = alternate !== queueOnEnter ? "queue" : "active",
  ) => {
    const trimmed = text.trim();
    if (!canSend) return;

    if (bashMode && !attached) {
      const exclude = trimmed.startsWith("!!");
      const command = trimmed.slice(exclude ? 2 : 1).trim();
      if (!command) return;
      if (running) {
        toast(t("cb.bash_busy"));
        return;
      }
      useComposer.getState().clearDraft(sessionId);
      void runBash(sessionId, command, exclude);
      return;
    }
    // 必填命令参数留空直接回车：保留草稿，点亮参数引导条上的必填提示。
    if (!attached) {
      const pendingHint = resolveStage2(text, text.length, commands, features, commandCtx);
      if (pendingHint?.kind === "hint" && pendingHint.missingRequired) {
        setDismissedAt(null);
        setMissingParamError(true);
        return;
      }
    }
    // `.` 独占整条输入时是 /retry 的快捷写法：转写成命令，走同一套门控。
    const dispatch = trimmed === "." ? "/retry" : trimmed;
    if (!attached && runSlash(sessionId, dispatch, commands, features)) return;

    const input = toInput(trimmed, images, files);
    useComposer.getState().clearDraft(sessionId);
    if (!running) {
      const uploads = files.filter((file) => !file.path);
      void sendPrompt(
        sessionId,
        input,
        uploads.length > 0 ? { uploads: { files: uploads, draft: { text, images, files } } } : {},
      );
      return;
    }

    const queue = mode === "queue" || runningEnter === "interrupt";
    const item: PendingItem = {
      key: nanoid(),
      kind: queue ? "queue" : "steer",
      text: trimmed,
      images,
      files,
    };
    useComposer.getState().pushPending(sessionId, item);
    if (queue) {
      dispatchNext(sessionId);
      if (mode === "active") {
        void rpc
          .call("session/cancel", { session_id: sessionId })
          .catch((error) => toast.error(t("cb.interrupt_failed", { error: errorText(error) })));
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
          toast(t("cb.steer_queued"));
        } else if (result.id != null) {
          composer.updatePending(sessionId, item.key, { steerId: result.id });
        }
      })
      .catch((error) => {
        const composer = useComposer.getState();
        if (composer.pending[sessionId]?.some((entry) => entry.key === item.key)) {
          composer.removePending(sessionId, item.key);
          const current = composer.drafts[sessionId];
          if (!current?.text && !current?.images.length && !current?.files.length)
            composer.restore(sessionId, { text: trimmed, images, files });
        }
        toast.error(t("cb.steer_failed", { error: errorText(error) }));
      });
  };

  const stop = () =>
    void rpc
      .call(
        skillsWorking ? "skills/cancel" : "session/cancel",
        skillsWorking ? {} : { session_id: sessionId },
      )
      .catch((error) => toast.error(t("cb.stop_failed", { error: errorText(error) })));

  /** 队列项取回草稿编辑：`/ask` 项保留前缀（重发仍走只读模式）；
   * 草稿非空时把待发内容追加进草稿一起编辑，不把未确认的草稿换进队列自动发出。 */
  const editPending = (item: PendingItem) => {
    const composer = useComposer.getState();
    const current = composer.drafts[sessionId] ?? { text: "", images: [], files: [] };
    // 斜杠 / ! 命令草稿不是普通消息：合并后回车会执行命令并吞掉待发正文，直接阻止取回
    if (current.text.startsWith("/") || current.text.startsWith("!")) {
      toast(t("q.edit_command_draft"));
      return;
    }
    composer.removePending(sessionId, item.key);
    const absorbed = absorbRefs(current, {
      text: item.mode === "ask" ? `/ask ${item.text}` : item.text,
      images: item.images,
      files: item.files,
    });
    composer.restore(sessionId, {
      ...absorbed,
      text: current.text.trim() ? `${current.text}\n${absorbed.text}` : absorbed.text,
    });
    editorRef.current?.focus();
  };

  /** 立即发送：空闲时直接发；运行中移到队首并中断当前轮，轮到后由 dispatchNext 发出。 */
  const sendPendingNow = (item: PendingItem) => {
    const composer = useComposer.getState();
    composer.removePending(sessionId, item.key);
    if (!running) {
      void sendPrompt(sessionId, toInput(item.text, item.images, item.files), {
        mode: item.mode,
      });
      return;
    }
    composer.pushPending(sessionId, item, true);
    void rpc
      .call("session/cancel", { session_id: sessionId })
      .catch((error) => toast.error(t("cb.interrupt_failed", { error: errorText(error) })));
  };

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
      else toast(t("cb.unsteer_late"));
    } catch (error) {
      toast.error(t("cb.unsteer_failed", { error: errorText(error) }));
    }
  };

  /** 编辑器按键：返回 true 表示已处理。输入法组字期间编辑器不会调用这里。 */
  const onKeyDown = (event: KeyboardEvent, view: EditorView): boolean => {
    const handled = () => {
      event.preventDefault();
      return true;
    };
    if (activeOverlay === "commands" && commandEntries.length > 0) {
      const subCount = activeSubmenu.flat.length;
      if (focusPane === "sub" && subCount > 0) {
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          const step = event.key === "ArrowDown" ? 1 : -1;
          setSubActiveIndex((index) => (index < 0 ? 0 : (index + step + subCount) % subCount));
          return handled();
        }
        if (event.key === "ArrowLeft" || event.key === "Escape") {
          setFocusPane("list");
          setSubActiveIndex(-1);
          return handled();
        }
        if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey && !event.altKey)) {
          const chosen = activeSubmenu.flat[Math.max(subActiveIndex, 0)];
          if (chosen) pickSubAction(chosen);
          return handled();
        }
      } else {
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          const step = event.key === "ArrowDown" ? 1 : -1;
          setActiveIndex((index) => stepEnabledIndex(commandEntries, index, step));
          return handled();
        }
        if (
          event.key === "ArrowRight" &&
          activeCommandEntry &&
          !activeCommandEntry.disabledReason &&
          subCount > 0
        ) {
          setFocusPane("sub");
          setSubActiveIndex(0);
          return handled();
        }
        if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey && !event.altKey)) {
          pickCommand(activeIndex);
          return handled();
        }
        if (event.key === "Escape") {
          setDismissedAt(text);
          return handled();
        }
      }
    } else if (activeOverlay === "args" && stage2?.kind === "choices" && stage2.items.length > 0) {
      const count = stage2.items.length;
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        const step = event.key === "ArrowDown" ? 1 : -1;
        setActiveIndex((index) => (index + step + count) % count);
        return handled();
      }
      if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey && !event.altKey)) {
        pickArgChoice(activeIndex);
        return handled();
      }
      if (event.key === "Escape") {
        setDismissedAt(text);
        return handled();
      }
    } else if (activeOverlay === "resources" && activeResourceSuggestions.length > 0) {
      const count = activeResourceSuggestions.length;
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        const step = event.key === "ArrowDown" ? 1 : -1;
        setActiveIndex((index) => (index + step + count) % count);
        return handled();
      }
      if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey && !event.altKey)) {
        pickResource(activeIndex);
        return handled();
      }
      if (event.key === "Escape") {
        setDismissedAt(text);
        return handled();
      }
    } else if (activeOverlay === "hint" && event.key === "Escape") {
      setDismissedAt(text);
      return handled();
    }
    if (event.key === "Escape" && running && !text) {
      stop();
      return handled();
    }
    // Shift+Enter / Alt+Enter 换行（与 TUI 的 newline 键位一致）
    if (event.key === "Enter" && (event.shiftKey || event.altKey)) {
      view.dispatch(view.state.replaceSelection("\n"), {
        userEvent: "input",
        scrollIntoView: true,
      });
      return handled();
    }
    if (event.key === "Enter") {
      send(event.ctrlKey || event.metaKey);
      return handled();
    }
    return false;
  };

  const onEditorChange = (value: string, caret: number, typed: boolean, deleted = false) => {
    const plan = PLAN_PREFIX.exec(value);
    if (plan && consumePlan(value.slice(plan[0].length))) return;
    setText(value);
    setCursor(caret);
    setTypingAt((prev) => (typed || (deleted && prev !== null) ? caret : null));
  };

  const onEditorCursor = (caret: number) => {
    setCursor(caret);
    setTypingAt(null);
  };

  const onEditorBlur = () => {
    setTypingAt(null);
  };

  const onChipClick = (entry: EditorChip, element: HTMLElement) => {
    if (entry.chip.kind !== "command" || !parsedCommand?.editable) return;
    chipAnchorRef.current = element;
    setTypingAt(null);
    setChipEditorOpen((open) => {
      if (open) {
        requestAnimationFrame(() => editorRef.current?.focus());
        return false;
      }
      return true;
    });
  };

  const applyChipEdit = (
    nextText: string,
    nextCaret: number,
    close: boolean,
    expectsText: boolean,
  ) => {
    useComposer.getState().setText(sessionId, nextText);
    setDismissedAt(close && !expectsText ? nextText : null);
    setPreferHintFor(null);
    setMissingParamError(false);
    setCursor(nextCaret);
    setTypingAt(null);
    if (close) {
      setChipEditorOpen(false);
      requestAnimationFrame(() => editorRef.current?.setCaret(nextCaret));
    }
  };

  const closeChipEditor = (restoreFocus: boolean) => {
    setChipEditorOpen(false);
    if (restoreFocus) requestAnimationFrame(() => editorRef.current?.focus());
  };

  const getChipAnchor = () =>
    editorRef.current?.commandChipElement() ?? chipAnchorRef.current ?? editorBoxRef.current;

  // ---------- 渲染 ----------

  if (readOnly) {
    return (
      <ComposerDock inline={inline}>
        <div className="flex min-h-14 items-center gap-3 rounded-2xl bg-muted px-4 py-3 text-sm text-muted-foreground ring-1 ring-inset ring-border-subtle">
          <LockIcon className="size-4 shrink-0" />
          <span className="min-w-0 flex-1">
            {t("cb.readonly", {
              held: holder
                ? t("cb.readonly_by", { frontend: holder.frontend, pid: holder.pid })
                : "",
            })}
          </span>
          {!info?.parent_session_id && (
            <Button
              size="sm"
              variant="outline"
              className="h-7 rounded-lg"
              onClick={() =>
                void reacquire(sessionId).catch((error) =>
                  toast.error(t("cb.acquire_failed", { error: errorText(error) })),
                )
              }
            >
              {t("cb.acquire")}
            </Button>
          )}
        </div>
      </ComposerDock>
    );
  }

  const queueKey = queueOnEnter ? "Enter" : "Ctrl+Enter";
  const activeKey = queueOnEnter ? "Ctrl+Enter" : "Enter";
  const placeholder = skillsWorking
    ? t("cb.skills_busy")
    : running
      ? t(runningEnter === "interrupt" ? "cb.ph_interrupt" : "cb.ph_steer", {
          send: activeKey,
          queue: queueKey,
        })
      : inline
        ? t("cb.ph_draft")
        : "";

  return (
    <ComposerDock inline={inline}>
      <div className="relative">
        {/* 会话输入区保留状态托盘；欢迎页状态条独立放在输入卡下方。 */}
        {/* biome-ignore lint/a11y/noStaticElementInteractions: 拖放区只承接文件，键盘用户走附件按钮 */}
        <div
          onDragOver={(event) => {
            if (!event.dataTransfer.types.includes("Files")) return;
            event.preventDefault();
            setDragging(true);
          }}
          onDragLeave={(event) => {
            // 进出子元素也会触发 dragleave；只有真正离开拖放区才收起高亮，避免闪烁。
            if (!event.currentTarget.contains(event.relatedTarget as Node | null))
              setDragging(false);
          }}
          onDrop={onDrop}
          className={cn(
            "relative",
            !inline && "rounded-[22px] bg-[var(--composer-tray)] p-1.5",
            dragging && "ring-2 ring-brand/40",
          )}
        >
          {!inline && <ComposerStatus sessionId={sessionId} todos={todos} />}
          {pending.length > 0 && (
            <div className="pb-1.5">
              <PendingList
                items={pending}
                onRemove={(item) => void removePending(item)}
                onEdit={editPending}
                onSendNow={sendPendingNow}
                onMove={(key, toIndex) =>
                  useComposer.getState().movePending(sessionId, key, toIndex)
                }
              />
            </div>
          )}

          {/* biome-ignore lint/a11y/noStaticElementInteractions: 点击空白处聚焦输入框只是鼠标便利 */}
          {/* biome-ignore lint/a11y/useKeyWithClickEvents: 键盘本就在输入框内 */}
          <div
            onClick={(event) => {
              if (event.target === event.currentTarget) editorRef.current?.focus();
            }}
            className={cn(
              "relative rounded-[16px] bg-card shadow-[var(--elevation-composer-inset)] transition-shadow",
              bashMode && "ring-1 ring-inset ring-warn/50",
            )}
          >
            <ShellModeNotice
              text={text}
              permission={isDraft ? draftPermission : info?.permission}
            />

            {images.length > 0 && (
              <Attachments variant="grid" className="px-3 pt-3">
                {images.map((image, index) => (
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
                    <button
                      type="button"
                      onClick={() =>
                        setPreviewImage({ src: imageUrl(image.source), name: image.name })
                      }
                      aria-label={t("th.image_preview")}
                      className="size-full rounded-lg focus-visible:outline-2 focus-visible:outline-brand"
                    >
                      <AttachmentPreview />
                    </button>
                    {/* 正文用 `#N` 指代这张图 */}
                    <span className="pointer-events-none absolute bottom-0.5 left-0.5 rounded bg-background/85 px-1 text-[10px] leading-4 font-medium text-foreground tabular-nums">
                      #{index + 1}
                    </span>
                    <AttachmentRemove label={t("cb.remove_image", { name: image.name })} />
                  </Attachment>
                ))}
              </Attachments>
            )}

            {files.length > 0 && (
              <Attachments variant="inline" className="px-3 pt-3">
                {files.map((file) => (
                  <Attachment
                    key={file.id}
                    data={{
                      id: file.id,
                      type: "file",
                      url: "",
                      filename: `#${file.name}`,
                      mediaType: "application/octet-stream",
                    }}
                    onRemove={() => useComposer.getState().removeFile(sessionId, file.id)}
                    title={file.path ?? (file.uploading ? t("cb.uploading") : t("cb.file_pending"))}
                  >
                    <AttachmentPreview
                      fallbackIcon={
                        file.uploading ? (
                          <Loader2Icon className="size-3 animate-spin text-muted-foreground" />
                        ) : undefined
                      }
                    />
                    <AttachmentInfo />
                    <span className="shrink-0 text-2xs text-muted-foreground tabular-nums">
                      {formatBytes(file.size)}
                    </span>
                    <AttachmentRemove label={t("cb.remove_file", { name: file.name })} />
                  </Attachment>
                ))}
              </Attachments>
            )}

            <Popover
              open={activeOverlay !== null}
              onOpenChange={(open) => {
                if (!open) setDismissedAt(text);
              }}
            >
              <PopoverAnchor asChild>
                <div ref={editorBoxRef}>
                  <ComposerEditor
                    ref={editorRef}
                    value={text}
                    chips={stableChips}
                    placeholder={placeholder}
                    ghostHint={ghostHint}
                    className={cn("text-sm leading-6 text-foreground", bashMode && "font-mono")}
                    minHeight={inline ? 76 : 64}
                    maxHeight={MAX_HEIGHT}
                    attributes={{
                      "aria-label": t("cb.input"),
                      "aria-autocomplete": "list",
                      ...(activeOverlay === "commands" ||
                      activeOverlay === "args" ||
                      activeOverlay === "resources"
                        ? {
                            "aria-controls": suggestionsId,
                            "aria-activedescendant": `${suggestionsId}-${activeIndex}`,
                          }
                        : {}),
                    }}
                    onChange={onEditorChange}
                    onCursor={onEditorCursor}
                    onBlur={onEditorBlur}
                    onChipClick={onChipClick}
                    onKeyDown={onKeyDown}
                    onPaste={onPaste}
                    onCopy={onCopy}
                  />
                </div>
              </PopoverAnchor>
              {activeOverlay === "commands" && (
                <CommandMenu
                  id={suggestionsId}
                  side={inline ? "bottom" : "top"}
                  entries={commandEntries}
                  active={activeIndex}
                  focusPane={focusPane}
                  subActive={subActiveIndex}
                  ctx={commandCtx}
                  onPickCommand={pickCommand}
                  onHoverCommand={(index) => {
                    setActiveIndex(index);
                    setFocusPane("list");
                    setSubActiveIndex(-1);
                  }}
                  onPickSubAction={pickSubAction}
                  onHoverSubAction={(index) => {
                    setFocusPane("sub");
                    setSubActiveIndex(index);
                  }}
                  inputRef={editorBoxRef}
                />
              )}
              {activeOverlay === "args" && stage2?.kind === "choices" && (
                <CommandArgMenu
                  id={suggestionsId}
                  side={inline ? "bottom" : "top"}
                  state={stage2}
                  active={activeIndex}
                  onPick={pickArgChoice}
                  onHover={setActiveIndex}
                  inputRef={editorBoxRef}
                />
              )}
              {activeOverlay === "hint" && stage2?.kind === "hint" && (
                <CommandParamHint
                  side={inline ? "bottom" : "top"}
                  state={stage2}
                  missingError={missingParamError}
                  inputRef={editorBoxRef}
                />
              )}
              {activeOverlay === "resources" && (
                <SuggestionMenu
                  id={suggestionsId}
                  side={inline ? "bottom" : "top"}
                  wide
                  heading={t("cb.resources")}
                  items={activeResourceSuggestions}
                  active={activeIndex}
                  onPick={pickResource}
                  onHover={setActiveIndex}
                  inputRef={editorBoxRef}
                />
              )}
            </Popover>

            {parsedCommand?.editable && (
              <CommandChipEditor
                open={chipEditorOpen}
                side={inline ? "bottom" : "top"}
                parsed={parsedCommand}
                text={text}
                ctx={commandCtx}
                getAnchorElement={getChipAnchor}
                onApply={applyChipEdit}
                onClose={closeChipEditor}
              />
            )}

            <div className="flex min-h-12 flex-wrap items-center gap-1 px-2.5 pb-2 sm:px-3">
              <input
                ref={fileInputRef}
                type="file"
                multiple
                hidden
                onChange={(event) => {
                  const files = [...(event.target.files ?? [])];
                  event.target.value = "";
                  void addFiles(files);
                }}
              />
              <AddContextMenu
                disabled={reading}
                canCommand={!text.trim() && !attached}
                canReference={!isDraft}
                onUpload={() => fileInputRef.current?.click()}
                onBrowseServer={browseServer}
                onInsert={insertTrigger}
                onClose={() => editorRef.current?.focus()}
              />
              <SessionOptions sessionId={sessionId} />
              <PlanToggle sessionId={sessionId} />

              <div className="ml-auto flex min-w-0 items-center gap-1">
                {context && context.context_window > 0 && (
                  <ContextRing sessionId={sessionId} context={context} />
                )}
                <ModelPicker sessionId={sessionId} />
                {running && !canSend ? (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <button
                        type="button"
                        onClick={stop}
                        className="ml-1 flex size-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-colors hover:bg-primary/85 motion-safe:active:scale-95"
                        aria-label={t("cb.stop_gen")}
                      >
                        <SquareIcon className="size-3 fill-current" />
                      </button>
                    </TooltipTrigger>
                    <TooltipContent side="top">{t("cb.stop_esc")}</TooltipContent>
                  </Tooltip>
                ) : running && !bashMode ? (
                  // 运行中有草稿：左半排队（Ctrl+Enter），右半按偏好补充或中断（Enter）
                  <ButtonGroup className="ml-1">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          variant="secondary"
                          size="icon-sm"
                          onClick={() => send(false, "queue")}
                          className="w-9 rounded-full pl-1 motion-safe:active:scale-95"
                          aria-label={t("cb.queue_send")}
                        >
                          <ListPlusIcon className="size-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">
                        {t("cb.queue_hint", { key: queueKey })}
                      </TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="icon-sm"
                          onClick={() => send(false, "active")}
                          className="w-9 rounded-full pr-1 hover:bg-primary/85 motion-safe:active:scale-95"
                          aria-label={
                            runningEnter === "interrupt"
                              ? t("cb.interrupt_send")
                              : t("cb.steer_send")
                          }
                        >
                          <ArrowUpIcon className="size-4" strokeWidth={2.4} />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">
                        {t(runningEnter === "interrupt" ? "cb.interrupt_hint" : "cb.steer_hint", {
                          key: activeKey,
                        })}
                      </TooltipContent>
                    </Tooltip>
                  </ButtonGroup>
                ) : (
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <button
                        type="button"
                        onClick={() => send()}
                        disabled={!canSend}
                        className="ml-1 flex size-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-colors hover:bg-primary/85 motion-safe:active:scale-95 disabled:bg-text-tertiary disabled:text-card"
                        aria-label={t("cb.send")}
                      >
                        <ArrowUpIcon className="size-4" strokeWidth={2.4} />
                      </button>
                    </TooltipTrigger>
                    <TooltipContent side="top">{t("cb.send_hint")}</TooltipContent>
                  </Tooltip>
                )}
              </div>
            </div>
          </div>
          <ImagePreviewDialog
            open={previewImage !== null}
            src={previewImage?.src ?? null}
            alt={previewImage?.name ?? t("th.image_preview")}
            title={t("th.image_preview")}
            onOpenChange={(open) => !open && setPreviewImage(null)}
          />
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
