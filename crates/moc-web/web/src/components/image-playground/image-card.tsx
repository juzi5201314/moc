// 通用图片展示卡片：用于会话输出、画廊与收藏三处。
// 右上角半透明星标按钮收藏，右键菜单支持在系统文件管理器定位、以此图编辑、下载、删除等。

import {
  ArrowRightLeftIcon,
  CheckIcon,
  CopyIcon,
  DownloadIcon,
  ExternalLinkIcon,
  EyeIcon,
  FolderInputIcon,
  FolderOpenIcon,
  PencilIcon,
  StarIcon,
  Trash2Icon,
} from "lucide-react";
import { toast } from "sonner";
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
import {
  type ConvertFormat,
  type PlaygroundImage,
  pgImageUrl,
  revealPgImage,
} from "@/lib/image-playground-api";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";

export const FAV_DRAG_MIME = "application/x-moc-fav-item";

export type ConvertOption = {
  format: ConvertFormat;
  label: string;
  lossy: boolean;
};

export const STATIC_CONVERT_OPTIONS: ConvertOption[] = [
  { format: "webp", label: "WebP", lossy: true },
  { format: "webp_lossless", label: "WebP (Lossless)", lossy: false },
  { format: "avif", label: "AVIF", lossy: true },
  { format: "jpeg", label: "JPEG", lossy: true },
  { format: "png", label: "PNG", lossy: false },
];

export const ANIMATED_CONVERT_OPTIONS: ConvertOption[] = [
  { format: "webp_animated", label: "WebP (Animated)", lossy: true },
  { format: "apng", label: "APNG", lossy: false },
];

