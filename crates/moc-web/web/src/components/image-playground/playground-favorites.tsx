// 收藏视图：支持一级文件夹分组、拖拽图片或提示词组合入文件夹/退回收藏根、
// 新建与编辑提示词（含多张参考图 + 提示词组合收藏）以及直接上传本地参考图。

import {
  CheckIcon,
  CopyIcon,
  FolderIcon,
  FolderInputIcon,
  FolderPlusIcon,
  ImageIcon,
  PencilIcon,
  PlayIcon,
  SearchIcon,
  SparklesIcon,
  Trash2Icon,
  UploadIcon,
  XIcon,
} from "lucide-react";
import { type DragEvent, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuSub,
  ContextMenuSubContent,
  ContextMenuSubTrigger,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { useT } from "@/lib/i18n";
import { type FavoriteFolder, type FavoritePreset, pgImageUrl } from "@/lib/image-playground-api";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";
import { FAV_DRAG_MIME, ImageCard } from "./image-card";

type FolderFilter = "all" | "root" | string;

type DragPayload = {
  kind: "image" | "preset";
  id: string;
};

function parseDragPayload(event: DragEvent): DragPayload | null {
  const raw = event.dataTransfer.getData(FAV_DRAG_MIME);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Partial<DragPayload>;
    if ((parsed.kind === "image" || parsed.kind === "preset") && typeof parsed.id === "string") {
      return { kind: parsed.kind, id: parsed.id };
    }
  } catch {
    // 忽略非预期格式
  }
  return null;
}

