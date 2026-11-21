// 图片大图灯箱预览：支持左右切换、查看元信息与快捷操作。

import {
  ChevronLeftIcon,
  ChevronRightIcon,
  CopyIcon,
  DownloadIcon,
  FolderOpenIcon,
  PencilIcon,
  StarIcon,
  Trash2Icon,
} from "lucide-react";
import { useEffect } from "react";
import { toast } from "sonner";
import { IconButton } from "@/components/common/icon-button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { relativeTime } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { pgImageUrl, revealPgImage } from "@/lib/image-playground-api";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";
import { formatFileSize } from "./image-card";

export function ImageLightbox() {
  const t = useT();
  const imageId = useImagePlayground((s) => s.lightboxImageId);
  const list = useImagePlayground((s) => s.lightboxList);
  const imagesById = useImagePlayground((s) => s.imagesById);
  const setLightbox = useImagePlayground((s) => s.setLightbox);
  const toggleFavorite = useImagePlayground((s) => s.toggleFavorite);
  const editWithImage = useImagePlayground((s) => s.editWithImage);
  const setConfirmDeleteImage = useImagePlayground((s) => s.setConfirmDeleteImage);

  const image = imageId ? imagesById[imageId] : undefined;
  const index = imageId ? list.indexOf(imageId) : -1;
  const hasPrev = index > 0;
  const hasNext = index >= 0 && index < list.length - 1;

  useEffect(() => {
    if (!imageId) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "ArrowLeft" && hasPrev) {
        setLightbox(list[index - 1], list);
      } else if (event.key === "ArrowRight" && hasNext) {
        setLightbox(list[index + 1], list);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [imageId, index, list, hasPrev, hasNext, setLightbox]);

  if (!image) return null;

  const compressedDiff =
    image.original_size_bytes > 0 && image.original_size_bytes !== image.size_bytes;

  return (
    <Dialog open onOpenChange={(open) => !open && setLightbox(null)}>
      <DialogContent className="flex h-[min(88vh,860px)] w-[min(94vw,1180px)] max-w-none flex-col gap-0 overflow-hidden rounded-2xl border-0 bg-popover p-0 shadow-[var(--elevation-dialog)] sm:max-w-none md:flex-row">
        <DialogTitle className="sr-only">{image.filename}</DialogTitle>
        <DialogDescription className="sr-only">{image.prompt || image.filename}</DialogDescription>

        {/* 左侧主图区 */}
        <div className="relative flex min-h-0 flex-1 items-center justify-center bg-muted/40 p-4">
          {hasPrev && (
            <button
              type="button"
              onClick={() => setLightbox(list[index - 1], list)}
              aria-label={t("ip.prev_image")}
              className="absolute left-3 z-10 flex size-9 items-center justify-center rounded-full bg-popover/90 text-foreground shadow-[var(--elevation-popover)] hover:bg-popover"
            >
              <ChevronLeftIcon className="size-4" />
            </button>
          )}
          <img
            src={pgImageUrl(image.id, false, `${image.format}-${image.size_bytes}`)}
            alt={image.prompt || image.filename}
            className="max-h-full max-w-full rounded-lg object-contain"
          />
          {hasNext && (
            <button
              type="button"
              onClick={() => setLightbox(list[index + 1], list)}
              aria-label={t("ip.next_image")}
              className="absolute right-3 z-10 flex size-9 items-center justify-center rounded-full bg-popover/90 text-foreground shadow-[var(--elevation-popover)] hover:bg-popover"
            >
              <ChevronRightIcon className="size-4" />
            </button>
          )}
        </div>

        {/* 右侧信息与操作区 */}
        <aside className="flex w-full shrink-0 flex-col border-t border-border-subtle bg-card md:w-80 md:border-t-0 md:border-l">
          <div className="flex h-12 shrink-0 items-center gap-1 border-b border-border-subtle px-3 pr-12">
            <IconButton
              label={image.favorite ? t("ip.unfavorite") : t("ip.favorite")}
              onClick={() => void toggleFavorite(image.id).catch((err) => toast.error(String(err)))}
            >
              <StarIcon className={cn(image.favorite && "fill-current text-amber-500")} />
            </IconButton>
            {image.format.toLowerCase() !== "avif" && (
              <IconButton
                label={t("ip.ctx_edit_with")}
                onClick={() => void editWithImage(image.id)}
              >
                <PencilIcon />
              </IconButton>
            )}
            <IconButton
              label={t("ip.ctx_download")}
              onClick={() => {
                const a = document.createElement("a");
                a.href = pgImageUrl(image.id, true, `${image.format}-${image.size_bytes}`);
                a.download = image.filename;
                a.click();
              }}
            >
              <DownloadIcon />
            </IconButton>
            <IconButton
              label={t("ip.ctx_reveal")}
              onClick={() =>
                void revealPgImage(image.id).catch((err) =>
                  toast.error(t("ip.reveal_failed", { error: String(err) })),
                )
              }
            >
              <FolderOpenIcon />
            </IconButton>
            <IconButton
              label={t("common.delete")}
              onClick={() => setConfirmDeleteImage(image)}
              className="ml-auto text-destructive hover:text-destructive"
            >
              <Trash2Icon />
            </IconButton>
          </div>

          <div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-4 text-ui">
            {image.prompt && (
              <section className="space-y-1">
                <div className="flex items-center justify-between">
                  <span className="text-xs font-medium text-text-tertiary">{t("ip.prompt")}</span>
                  <button
                    type="button"
                    onClick={() => {
                      void navigator.clipboard.writeText(image.prompt ?? "");
                      toast.success(t("common.copied"));
                    }}
                    className="inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
                  >
                    <CopyIcon className="size-3" />
                    {t("common.copy")}
                  </button>
                </div>
                <p className="whitespace-pre-wrap break-words text-sm leading-6 text-foreground">
                  {image.prompt}
                </p>
              </section>
            )}

            {image.revised_prompt && image.revised_prompt !== image.prompt && (
              <section className="space-y-1">
                <span className="text-xs font-medium text-text-tertiary">revised_prompt</span>
                <p className="whitespace-pre-wrap break-words text-xs leading-5 text-muted-foreground">
                  {image.revised_prompt}
                </p>
              </section>
            )}

            <dl className="space-y-2 border-t border-border-subtle pt-3 text-xs">
              {image.model && (
                <div className="flex justify-between gap-2">
                  <dt className="text-text-tertiary">{t("ip.model")}</dt>
                  <dd className="truncate font-mono text-foreground" title={image.model}>
                    {image.model}
                  </dd>
                </div>
              )}
              <div className="flex justify-between gap-2">
                <dt className="text-text-tertiary">{t("ip.dimensions")}</dt>
                <dd className="font-mono text-foreground tabular-nums">
                  {image.width} × {image.height}
                </dd>
              </div>
              <div className="flex justify-between gap-2">
                <dt className="text-text-tertiary">{t("ip.format")}</dt>
                <dd className="font-mono text-foreground uppercase">
                  {image.format}
                  {image.is_animated ? " (animated)" : ""}
                </dd>
              </div>
              <div className="flex justify-between gap-2">
                <dt className="text-text-tertiary">{t("ip.file_size")}</dt>
                <dd className="font-mono text-foreground tabular-nums">
                  {formatFileSize(image.size_bytes)}
                  {compressedDiff && (
                    <span className="ml-1 text-text-tertiary">
                      ({t("ip.original_size", { size: formatFileSize(image.original_size_bytes) })})
                    </span>
                  )}
                </dd>
              </div>
              <div className="flex justify-between gap-2">
                <dt className="text-text-tertiary">{t("ip.created_at")}</dt>
                <dd className="text-muted-foreground">{relativeTime(image.created_at_ms)}</dd>
              </div>
              <div className="space-y-1 pt-1">
                <dt className="text-text-tertiary">{t("ip.local_path")}</dt>
                <dd className="break-all font-mono text-2xs text-text-tertiary">
                  {image.file_path}
                </dd>
              </div>
            </dl>
          </div>
        </aside>
      </DialogContent>
    </Dialog>
  );
}
