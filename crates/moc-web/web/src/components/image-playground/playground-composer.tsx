// Image Playground 输入区：复用主界面的灰托盘与白卡语言、附件预览、拖放/粘贴上传；
// 裁剪掉 Agent 专属控件，新增参数设置浮层、仅限图片模型的选择器，以及与发送按钮平行的浮动收藏选择器。

import {
  ArrowUpIcon,
  BookmarkPlusIcon,
  CheckIcon,
  ChevronRightIcon,
  FolderIcon,
  ImageIcon,
  PaperclipIcon,
  Settings2Icon,
  SlidersHorizontalIcon,
  SparklesIcon,
  SquareIcon,
  StarIcon,
} from "lucide-react";
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
import { CHIP } from "@/components/chat/composer-parts";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useT } from "@/lib/i18n";
import {
  type EditEncoding,
  type ImageGenParams,
  type ImageModelRow,
  pgImageUrl,
} from "@/lib/image-playground-api";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";
import { openSettings } from "@/store/ui";

const UNSET = "__unset__";
const MAX_HEIGHT = 240;

const SIZE_PRESETS = [
  "auto",
  "1024x1024",
  "1536x1024",
  "1024x1536",
  "1792x1024",
  "1024x1792",
  "512x512",
  "2048x2048",
];

const ASPECT_PRESETS = ["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3", "21:9"];
const QUALITY_OPTIONS = ["auto", "low", "medium", "high", "standard", "hd"];
const FORMAT_OPTIONS = ["png", "webp", "jpeg", "gif"];
const BACKGROUND_OPTIONS = ["auto", "opaque", "transparent"];

function groupImageModels(models: ImageModelRow[]): [string, ImageModelRow[]][] {
  const map = new Map<string, ImageModelRow[]>();
  for (const m of models) {
    const list = map.get(m.provider);
    if (list) list.push(m);
    else map.set(m.provider, [m]);
  }
  return [...map.entries()];
}

function countActiveParams(params: ImageGenParams): number {
  let n = 0;
  for (const [k, v] of Object.entries(params)) {
    if (k === "extra_params") {
      if (v && typeof v === "object" && Object.keys(v).length > 0) n += 1;
    } else if (v !== undefined && v !== null && v !== "") {
      n += 1;
    }
  }
  return n;
}