export function PlaygroundFavoritesView() {
  const t = useT();
  const favoriteIds = useImagePlayground((s) => s.favoriteIds);
  const favoriteFolders = useImagePlayground((s) => s.favoriteFolders);
  const favoritePresets = useImagePlayground((s) => s.favoritePresets);
  const imagesById = useImagePlayground((s) => s.imagesById);
  const uploadFavorites = useImagePlayground((s) => s.uploadFavorites);
  const createFavoriteFolder = useImagePlayground((s) => s.createFavoriteFolder);
  const renameFavoriteFolder = useImagePlayground((s) => s.renameFavoriteFolder);
  const deleteFavoriteFolder = useImagePlayground((s) => s.deleteFavoriteFolder);
  const moveFavoriteImageToFolder = useImagePlayground((s) => s.moveFavoriteImageToFolder);
  const updateFavoritePreset = useImagePlayground((s) => s.updateFavoritePreset);
  const deleteFavoritePreset = useImagePlayground((s) => s.deleteFavoritePreset);
  const openPresetEditor = useImagePlayground((s) => s.openPresetEditor);
  const applyFavoritePreset = useImagePlayground((s) => s.applyFavoritePreset);

  const fileInputRef = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState("");
  const [activeFolder, setActiveFolder] = useState<FolderFilter>("all");
  const [draggingFiles, setDraggingFiles] = useState(false);
  const [dragOverTarget, setDragOverTarget] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");
  const [renamingFolderId, setRenamingFolderId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");

  // 当前所选的具体文件夹 ID（仅在选中自定义文件夹时有值，用于上传和新建组合时的默认归属）。
  const currentFolderId = activeFolder !== "all" && activeFolder !== "root" ? activeFolder : null;

  const allFavoriteImages = useMemo(
    () =>
      favoriteIds
        .map((id) => imagesById[id])
        .filter((img): img is NonNullable<typeof img> => Boolean(img)),
    [favoriteIds, imagesById],
  );

  // 每个分组（全部 / 根 / 各文件夹）的条目数（图片 + 提示词组合）。
  const counts = useMemo(() => {
    const byFolder: Record<string, number> = {};
    let root = 0;
    for (const img of allFavoriteImages) {
      if (img.favorite_folder_id) {
        byFolder[img.favorite_folder_id] = (byFolder[img.favorite_folder_id] ?? 0) + 1;
      } else {
        root += 1;
      }
    }
    for (const preset of favoritePresets) {
      if (preset.folder_id) {
        byFolder[preset.folder_id] = (byFolder[preset.folder_id] ?? 0) + 1;
      } else {
        root += 1;
      }
    }
    return {
      all: allFavoriteImages.length + favoritePresets.length,
      root,
      byFolder,
    };
  }, [allFavoriteImages, favoritePresets]);

  const filteredPresets = useMemo(() => {
    const q = query.trim().toLowerCase();
    return favoritePresets
      .filter((p) => {
        if (activeFolder === "root") return p.folder_id === null;
        if (activeFolder !== "all") return p.folder_id === activeFolder;
        return true;
      })
      .filter((p) => {
        if (!q) return true;
        return `${p.title} ${p.prompt}`.toLowerCase().includes(q);
      });
  }, [favoritePresets, activeFolder, query]);

  const filteredImages = useMemo(() => {
    const q = query.trim().toLowerCase();
    return allFavoriteImages
      .filter((img) => {
        if (activeFolder === "root") return img.favorite_folder_id === null;
        if (activeFolder !== "all") return img.favorite_folder_id === activeFolder;
        return true;
      })
      .filter((img) => {
        if (!q) return true;
        return `${img.prompt ?? ""} ${img.model ?? ""} ${img.filename}`.toLowerCase().includes(q);
      });
  }, [allFavoriteImages, activeFolder, query]);

  const listIds = useMemo(() => filteredImages.map((i) => i.id), [filteredImages]);
  const totalVisible = filteredPresets.length + filteredImages.length;

  const handleFiles = async (files: File[]) => {
    const images = files.filter((f) => f.type.startsWith("image/"));
    if (images.length === 0) return;
    setUploading(true);
    try {
      await uploadFavorites(images, currentFolderId);
      toast.success(t("ip.fav_uploaded", { n: images.length }));
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    } finally {
      setUploading(false);
    }
  };

  const handleCreateFolder = async () => {
    const name = newFolderName.trim();
    if (!name) {
      setCreatingFolder(false);
      return;
    }
    try {
      const folder = await createFavoriteFolder(name);
      setNewFolderName("");
      setCreatingFolder(false);
      setActiveFolder(folder.id);
      toast.success(t("ip.folder_created", { name: folder.name }));
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    }
  };

  const handleRenameFolder = async (folder: FavoriteFolder) => {
    const name = renameDraft.trim();
    setRenamingFolderId(null);
    if (!name || name === folder.name) return;
    try {
      await renameFavoriteFolder(folder.id, name);
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    }
  };

  const handleDeleteFolder = async (folder: FavoriteFolder) => {
    try {
      await deleteFavoriteFolder(folder.id);
      if (activeFolder === folder.id) setActiveFolder("all");
      toast.success(t("ip.folder_deleted", { name: folder.name }));
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    }
  };

  const handleDropToFolder = async (
    event: DragEvent,
    targetFolderId: string | null,
    targetLabel: string,
  ) => {
    event.preventDefault();
    event.stopPropagation();
    setDragOverTarget(null);
    const payload = parseDragPayload(event);
    if (!payload) return;
    try {
      if (payload.kind === "image") {
        await moveFavoriteImageToFolder(payload.id, targetFolderId);
      } else {
        await updateFavoritePreset(payload.id, { folder_id: targetFolderId });
      }
      toast.success(t("ip.moved_to_folder", { name: targetLabel }));
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    }
  };

  const onPageDrop = (event: DragEvent<HTMLDivElement>) => {
    setDraggingFiles(false);
    if (event.dataTransfer.types.includes(FAV_DRAG_MIME)) return;
    const files = [...event.dataTransfer.files];
    if (files.length === 0) return;
    event.preventDefault();
    void handleFiles(files);
  };

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: 拖放导入参考图
    <div
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes(FAV_DRAG_MIME)) return;
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        setDraggingFiles(true);
      }}
      onDragLeave={() => setDraggingFiles(false)}
      onDrop={onPageDrop}
      className={cn(
        "flex h-full min-h-0 flex-1 flex-col",
        draggingFiles && "ring-2 ring-inset ring-brand/40",
      )}
    >
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border-subtle px-3">
        <h1 className="text-sm font-medium text-foreground">{t("ip.nav_favorites")}</h1>
        <span className="font-mono text-xs tabular-nums text-text-tertiary">{totalVisible}</span>

        <div className="ml-auto flex items-center gap-1.5">
          <label className="hidden h-7 w-44 items-center gap-1.5 rounded-lg bg-muted/70 px-2.5 md:flex">
            <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t("ip.search_favorites")}
              className="min-w-0 flex-1 bg-transparent text-xs outline-none placeholder:text-text-tertiary"
            />
          </label>
          <Button
            size="xs"
            variant="outline"
            onClick={() => {
              setCreatingFolder(true);
              setNewFolderName("");
            }}
            className="h-7 shadow-none"
          >
            <FolderPlusIcon className="size-3.5" />
            <span className="hidden sm:inline">{t("ip.new_folder")}</span>
          </Button>
          <Button
            size="xs"
            variant="outline"
            onClick={() => openPresetEditor({ initialFolderId: currentFolderId })}
            className="h-7 shadow-none"
          >
            <SparklesIcon className="size-3.5" />
            <span className="hidden sm:inline">{t("ip.new_preset")}</span>
          </Button>
          <input
            ref={fileInputRef}
            type="file"
            accept="image/png,image/jpeg,image/webp,image/gif,image/bmp"
            multiple
            hidden
            onChange={(e) => {
              const files = [...(e.target.files ?? [])];
              e.target.value = "";
              void handleFiles(files);
            }}
          />
          <Button
            size="xs"
            variant="outline"
            disabled={uploading}
            onClick={() => fileInputRef.current?.click()}
            className="h-7 shadow-none"
          >
            <UploadIcon className="size-3.5" />
            {t("ip.upload_favorite")}
          </Button>
        </div>
      </header>

      {/* 文件夹分组条：支持点击筛选，也支持直接拖拽图片或组合卡片到任意文件夹 / 收藏根 */}
      <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b border-border-subtle bg-muted/20 px-4 py-2">
        <button
          type="button"
          onClick={() => setActiveFolder("all")}
          className={cn(
            "inline-flex h-7 items-center gap-1.5 rounded-lg px-2.5 text-xs transition-colors",
            activeFolder === "all"
              ? "bg-accent font-medium text-foreground"
              : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
          )}
        >
          <span>{t("ip.folder_all")}</span>
          <span className="font-mono text-2xs tabular-nums text-text-tertiary">{counts.all}</span>
        </button>

        <button
          type="button"
          onClick={() => setActiveFolder("root")}
          onDragOver={(e) => {
            if (!e.dataTransfer.types.includes(FAV_DRAG_MIME)) return;
            e.preventDefault();
            e.dataTransfer.dropEffect = "move";
            setDragOverTarget("root");
          }}
          onDragLeave={() => setDragOverTarget((cur) => (cur === "root" ? null : cur))}
          onDrop={(e) => void handleDropToFolder(e, null, t("ip.folder_root"))}
          className={cn(
            "inline-flex h-7 items-center gap-1.5 rounded-lg px-2.5 text-xs transition-all",
            activeFolder === "root"
              ? "bg-accent font-medium text-foreground"
              : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
            dragOverTarget === "root" && "bg-brand/15 text-brand ring-2 ring-brand",
          )}
        >
          <span>{t("ip.folder_root")}</span>
          <span className="font-mono text-2xs tabular-nums text-text-tertiary">{counts.root}</span>
        </button>

        {favoriteFolders.map((folder) => {
          const isRenaming = renamingFolderId === folder.id;
          const isDropHot = dragOverTarget === folder.id;
          const count = counts.byFolder[folder.id] ?? 0;

          if (isRenaming) {
            return (
              <form
                key={folder.id}
                onSubmit={(e) => {
                  e.preventDefault();
                  void handleRenameFolder(folder);
                }}
                className="inline-flex h-7 items-center gap-1 rounded-lg bg-background px-2 ring-1 ring-ring"
              >
                <FolderIcon className="size-3.5 text-muted-foreground" />
                <input
                  // biome-ignore lint/a11y/noAutofocus: 重命名文件夹内联输入框
                  autoFocus
                  value={renameDraft}
                  onChange={(e) => setRenameDraft(e.target.value)}
                  onBlur={() => void handleRenameFolder(folder)}
                  onKeyDown={(e) => e.key === "Escape" && setRenamingFolderId(null)}
                  className="w-24 bg-transparent text-xs outline-none"
                />
              </form>
            );
          }

          return (
            <ContextMenu key={folder.id}>
              <ContextMenuTrigger asChild>
                <button
                  type="button"
                  onClick={() => setActiveFolder(folder.id)}
                  onDragOver={(e) => {
                    if (!e.dataTransfer.types.includes(FAV_DRAG_MIME)) return;
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    setDragOverTarget(folder.id);
                  }}
                  onDragLeave={() => setDragOverTarget((cur) => (cur === folder.id ? null : cur))}
                  onDrop={(e) => void handleDropToFolder(e, folder.id, folder.name)}
                  className={cn(
                    "inline-flex h-7 items-center gap-1.5 rounded-lg px-2.5 text-xs transition-all",
                    activeFolder === folder.id
                      ? "bg-accent font-medium text-foreground"
                      : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
                    isDropHot && "bg-brand/15 text-brand ring-2 ring-brand",
                  )}
                >
                  <FolderIcon className="size-3.5 shrink-0" />
                  <span className="max-w-[140px] truncate">{folder.name}</span>
                  <span className="font-mono text-2xs tabular-nums text-text-tertiary">
                    {count}
                  </span>
                </button>
              </ContextMenuTrigger>
              <ContextMenuContent className="w-40 rounded-xl border-0 bg-popover p-1 shadow-[var(--elevation-popover)]">
                <ContextMenuItem
                  className="rounded-lg text-ui"
                  onSelect={() => {
                    setRenamingFolderId(folder.id);
                    setRenameDraft(folder.name);
                  }}
                >
                  <PencilIcon className="size-3.5" />
                  {t("ft.rename")}
                </ContextMenuItem>
                <ContextMenuSeparator className="bg-border-subtle" />
                <ContextMenuItem
                  variant="destructive"
                  className="rounded-lg text-ui"
                  onSelect={() => void handleDeleteFolder(folder)}
                >
                  <Trash2Icon className="size-3.5" />
                  {t("common.delete")}
                </ContextMenuItem>
              </ContextMenuContent>
            </ContextMenu>
          );
        })}

        {creatingFolder && (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void handleCreateFolder();
            }}
            className="inline-flex h-7 items-center gap-1 rounded-lg bg-background px-2 ring-1 ring-ring"
          >
            <FolderPlusIcon className="size-3.5 text-brand" />
            <input
              // biome-ignore lint/a11y/noAutofocus: 新建文件夹内联输入框
              autoFocus
              value={newFolderName}
              onChange={(e) => setNewFolderName(e.target.value)}
              onKeyDown={(e) => e.key === "Escape" && setCreatingFolder(false)}
              placeholder={t("ip.folder_name_ph")}
              className="w-28 bg-transparent text-xs outline-none placeholder:text-text-tertiary"
            />
            <button
              type="submit"
              aria-label={t("common.confirm")}
              className="text-brand hover:opacity-80"
            >
              <CheckIcon className="size-3.5" />
            </button>
            <button
              type="button"
              aria-label={t("common.cancel")}
              onClick={() => setCreatingFolder(false)}
              className="text-muted-foreground hover:text-foreground"
            >
              <XIcon className="size-3.5" />
            </button>
          </form>
        )}

        {favoriteFolders.length > 0 && (
          <span className="ml-auto hidden text-2xs text-text-tertiary lg:inline">
            {t("ip.folder_drag_hint")}
          </span>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-5">
        {totalVisible === 0 ? (
          <div className="flex h-full flex-col items-center justify-center gap-3 text-center">
            <p className="max-w-sm text-sm text-muted-foreground">
              {counts.all === 0
                ? t("ip.fav_empty_page")
                : query.trim()
                  ? t("sb.no_match")
                  : t("ip.folder_empty")}
            </p>
            <div className="flex flex-wrap items-center justify-center gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => openPresetEditor({ initialFolderId: currentFolderId })}
              >
                <SparklesIcon className="size-4" />
                {t("ip.new_preset")}
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={uploading}
                onClick={() => fileInputRef.current?.click()}
              >
                <UploadIcon className="size-4" />
                {t("ip.upload_favorite")}
              </Button>
            </div>
          </div>
        ) : (
          <div className="space-y-6">
            {/* 提示词 / 多图组合收藏区 */}
            {filteredPresets.length > 0 && (
              <section className="space-y-2.5">
                <div className="flex items-center gap-2">
                  <SparklesIcon className="size-3.5 text-brand" />
                  <h2 className="text-xs font-medium text-muted-foreground">
                    {t("ip.presets_section")}
                  </h2>
                  <span className="font-mono text-2xs tabular-nums text-text-tertiary">
                    {filteredPresets.length}
                  </span>
                </div>
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4">
                  {filteredPresets.map((preset) => (
                    <PresetCard
                      key={preset.id}
                      preset={preset}
                      folders={favoriteFolders}
                      onApply={() => {
                        void applyFavoritePreset(preset)
                          .then(() => toast.success(t("ip.preset_applied")))
                          .catch((err) =>
                            toast.error(t("ip.action_failed", { error: String(err) })),
                          );
                      }}
                      onEdit={() => openPresetEditor({ editing: preset })}
                      onMove={(folderId, folderName) => {
                        void updateFavoritePreset(preset.id, { folder_id: folderId })
                          .then(() => toast.success(t("ip.moved_to_folder", { name: folderName })))
                          .catch((err) =>
                            toast.error(t("ip.action_failed", { error: String(err) })),
                          );
                      }}
                      onDelete={() => {
                        void deleteFavoritePreset(preset.id)
                          .then(() => toast.success(t("ip.preset_deleted")))
                          .catch((err) =>
                            toast.error(t("ip.action_failed", { error: String(err) })),
                          );
                      }}
                    />
                  ))}
                </div>
              </section>
            )}

            {/* 单图参考收藏区 */}
            {filteredImages.length > 0 && (
              <section className="space-y-2.5">
                {filteredPresets.length > 0 && (
                  <div className="flex items-center gap-2">
                    <ImageIcon className="size-3.5 text-muted-foreground" />
                    <h2 className="text-xs font-medium text-muted-foreground">
                      {t("ip.images_section")}
                    </h2>
                    <span className="font-mono text-2xs tabular-nums text-text-tertiary">
                      {filteredImages.length}
                    </span>
                  </div>
                )}
                <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5">
                  {filteredImages.map((img) => (
                    <ImageCard
                      key={img.id}
                      image={img}
                      listIds={listIds}
                      aspectSquare
                      draggableFav
                    />
                  ))}
                </div>
              </section>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

function PresetCard({
  preset,
  folders,
  onApply,
  onEdit,
  onMove,
  onDelete,
}: {
  preset: FavoritePreset;
  folders: FavoriteFolder[];
  onApply: () => void;
  onEdit: () => void;
  onMove: (folderId: string | null, folderName: string) => void;
  onDelete: () => void;
}) {
  const t = useT();
  const imagesById = useImagePlayground((s) => s.imagesById);
  const validImages = preset.image_ids
    .map((id) => imagesById[id])
    .filter((img): img is NonNullable<typeof img> => Boolean(img));
  const previewImages = validImages.slice(0, 4);
  const extraCount = validImages.length - previewImages.length;

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        {/* biome-ignore lint/a11y/noStaticElementInteractions: 组合收藏卡片支持拖入文件夹，右键菜单同样提供移动选项 */}
        <div
          draggable
          onDragStart={(event) => {
            event.dataTransfer.setData(
              FAV_DRAG_MIME,
              JSON.stringify({ kind: "preset", id: preset.id }),
            );
            event.dataTransfer.effectAllowed = "move";
          }}
          className="group flex cursor-grab flex-col justify-between gap-2.5 rounded-xl bg-card p-3 ring-1 ring-inset ring-border-subtle transition-colors hover:ring-border active:cursor-grabbing"
        >
          <div className="space-y-2">
            <div className="flex items-start justify-between gap-2">
              <h3 className="line-clamp-1 text-ui font-medium text-foreground">{preset.title}</h3>
              {validImages.length > 0 && (
                <span className="inline-flex shrink-0 items-center gap-1 rounded-md bg-muted/70 px-1.5 py-0.5 font-mono text-2xs tabular-nums text-muted-foreground">
                  <ImageIcon className="size-3" />
                  {validImages.length}
                </span>
              )}
            </div>

            {previewImages.length > 0 && (
              <div className="flex items-center gap-1.5">
                {previewImages.map((img) => (
                  <img
                    key={img.id}
                    src={pgImageUrl(img.id)}
                    alt={img.filename}
                    loading="lazy"
                    draggable={false}
                    className="size-12 rounded-lg object-cover ring-1 ring-inset ring-border-subtle"
                  />
                ))}
                {extraCount > 0 && (
                  <span className="flex size-12 items-center justify-center rounded-lg bg-muted/60 font-mono text-xs text-muted-foreground">
                    +{extraCount}
                  </span>
                )}
              </div>
            )}

            {preset.prompt && (
              <p className="line-clamp-3 whitespace-pre-wrap text-xs leading-relaxed text-muted-foreground">
                {preset.prompt}
              </p>
            )}
          </div>

          <div className="flex items-center justify-end gap-1 pt-1">
            <Button
              type="button"
              size="xs"
              variant="ghost"
              className="h-6 px-2 text-xs text-muted-foreground hover:text-foreground"
              onClick={onEdit}
            >
              <PencilIcon className="size-3" />
              {t("ip.preset_edit")}
            </Button>
            <Button
              type="button"
              size="xs"
              variant="secondary"
              className="h-6 px-2 text-xs"
              onClick={onApply}
            >
              <PlayIcon className="size-3" />
              {t("ip.preset_use")}
            </Button>
          </div>
        </div>
      </ContextMenuTrigger>

      <ContextMenuContent className="w-48 rounded-xl border-0 bg-popover p-1 shadow-[var(--elevation-popover)]">
        <ContextMenuItem className="rounded-lg text-ui" onSelect={onApply}>
          <PlayIcon className="size-4" />
          {t("ip.preset_use")}
        </ContextMenuItem>
        <ContextMenuItem className="rounded-lg text-ui" onSelect={onEdit}>
          <PencilIcon className="size-4" />
          {t("ip.preset_edit")}
        </ContextMenuItem>
        {preset.prompt && (
          <ContextMenuItem
            className="rounded-lg text-ui"
            onSelect={() => {
              void navigator.clipboard.writeText(preset.prompt);
              toast.success(t("common.copied"));
            }}
          >
            <CopyIcon className="size-4" />
            {t("ip.ctx_copy_prompt")}
          </ContextMenuItem>
        )}
        {folders.length > 0 && (
          <ContextMenuSub>
            <ContextMenuSubTrigger className="rounded-lg text-ui">
              <FolderInputIcon className="size-4" />
              {t("ip.ctx_move_folder")}
            </ContextMenuSubTrigger>
            <ContextMenuSubContent className="w-44 rounded-xl border-0 bg-popover p-1 shadow-[var(--elevation-popover)]">
              <ContextMenuItem
                className="rounded-lg text-ui"
                onSelect={() => onMove(null, t("ip.folder_root"))}
              >
                <span className="flex-1 truncate">{t("ip.folder_root")}</span>
                {preset.folder_id === null && <CheckIcon className="size-3.5 text-brand" />}
              </ContextMenuItem>
              {folders.map((folder) => (
                <ContextMenuItem
                  key={folder.id}
                  className="rounded-lg text-ui"
                  onSelect={() => onMove(folder.id, folder.name)}
                >
                  <span className="flex-1 truncate">{folder.name}</span>
                  {preset.folder_id === folder.id && <CheckIcon className="size-3.5 text-brand" />}
                </ContextMenuItem>
              ))}
            </ContextMenuSubContent>
          </ContextMenuSub>
        )}
        <ContextMenuSeparator className="bg-border-subtle" />
        <ContextMenuItem variant="destructive" className="rounded-lg text-ui" onSelect={onDelete}>
          <Trash2Icon className="size-4" />
          {t("common.delete")}
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}