export function convertOptionLabel(format: ConvertFormat): string {
  return (
    [...STATIC_CONVERT_OPTIONS, ...ANIMATED_CONVERT_OPTIONS].find((o) => o.format === format)
      ?.label ?? format.toUpperCase()
  );
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

export function ImageCard({
  image,
  listIds,
  className,
  aspectSquare = false,
  draggableFav = false,
}: {
  image: PlaygroundImage;
  listIds?: string[];
  className?: string;
  aspectSquare?: boolean;
  draggableFav?: boolean;
}) {
  const t = useT();
  const toggleFavorite = useImagePlayground((s) => s.toggleFavorite);
  const moveFavoriteImageToFolder = useImagePlayground((s) => s.moveFavoriteImageToFolder);
  const favoriteFolders = useImagePlayground((s) => s.favoriteFolders);
  const editWithImage = useImagePlayground((s) => s.editWithImage);
  const setLightbox = useImagePlayground((s) => s.setLightbox);
  const setConfirmDeleteImage = useImagePlayground((s) => s.setConfirmDeleteImage);
  const setPendingConvert = useImagePlayground((s) => s.setPendingConvert);
  const convertImage = useImagePlayground((s) => s.convertImage);
  const selectSession = useImagePlayground((s) => s.selectSession);

  const cacheKey = `${image.format}-${image.size_bytes}`;
  const src = pgImageUrl(image.id, false, cacheKey);
  const isAvif = image.format.toLowerCase() === "avif";
  const convertOptions = image.is_animated ? ANIMATED_CONVERT_OPTIONS : STATIC_CONVERT_OPTIONS;

  const onFavorite = () => {
    void toggleFavorite(image.id).catch((error) =>
      toast.error(t("ip.action_failed", { error: errorText(error) })),
    );
  };

  const onMoveToFolder = (folderId: string | null, folderName: string) => {
    void moveFavoriteImageToFolder(image.id, folderId)
      .then(() => toast.success(t("ip.moved_to_folder", { name: folderName })))
      .catch((error) => toast.error(t("ip.action_failed", { error: errorText(error) })));
  };

  const onSelectConvert = (option: ConvertOption) => {
    if (option.lossy) {
      setPendingConvert({ image, format: option.format });
      return;
    }
    const toastId = toast.loading(t("ip.converting", { format: option.label }));
    void convertImage(image.id, option.format)
      .then((resp) => {
        toast.success(
          t("ip.convert_done", {
            format: resp.image.format.toUpperCase(),
            from: formatFileSize(resp.previous_size_bytes),
            to: formatFileSize(resp.image.size_bytes),
          }),
          { id: toastId },
        );
      })
      .catch((error) => {
        toast.error(t("ip.action_failed", { error: errorText(error) }), { id: toastId });
      });
  };

  const onReveal = () => {
    void revealPgImage(image.id).catch((error) =>
      toast.error(t("ip.reveal_failed", { error: errorText(error) })),
    );
  };

  const onDownload = () => {
    const a = document.createElement("a");
    a.href = pgImageUrl(image.id, true, cacheKey);
    a.download = image.filename;
    a.click();
  };

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        {/* biome-ignore lint/a11y/noStaticElementInteractions: 收藏卡片支持拖入文件夹，右键菜单同样提供移动选项 */}
        <div
          draggable={draggableFav}
          onDragStart={
            draggableFav
              ? (event) => {
                  event.dataTransfer.setData(
                    FAV_DRAG_MIME,
                    JSON.stringify({ kind: "image", id: image.id }),
                  );
                  event.dataTransfer.effectAllowed = "move";
                }
              : undefined
          }
          className={cn(
            "group/img relative overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle",
            aspectSquare && "aspect-square",
            draggableFav && "cursor-grab active:cursor-grabbing",
            className,
          )}
        >
          <button
            type="button"
            onClick={() => setLightbox(image.id, listIds)}
            className="block size-full cursor-zoom-in text-left focus:outline-none"
            aria-label={image.prompt || image.filename}
          >
            <img
              src={src}
              alt={image.prompt || image.filename}
              loading="lazy"
              decoding="async"
              draggable={false}
              className={cn(
                "block w-full",
                aspectSquare
                  ? "size-full object-cover"
                  : "max-h-[480px] object-contain bg-muted/30",
              )}
            />
          </button>

          {/* 动图徽标：极淡磨砂底，不影响观图 */}
          {image.is_animated && (
            <span className="pointer-events-none absolute bottom-2 left-2 rounded-md bg-black/55 px-1.5 py-0.5 font-mono text-2xs font-medium text-white backdrop-blur-xs">
              {image.format.toUpperCase()}
            </span>
          )}

          {/* 右上角透明星标收藏按钮：未收藏时悬停/聚焦出现，已收藏常驻 */}
          <button
            type="button"
            onClick={(event) => {
              event.stopPropagation();
              onFavorite();
            }}
            aria-label={image.favorite ? t("ip.unfavorite") : t("ip.favorite")}
            aria-pressed={image.favorite}
            title={image.favorite ? t("ip.unfavorite") : t("ip.favorite")}
            className={cn(
              "absolute top-2 right-2 flex size-8 items-center justify-center rounded-full backdrop-blur-sm transition-opacity",
              image.favorite
                ? "bg-black/45 text-amber-400 opacity-100 hover:bg-black/60"
                : "bg-black/35 text-white/90 opacity-0 hover:bg-black/55 group-hover/img:opacity-100 focus-visible:opacity-100 pointer-coarse:opacity-100",
            )}
          >
            <StarIcon
              className={cn("size-4", image.favorite && "fill-current")}
              strokeWidth={1.75}
            />
          </button>
        </div>
      </ContextMenuTrigger>

      <ContextMenuContent className="w-56 rounded-xl border-0 bg-popover p-1 shadow-[var(--elevation-popover)]">
        {/* 图片简略信息：格式 · 分辨率 · 体积 */}
        <div className="flex items-center justify-between gap-2 px-2 py-1 font-mono text-2xs tabular-nums text-text-tertiary">
          <span className="truncate">
            {image.format.toUpperCase()}
            {image.width > 0 && image.height > 0 ? ` · ${image.width}×${image.height}` : ""}
          </span>
          <span className="shrink-0 font-medium text-muted-foreground">
            {formatFileSize(image.size_bytes)}
          </span>
        </div>
        <ContextMenuSeparator className="bg-border-subtle" />

        <ContextMenuItem
          className="rounded-lg text-ui"
          onSelect={() => setLightbox(image.id, listIds)}
        >
          <EyeIcon className="size-4" />
          {t("ip.ctx_preview")}
        </ContextMenuItem>
        {!isAvif && (
          <ContextMenuItem
            className="rounded-lg text-ui"
            onSelect={() => void editWithImage(image.id)}
          >
            <PencilIcon className="size-4" />
            {t("ip.ctx_edit_with")}
          </ContextMenuItem>
        )}
        <ContextMenuItem className="rounded-lg text-ui" onSelect={onFavorite}>
          <StarIcon className={cn("size-4", image.favorite && "fill-current text-amber-500")} />
          {image.favorite ? t("ip.unfavorite") : t("ip.favorite")}
        </ContextMenuItem>
        {favoriteFolders.length > 0 && (
          <ContextMenuSub>
            <ContextMenuSubTrigger className="rounded-lg text-ui">
              <FolderInputIcon className="size-4" />
              {t("ip.ctx_move_folder")}
            </ContextMenuSubTrigger>
            <ContextMenuSubContent className="w-44 rounded-xl border-0 bg-popover p-1 shadow-[var(--elevation-popover)]">
              <ContextMenuItem
                className="rounded-lg text-ui"
                onSelect={() => onMoveToFolder(null, t("ip.folder_root"))}
              >
                <span className="flex-1 truncate">{t("ip.folder_root")}</span>
                {image.favorite && image.favorite_folder_id === null && (
                  <CheckIcon className="size-3.5 text-brand" />
                )}
              </ContextMenuItem>
              {favoriteFolders.map((folder) => (
                <ContextMenuItem
                  key={folder.id}
                  className="rounded-lg text-ui"
                  onSelect={() => onMoveToFolder(folder.id, folder.name)}
                >
                  <span className="flex-1 truncate">{folder.name}</span>
                  {image.favorite && image.favorite_folder_id === folder.id && (
                    <CheckIcon className="size-3.5 text-brand" />
                  )}
                </ContextMenuItem>
              ))}
            </ContextMenuSubContent>
          </ContextMenuSub>
        )}

        {!isAvif && (
          <ContextMenuSub>
            <ContextMenuSubTrigger className="rounded-lg text-ui">
              <ArrowRightLeftIcon className="size-4" />
              {t("ip.ctx_convert")}
            </ContextMenuSubTrigger>
            <ContextMenuSubContent className="w-48 rounded-xl border-0 bg-popover p-1 shadow-[var(--elevation-popover)]">
              {convertOptions.map((option) => (
                <ContextMenuItem
                  key={option.format}
                  className="rounded-lg text-ui"
                  onSelect={() => onSelectConvert(option)}
                >
                  <span className="flex-1 truncate">{option.label}</span>
                  <span className="text-2xs text-text-tertiary">
                    {option.lossy ? t("ip.convert_lossy_tag") : t("ip.convert_lossless_tag")}
                  </span>
                </ContextMenuItem>
              ))}
            </ContextMenuSubContent>
          </ContextMenuSub>
        )}

        <ContextMenuSeparator className="bg-border-subtle" />
        <ContextMenuItem className="rounded-lg text-ui" onSelect={onDownload}>
          <DownloadIcon className="size-4" />
          {t("ip.ctx_download")}
        </ContextMenuItem>
        {image.prompt && (
          <ContextMenuItem
            className="rounded-lg text-ui"
            onSelect={() => {
              void navigator.clipboard.writeText(image.prompt ?? "");
              toast.success(t("common.copied"));
            }}
          >
            <CopyIcon className="size-4" />
            {t("ip.ctx_copy_prompt")}
          </ContextMenuItem>
        )}
        {image.session_id && (
          <ContextMenuItem
            className="rounded-lg text-ui"
            onSelect={() => {
              if (image.session_id) void selectSession(image.session_id);
            }}
          >
            <ExternalLinkIcon className="size-4" />
            {t("ip.ctx_open_session")}
          </ContextMenuItem>
        )}
        <ContextMenuItem className="rounded-lg text-ui" onSelect={onReveal}>
          <FolderOpenIcon className="size-4" />
          {t("ip.ctx_reveal")}
        </ContextMenuItem>
        <ContextMenuSeparator className="bg-border-subtle" />
        <ContextMenuItem
          variant="destructive"
          className="rounded-lg text-ui"
          onSelect={() => setConfirmDeleteImage(image)}
        >
          <Trash2Icon className="size-4" />
          {t("common.delete")}
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}