export function PlaygroundComposer({
  sessionId,
  inline = false,
}: {
  sessionId: string;
  inline?: boolean;
}) {
  const t = useT();
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);

  const draft = useImagePlayground((s) => s.drafts[sessionId]);
  const running = useImagePlayground((s) => Boolean(s.runningBySession[sessionId]));
  const models = useImagePlayground((s) => s.models);
  const selectedModel = useImagePlayground((s) => s.selectedModel);
  const setSelectedModel = useImagePlayground((s) => s.setSelectedModel);
  const imagesById = useImagePlayground((s) => s.imagesById);
  const updateDraft = useImagePlayground((s) => s.updateDraft);
  const addUploadFiles = useImagePlayground((s) => s.addUploadFiles);
  const removeUploadFile = useImagePlayground((s) => s.removeUploadFile);
  const removeRefImage = useImagePlayground((s) => s.removeRefImage);
  const submitGenerate = useImagePlayground((s) => s.submitGenerate);
  const cancelGenerate = useImagePlayground((s) => s.cancelGenerate);

  const prompt = draft?.prompt ?? "";
  const uploads = draft?.uploads ?? [];
  const refImageIds = draft?.refImageIds ?? [];
  const params = draft?.params ?? {};

  const totalRefs = uploads.length + refImageIds.length;
  const canSend = prompt.trim().length > 0 && Boolean(selectedModel) && !running;

  const onSend = () => {
    if (!selectedModel) {
      toast.error(t("ip.no_model_selected"));
      return;
    }
    if (!canSend) return;
    void submitGenerate(sessionId).catch((err) =>
      toast.error(t("ip.action_failed", { error: String(err) })),
    );
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.nativeEvent.isComposing) return;
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      onSend();
    }
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = [...event.clipboardData.files].filter((f) => f.type.startsWith("image/"));
    if (files.length === 0) return;
    // 图文混贴时保留文字原生插入；纯图片或文件管理器带上的同名文本则拦截，避免把文件名贴进输入框。
    const plain = event.clipboardData.getData("text/plain").replace(/\r\n/g, "\n").trim();
    if (!plain || plain === files.map((file) => file.name).join("\n")) {
      event.preventDefault();
    }
    addUploadFiles(sessionId, files);
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    setDragging(false);
    const files = [...event.dataTransfer.files].filter((f) => f.type.startsWith("image/"));
    if (files.length === 0) return;
    event.preventDefault();
    addUploadFiles(sessionId, files);
  };

  const card = (
    // biome-ignore lint/a11y/noStaticElementInteractions: 拖放图片文件上传参考图区域
    <div
      onDragOver={(event) => {
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        setDragging(true);
      }}
      onDragLeave={(event) => {
        // 进出子元素也会触发 dragleave；只有真正离开拖放区才收起高亮，避免闪烁。
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDragging(false);
      }}
      onDrop={onDrop}
      className={cn(
        "relative",
        !inline && "rounded-[22px] bg-[var(--composer-tray)] p-1.5",
        dragging && "ring-2 ring-brand/40",
      )}
    >
      {/* biome-ignore lint/a11y/noStaticElementInteractions: 点击空白聚焦输入框 */}
      {/* biome-ignore lint/a11y/useKeyWithClickEvents: 键盘聚焦输入框 */}
      <div
        onClick={(e) => {
          if (e.target === e.currentTarget) textareaRef.current?.focus();
        }}
        className={cn(
          "relative rounded-[16px] bg-card transition-shadow",
          !inline && "shadow-[var(--elevation-composer-inset)]",
        )}
      >
        {/* 附件预览条：已选收藏/历史参考图 + 本次本地上传图 */}
        {totalRefs > 0 && (
          <Attachments variant="grid" className="px-3 pt-3">
            {refImageIds.map((id) => {
              const img = imagesById[id];
              return (
                <Attachment
                  key={`ref-${id}`}
                  data={{
                    id: `ref-${id}`,
                    type: "file",
                    filename: img?.filename || id,
                    mediaType: img?.mime_type || "image/png",
                    url: pgImageUrl(id),
                  }}
                  onRemove={() => removeRefImage(sessionId, id)}
                >
                  <AttachmentPreview />
                  <AttachmentRemove label={t("common.remove")} />
                </Attachment>
              );
            })}
            {uploads.map((up) => (
              <Attachment
                key={up.id}
                data={{
                  id: up.id,
                  type: "file",
                  filename: up.file.name || "image",
                  mediaType: up.file.type || "image/png",
                  url: up.previewUrl,
                }}
                onRemove={() => removeUploadFile(sessionId, up.id)}
              >
                <AttachmentPreview />
                <AttachmentRemove label={t("common.remove")} />
              </Attachment>
            ))}
          </Attachments>
        )}

        <textarea
          ref={textareaRef}
          value={prompt}
          rows={1}
          onChange={(e) => {
            updateDraft(sessionId, (prev) => ({ ...prev, prompt: e.target.value }));
            e.currentTarget.style.height = "auto";
            e.currentTarget.style.height = `${Math.min(e.currentTarget.scrollHeight, MAX_HEIGHT)}px`;
          }}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
          placeholder={totalRefs > 0 ? t("ip.ph_edit") : t("ip.ph_generate")}
          aria-label={t("ip.prompt")}
          className={cn(
            "block w-full resize-none bg-transparent px-4 pt-3.5 pb-1 text-chat leading-6 text-foreground outline-none placeholder:text-text-tertiary",
            inline ? "min-h-[72px]" : "min-h-[60px]",
          )}
          style={{ maxHeight: MAX_HEIGHT }}
        />

        <div className="flex min-h-11 flex-wrap items-center gap-1 px-2.5 pb-1.5 sm:px-3">
          <input
            ref={fileInputRef}
            type="file"
            accept="image/png,image/jpeg,image/webp,image/gif,image/bmp"
            multiple
            hidden
            onChange={(e) => {
              const files = [...(e.target.files ?? [])];
              e.target.value = "";
              addUploadFiles(sessionId, files);
            }}
          />

          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={() => fileInputRef.current?.click()}
                className={cn(CHIP, "w-8 justify-center px-0")}
                aria-label={t("ip.add_ref_image")}
              >
                <PaperclipIcon className="size-4" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="top">{t("ip.add_ref_image")}</TooltipContent>
          </Tooltip>

          <ParamsPopover
            key={sessionId}
            params={params}
            onChange={(next) => updateDraft(sessionId, (prev) => ({ ...prev, params: next }))}
          />

          {(prompt.trim() || totalRefs > 0) && (
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  onClick={() =>
                    useImagePlayground.getState().openPresetEditor({
                      initialPrompt: prompt.trim(),
                      initialImageIds: [...refImageIds],
                      initialFiles: uploads.map((u) => u.file),
                    })
                  }
                  className={cn(CHIP, "w-8 justify-center px-0")}
                  aria-label={t("ip.save_as_preset")}
                >
                  <BookmarkPlusIcon className="size-4" />
                </button>
              </TooltipTrigger>
              <TooltipContent side="top">{t("ip.save_as_preset")}</TooltipContent>
            </Tooltip>
          )}

          <div className="ml-auto flex min-w-0 items-center gap-1">
            <ImageModelPicker models={models} value={selectedModel} onChange={setSelectedModel} />

            {/* 与发送按钮平行的浮动收藏按钮：点击展开收藏列表，支持按文件夹浏览、附带参考图或一键应用提示词组合 */}
            <FavoritesQuickPicker sessionId={sessionId} selectedIds={refImageIds} />

            {running ? (
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    onClick={() => void cancelGenerate(sessionId)}
                    className="ml-0.5 flex size-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-colors hover:bg-primary/85"
                    aria-label={t("cb.stop_gen")}
                  >
                    <SquareIcon className="size-3 fill-current" />
                  </button>
                </TooltipTrigger>
                <TooltipContent side="top">{t("cb.stop_gen")}</TooltipContent>
              </Tooltip>
            ) : (
              <button
                type="button"
                onClick={onSend}
                disabled={!canSend}
                className="ml-0.5 flex size-8 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-colors hover:bg-primary/85 disabled:bg-text-tertiary disabled:text-card"
                aria-label={totalRefs > 0 ? t("ip.send_edit") : t("ip.send_generate")}
              >
                <ArrowUpIcon className="size-4" strokeWidth={2.4} />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );

  if (inline) return <div className="w-full">{card}</div>;
  return (
    <div className="moc-chat-pane relative shrink-0 bg-background">
      <div className="pointer-events-none absolute inset-x-0 -top-8 h-8 bg-gradient-to-t from-background to-transparent" />
      <div className="moc-chat-column pt-1 pb-5">{card}</div>
    </div>
  );
}

/** 浮动收藏选择器：与发送按钮平行，支持文件夹筛选、点击图片附带参考图、点击提示词/组合一键填入提示词与多张参考图。 */
function FavoritesQuickPicker({
  sessionId,
  selectedIds,
}: {
  sessionId: string;
  selectedIds: string[];
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [folderFilter, setFolderFilter] = useState<"all" | "root" | string>("all");
  const favoriteIds = useImagePlayground((s) => s.favoriteIds);
  const favoriteFolders = useImagePlayground((s) => s.favoriteFolders);
  const favoritePresets = useImagePlayground((s) => s.favoritePresets);
  const imagesById = useImagePlayground((s) => s.imagesById);
  const toggleRefImage = useImagePlayground((s) => s.toggleRefImage);
  const applyFavoritePreset = useImagePlayground((s) => s.applyFavoritePreset);
  const setView = useImagePlayground((s) => s.setView);

  const filteredPresets = useMemo(() => {
    const q = query.trim().toLowerCase();
    return favoritePresets
      .filter((p) => {
        if (folderFilter === "root") return p.folder_id === null;
        if (folderFilter !== "all") return p.folder_id === folderFilter;
        return true;
      })
      .filter((p) => {
        if (!q) return true;
        return `${p.title} ${p.prompt}`.toLowerCase().includes(q);
      });
  }, [favoritePresets, folderFilter, query]);

  const items = useMemo(() => {
    const q = query.trim().toLowerCase();
    return favoriteIds
      .map((id) => imagesById[id])
      .filter((img): img is NonNullable<typeof img> => Boolean(img))
      .filter((img) => {
        if (folderFilter === "root") return img.favorite_folder_id === null;
        if (folderFilter !== "all") return img.favorite_folder_id === folderFilter;
        return true;
      })
      .filter((img) => {
        if (!q) return true;
        return `${img.prompt ?? ""} ${img.filename}`.toLowerCase().includes(q);
      });
  }, [favoriteIds, imagesById, folderFilter, query]);

  const selectedCount = selectedIds.filter((id) => favoriteIds.includes(id)).length;
  const totalFavorites = favoriteIds.length + favoritePresets.length;
  const hasVisible = filteredPresets.length > 0 || items.length > 0;

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <Tooltip>
        <TooltipTrigger asChild>
          <PopoverTrigger asChild>
            <button
              type="button"
              aria-label={t("ip.pick_favorites")}
              className={cn(
                "relative flex size-8 shrink-0 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-accent hover:text-foreground data-[state=open]:bg-accent data-[state=open]:text-foreground",
                selectedCount > 0 && "text-brand",
              )}
            >
              <StarIcon className={cn("size-4", selectedCount > 0 && "fill-current")} />
              {selectedCount > 0 && (
                <span className="absolute -top-0.5 -right-0.5 flex size-4 items-center justify-center rounded-full bg-brand text-2xs font-medium text-brand-foreground tabular-nums">
                  {selectedCount}
                </span>
              )}
            </button>
          </PopoverTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">{t("ip.pick_favorites")}</TooltipContent>
      </Tooltip>

      <PopoverContent
        align="end"
        side="top"
        sideOffset={8}
        className="w-[380px] rounded-xl border-0 bg-popover p-2.5 shadow-[var(--elevation-popover)]"
      >
        <div className="mb-2 flex items-center justify-between gap-2 px-0.5">
          <span className="text-xs font-medium text-muted-foreground">
            {t("ip.fav_picker_title")}
          </span>
          <button
            type="button"
            onClick={() => {
              setOpen(false);
              setView("favorites");
            }}
            className="text-xs text-muted-foreground hover:text-foreground"
          >
            {t("ip.manage_favorites")}
          </button>
        </div>

        {/* 文件夹快速筛选条 */}
        {favoriteFolders.length > 0 && (
          <div className="mb-2 flex items-center gap-1 overflow-x-auto pb-0.5">
            <button
              type="button"
              onClick={() => setFolderFilter("all")}
              className={cn(
                "shrink-0 rounded-md px-2 py-0.5 text-2xs transition-colors",
                folderFilter === "all"
                  ? "bg-accent font-medium text-foreground"
                  : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
              )}
            >
              {t("ip.folder_all")}
            </button>
            <button
              type="button"
              onClick={() => setFolderFilter("root")}
              className={cn(
                "shrink-0 rounded-md px-2 py-0.5 text-2xs transition-colors",
                folderFilter === "root"
                  ? "bg-accent font-medium text-foreground"
                  : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
              )}
            >
              {t("ip.folder_root")}
            </button>
            {favoriteFolders.map((folder) => (
              <button
                key={folder.id}
                type="button"
                onClick={() => setFolderFilter(folder.id)}
                className={cn(
                  "inline-flex shrink-0 items-center gap-1 rounded-md px-2 py-0.5 text-2xs transition-colors",
                  folderFilter === folder.id
                    ? "bg-accent font-medium text-foreground"
                    : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
                )}
              >
                <FolderIcon className="size-3" />
                <span className="max-w-[96px] truncate">{folder.name}</span>
              </button>
            ))}
          </div>
        )}

        {totalFavorites > 6 && (
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("ip.search_favorites")}
            className="mb-2 h-7 rounded-lg text-xs shadow-none"
          />
        )}

        {!hasVisible ? (
          <div className="py-8 text-center text-xs text-text-tertiary">
            {totalFavorites === 0 ? t("ip.fav_empty_hint") : t("sb.no_match")}
          </div>
        ) : (
          <div className="max-h-72 space-y-2.5 overflow-y-auto p-0.5">
            {filteredPresets.length > 0 && (
              <div className="space-y-1">
                <div className="flex items-center gap-1 px-0.5 text-2xs font-medium text-text-tertiary">
                  <SparklesIcon className="size-3 text-brand" />
                  <span>{t("ip.presets_section")}</span>
                </div>
                <div className="space-y-1">
                  {filteredPresets.map((preset) => {
                    const thumbs = preset.image_ids
                      .map((id) => imagesById[id])
                      .filter((img): img is NonNullable<typeof img> => Boolean(img));
                    return (
                      <button
                        key={preset.id}
                        type="button"
                        onClick={() => {
                          void applyFavoritePreset(preset, sessionId)
                            .then(() => {
                              setOpen(false);
                              toast.success(t("ip.preset_applied"));
                            })
                            .catch((err) =>
                              toast.error(t("ip.action_failed", { error: String(err) })),
                            );
                        }}
                        className="flex w-full items-center gap-2 rounded-lg bg-muted/40 p-1.5 text-left transition-colors hover:bg-accent"
                      >
                        {thumbs.length > 0 ? (
                          <div className="relative flex shrink-0 -space-x-2">
                            {thumbs.slice(0, 2).map((img) => (
                              <img
                                key={img.id}
                                src={pgImageUrl(img.id)}
                                alt={img.filename}
                                loading="lazy"
                                className="size-9 rounded-md object-cover ring-1 ring-background"
                              />
                            ))}
                            {thumbs.length > 2 && (
                              <span className="flex size-9 items-center justify-center rounded-md bg-muted font-mono text-2xs text-muted-foreground ring-1 ring-background">
                                +{thumbs.length - 2}
                              </span>
                            )}
                          </div>
                        ) : (
                          <span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-muted text-brand">
                            <SparklesIcon className="size-4" />
                          </span>
                        )}
                        <div className="min-w-0 flex-1">
                          <div className="flex items-center gap-1.5">
                            <span className="truncate text-xs font-medium text-foreground">
                              {preset.title}
                            </span>
                            {thumbs.length > 0 && (
                              <span className="inline-flex shrink-0 items-center gap-0.5 font-mono text-2xs text-text-tertiary">
                                <ImageIcon className="size-2.5" />
                                {thumbs.length}
                              </span>
                            )}
                          </div>
                          {preset.prompt && (
                            <p className="line-clamp-1 text-2xs text-muted-foreground">
                              {preset.prompt}
                            </p>
                          )}
                        </div>
                      </button>
                    );
                  })}
                </div>
              </div>
            )}

            {items.length > 0 && (
              <div className="space-y-1">
                {filteredPresets.length > 0 && (
                  <div className="flex items-center gap-1 px-0.5 text-2xs font-medium text-text-tertiary">
                    <ImageIcon className="size-3" />
                    <span>{t("ip.images_section")}</span>
                  </div>
                )}
                <div className="grid grid-cols-3 gap-1.5">
                  {items.map((img) => {
                    const picked = selectedIds.includes(img.id);
                    const isAvif = img.format.toLowerCase() === "avif";
                    return (
                      <button
                        key={img.id}
                        type="button"
                        disabled={isAvif}
                        onClick={() => !isAvif && toggleRefImage(sessionId, img.id)}
                        title={isAvif ? t("ip.convert_avif_note") : img.prompt || img.filename}
                        className={cn(
                          "group relative aspect-square overflow-hidden rounded-lg bg-muted/40 ring-1 ring-inset transition-opacity",
                          isAvif
                            ? "cursor-not-allowed opacity-45 ring-border-subtle"
                            : picked
                              ? "ring-2 ring-brand"
                              : "ring-border-subtle hover:opacity-90",
                        )}
                      >
                        <img
                          src={pgImageUrl(img.id)}
                          alt={img.filename}
                          loading="lazy"
                          className="size-full object-cover"
                        />
                        {picked && (
                          <span className="absolute top-1 right-1 flex size-5 items-center justify-center rounded-full bg-brand text-brand-foreground">
                            <CheckIcon className="size-3" />
                          </span>
                        )}
                      </button>
                    );
                  })}
                </div>
              </div>
            )}
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}

/** 图片模型选择器：仅展示已勾选 image_gen 的模型，不写死任何模型名。 */
function ImageModelPicker({
  models,
  value,
  onChange,
}: {
  models: ImageModelRow[];
  value: string;
  onChange: (id: string) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const current = models.find((m) => m.id === value);
  const groups = useMemo(() => groupImageModels(models), [models]);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button type="button" className={CHIP} aria-label={t("ip.pick_model")}>
          <span className="max-w-[180px] truncate">
            {current ? current.name : t("ip.no_model")}
          </span>
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        side="top"
        sideOffset={8}
        className="w-72 overflow-hidden rounded-xl border-0 bg-popover p-0 shadow-[var(--elevation-popover)]"
      >
        <Command loop>
          <CommandInput placeholder={t("mp.search")} className="h-9 text-ui" />
          <CommandList className="max-h-72 p-1">
            <CommandEmpty className="py-6 text-center text-xs text-text-tertiary">
              {models.length === 0 ? t("ip.no_image_models_hint") : t("mp.no_match")}
            </CommandEmpty>
            {groups.map(([provider, list]) => (
              <CommandGroup key={provider} heading={provider} className="p-0 pb-1">
                {list.map((model) => (
                  <CommandItem
                    key={model.id}
                    value={model.id}
                    onSelect={() => {
                      onChange(model.id);
                      setOpen(false);
                    }}
                    className="gap-2 rounded-lg px-2 py-1.5"
                  >
                    <span className="min-w-0 flex-1 truncate text-ui text-foreground">
                      {model.name}
                    </span>
                    <CheckIcon
                      className={cn(
                        "size-3.5 text-muted-foreground",
                        model.id !== value && "invisible",
                      )}
                    />
                  </CommandItem>
                ))}
              </CommandGroup>
            ))}
          </CommandList>
        </Command>
        <button
          type="button"
          onClick={() => {
            setOpen(false);
            openSettings("providers");
          }}
          className="flex h-9 w-full items-center gap-2 border-t border-border-subtle px-3 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          <Settings2Icon className="size-3.5" />
          {t("mp.manage")}
        </button>
      </PopoverContent>
    </Popover>
  );
}

/** 参数设置浮层：全部字段默认不指定（不发送），兼容 OpenAI 与第三方扩展。 */
function ParamsPopover({
  params,
  onChange,
}: {
  params: ImageGenParams;
  onChange: (next: ImageGenParams) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const serializedExtra = useMemo(
    () =>
      params.extra_params && Object.keys(params.extra_params).length > 0
        ? JSON.stringify(params.extra_params, null, 2)
        : "",
    [params.extra_params],
  );
  const [extraText, setExtraText] = useState(serializedExtra);
  const [extraErr, setExtraErr] = useState<string | null>(null);

  useEffect(() => {
    setExtraText(serializedExtra);
    setExtraErr(null);
  }, [serializedExtra]);

  const activeCount = countActiveParams(params);

  const patch = (partial: Partial<ImageGenParams>) => {
    const next: ImageGenParams = { ...params, ...partial };
    for (const key of Object.keys(next) as (keyof ImageGenParams)[]) {
      if (next[key] === undefined || next[key] === "") {
        delete next[key];
      }
    }
    onChange(next);
  };

  const commitExtra = (raw: string) => {
    const trimmed = raw.trim();
    if (!trimmed) {
      setExtraErr(null);
      patch({ extra_params: undefined });
      return;
    }
    try {
      const parsed: unknown = JSON.parse(trimmed);
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
        setExtraErr(t("set.err_obj"));
        return;
      }
      setExtraErr(null);
      patch({ extra_params: parsed as Record<string, unknown> });
    } catch (error) {
      setExtraErr(t("set.err_json", { error: String(error) }));
    }
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button type="button" className={CHIP} aria-label={t("ip.params")}>
          <SlidersHorizontalIcon className="size-3.5" />
          <span>{t("ip.params")}</span>
          {activeCount > 0 && (
            <span className="font-mono text-2xs tabular-nums text-brand">{activeCount}</span>
          )}
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        side="top"
        sideOffset={8}
        className="max-h-[75vh] w-96 overflow-y-auto rounded-xl border-0 bg-popover p-3 shadow-[var(--elevation-popover)]"
      >
        <div className="mb-2.5 flex items-center justify-between">
          <span className="text-xs font-medium text-muted-foreground">{t("ip.params")}</span>
          {activeCount > 0 && (
            <button
              type="button"
              onClick={() => {
                setExtraText("");
                setExtraErr(null);
                onChange({});
              }}
              className="text-xs text-text-tertiary hover:text-foreground"
            >
              {t("common.clear")}
            </button>
          )}
        </div>

        <div className="space-y-3 text-xs">
          {/* size：支持预设或自由输入 */}
          <div className="space-y-1">
            <div>
              <div className="font-mono text-foreground">size</div>
              <p className="text-2xs leading-4 text-text-tertiary">{t("ip.param_size_desc")}</p>
            </div>
            <Input
              value={params.size ?? ""}
              onChange={(e) => patch({ size: e.target.value || undefined })}
              placeholder={t("ip.param_unset")}
              list="moc-ip-size-presets"
              className="h-7 rounded-lg font-mono text-xs shadow-none"
            />
            <datalist id="moc-ip-size-presets">
              {SIZE_PRESETS.map((s) => (
                <option key={s} value={s} />
              ))}
            </datalist>
          </div>

          <div className="grid grid-cols-2 gap-2.5">
            {/* n */}
            <div className="space-y-1">
              <div>
                <div className="font-mono text-foreground">n</div>
                <p className="text-2xs leading-4 text-text-tertiary">{t("ip.param_n_desc")}</p>
              </div>
              <Input
                type="number"
                min={1}
                max={10}
                value={params.n ?? ""}
                onChange={(e) => {
                  const val = Number.parseInt(e.target.value, 10);
                  patch({ n: Number.isFinite(val) && val > 0 ? Math.min(val, 10) : undefined });
                }}
                placeholder="1"
                className="h-7 rounded-lg font-mono text-xs tabular-nums shadow-none"
              />
            </div>

            {/* quality */}
            <div className="space-y-1">
              <div>
                <div className="font-mono text-foreground">quality</div>
                <p className="text-2xs leading-4 text-text-tertiary">
                  {t("ip.param_quality_desc")}
                </p>
              </div>
              <Select
                value={params.quality ?? UNSET}
                onValueChange={(v) => patch({ quality: v === UNSET ? undefined : v })}
              >
                <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                  {QUALITY_OPTIONS.map((q) => (
                    <SelectItem key={q} value={q}>
                      {q}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          <div className="grid grid-cols-2 gap-2.5">
            {/* output_format（含 gif） */}
            <div className="space-y-1">
              <div>
                <div className="font-mono text-foreground">output_format</div>
                <p className="text-2xs leading-4 text-text-tertiary">
                  {t("ip.param_output_format_desc")}
                </p>
              </div>
              <Select
                value={params.output_format ?? UNSET}
                onValueChange={(v) => patch({ output_format: v === UNSET ? undefined : v })}
              >
                <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                  {FORMAT_OPTIONS.map((fmt) => (
                    <SelectItem key={fmt} value={fmt}>
                      {fmt}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            {/* aspect_ratio（第三方兼容网关常用） */}
            <div className="space-y-1">
              <div>
                <div className="font-mono text-foreground">aspect_ratio</div>
                <p className="text-2xs leading-4 text-text-tertiary">
                  {t("ip.param_aspect_ratio_desc")}
                </p>
              </div>
              <Select
                value={params.aspect_ratio ?? UNSET}
                onValueChange={(v) => patch({ aspect_ratio: v === UNSET ? undefined : v })}
              >
                <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                  {ASPECT_PRESETS.map((r) => (
                    <SelectItem key={r} value={r}>
                      {r}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          <button
            type="button"
            onClick={() => setShowAdvanced((v) => !v)}
            className="flex items-center gap-1 pt-1 text-xs text-muted-foreground hover:text-foreground"
          >
            <ChevronRightIcon
              className={cn("size-3.5 transition-transform", showAdvanced && "rotate-90")}
            />
            {t("ip.params_advanced")}
          </button>

          {showAdvanced && (
            <div className="space-y-3 border-t border-border-subtle pt-2.5">
              <div className="grid grid-cols-2 gap-2.5">
                <div className="space-y-1">
                  <div>
                    <div className="font-mono text-foreground">background</div>
                    <p className="text-2xs leading-4 text-text-tertiary">
                      {t("ip.param_background_desc")}
                    </p>
                  </div>
                  <Select
                    value={params.background ?? UNSET}
                    onValueChange={(v) => patch({ background: v === UNSET ? undefined : v })}
                  >
                    <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                      {BACKGROUND_OPTIONS.map((b) => (
                        <SelectItem key={b} value={b}>
                          {b}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>

                <div className="space-y-1">
                  <div>
                    <div className="font-mono text-foreground">image_size</div>
                    <p className="text-2xs leading-4 text-text-tertiary">
                      {t("ip.param_image_size_desc")}
                    </p>
                  </div>
                  <Select
                    value={params.image_size ?? UNSET}
                    onValueChange={(v) => patch({ image_size: v === UNSET ? undefined : v })}
                  >
                    <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                      {["1K", "2K", "4K"].map((v) => (
                        <SelectItem key={v} value={v}>
                          {v}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
              </div>

              <div className="grid grid-cols-2 gap-2.5">
                <div className="space-y-1">
                  <div>
                    <div className="font-mono text-foreground">input_fidelity</div>
                    <p className="text-2xs leading-4 text-text-tertiary">
                      {t("ip.param_input_fidelity_desc")}
                    </p>
                  </div>
                  <Select
                    value={params.input_fidelity ?? UNSET}
                    onValueChange={(v) => patch({ input_fidelity: v === UNSET ? undefined : v })}
                  >
                    <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                      {["low", "high"].map((v) => (
                        <SelectItem key={v} value={v}>
                          {v}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>

                <div className="space-y-1">
                  <div>
                    <div className="font-mono text-foreground">response_format</div>
                    <p className="text-2xs leading-4 text-text-tertiary">
                      {t("ip.param_response_format_desc")}
                    </p>
                  </div>
                  <Select
                    value={params.response_format ?? UNSET}
                    onValueChange={(v) => patch({ response_format: v === UNSET ? undefined : v })}
                  >
                    <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={UNSET}>{t("ip.param_unset")}</SelectItem>
                      {["b64_json", "url"].map((v) => (
                        <SelectItem key={v} value={v}>
                          {v}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
              </div>

              <div className="grid grid-cols-2 gap-2.5">
                <div className="space-y-1">
                  <div>
                    <div className="font-mono text-foreground">output_compression</div>
                    <p className="text-2xs leading-4 text-text-tertiary">
                      {t("ip.param_output_compression_desc")}
                    </p>
                  </div>
                  <Input
                    type="number"
                    min={0}
                    max={100}
                    value={params.output_compression ?? ""}
                    onChange={(e) => {
                      const val = Number.parseInt(e.target.value, 10);
                      patch({
                        output_compression: Number.isFinite(val)
                          ? Math.max(0, Math.min(100, val))
                          : undefined,
                      });
                    }}
                    placeholder="0–100"
                    className="h-7 rounded-lg font-mono text-xs tabular-nums shadow-none"
                  />
                </div>

                <div className="space-y-1">
                  <div>
                    <div className="font-mono text-foreground">edit_encoding</div>
                    <p className="text-2xs leading-4 text-text-tertiary">
                      {t("ip.param_edit_encoding_desc")}
                    </p>
                  </div>
                  <Select
                    value={params.edit_encoding ?? UNSET}
                    onValueChange={(v) =>
                      patch({ edit_encoding: v === UNSET ? undefined : (v as EditEncoding) })
                    }
                  >
                    <SelectTrigger size="sm" className="h-7 w-full rounded-lg text-xs shadow-none">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value={UNSET}>{t("common.default")}</SelectItem>
                      <SelectItem value="multipart">multipart (image[])</SelectItem>
                      <SelectItem value="multipart_repeat_image">multipart (image)</SelectItem>
                      <SelectItem value="json">json (images[])</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
              </div>

              <div className="space-y-1">
                <div>
                  <div className="font-mono text-foreground">extra_params (JSON)</div>
                  <p className="text-2xs leading-4 text-text-tertiary">
                    {t("ip.param_extra_params_desc")}
                  </p>
                </div>
                <Textarea
                  value={extraText}
                  onChange={(e) => setExtraText(e.target.value)}
                  onBlur={() => commitExtra(extraText)}
                  placeholder='{"seed": 42}'
                  className="min-h-16 rounded-lg font-mono text-xs shadow-none"
                />
                {extraErr && <p className="text-2xs text-destructive">{extraErr}</p>}
              </div>
            </div>
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
