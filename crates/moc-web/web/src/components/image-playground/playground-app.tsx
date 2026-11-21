// Image Playground 顶层容器：懒加载入口，与 Agent 主界面解耦，复用 AppFrame、SettingsDialog 与删除确认弹窗。

import { useEffect, useState } from "react";
import { toast } from "sonner";
import { AppFrame } from "@/components/layout/AppFrame";
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
import { useT } from "@/lib/i18n";
import { useImagePlayground } from "@/store/image-playground";
import { useUi } from "@/store/ui";
import { FavoritePresetDialog } from "./favorite-preset-dialog";
import { convertOptionLabel, formatFileSize } from "./image-card";
import { ImageLightbox } from "./image-lightbox";
import { PlaygroundChatView } from "./playground-chat";
import { PlaygroundFavoritesView } from "./playground-favorites";
import { PlaygroundGalleryView } from "./playground-gallery";
import { PlaygroundSidebar } from "./playground-sidebar";

export default function ImagePlaygroundApp({ onBackToAgent }: { onBackToAgent: () => void }) {
  const t = useT();
  const view = useImagePlayground((s) => s.view);
  const bootstrap = useImagePlayground((s) => s.bootstrap);
  const refreshModels = useImagePlayground((s) => s.refreshModels);
  const settingsOpen = useUi((s) => s.settings.open);

  const confirmDeleteImage = useImagePlayground((s) => s.confirmDeleteImage);
  const setConfirmDeleteImage = useImagePlayground((s) => s.setConfirmDeleteImage);
  const removeImage = useImagePlayground((s) => s.removeImage);

  const confirmDeleteSession = useImagePlayground((s) => s.confirmDeleteSession);
  const setConfirmDeleteSession = useImagePlayground((s) => s.setConfirmDeleteSession);
  const removeSession = useImagePlayground((s) => s.removeSession);

  const pendingConvert = useImagePlayground((s) => s.pendingConvert);
  const setPendingConvert = useImagePlayground((s) => s.setPendingConvert);
  const convertImage = useImagePlayground((s) => s.convertImage);
  const config = useImagePlayground((s) => s.config);
  const [convertQuality, setConvertQuality] = useState(85);
  const [converting, setConverting] = useState(false);

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

  useEffect(() => {
    if (!pendingConvert) return;
    const defaultQ =
      pendingConvert.format === "webp_animated"
        ? (config?.compression.animated_quality ?? 80)
        : (config?.compression.quality ?? 85);
    setConvertQuality(defaultQ);
  }, [pendingConvert, config]);

  // 关闭设置弹窗后刷新图片模型列表（用户在供应商页勾选/取消了 image_gen）。
  useEffect(() => {
    if (!settingsOpen) {
      void refreshModels().catch(() => undefined);
    }
  }, [settingsOpen, refreshModels]);

  return (
    <>
      <AppFrame
        sidebar={<PlaygroundSidebar onBackToAgent={onBackToAgent} />}
        main={
          view === "gallery" ? (
            <PlaygroundGalleryView />
          ) : view === "favorites" ? (
            <PlaygroundFavoritesView />
          ) : (
            <PlaygroundChatView />
          )
        }
        rightbar={null}
      />

      <ImageLightbox />
      <FavoritePresetDialog />

      {/* 有损格式转换二次确认弹窗 */}
      <AlertDialog
        open={pendingConvert !== null}
        onOpenChange={(open) => {
          if (!open && !converting) setPendingConvert(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {pendingConvert
                ? t("ip.convert_lossy_title", {
                    format: convertOptionLabel(pendingConvert.format),
                  })
                : ""}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {pendingConvert
                ? t("ip.convert_lossy_desc", {
                    from: pendingConvert.image.format.toUpperCase(),
                    size: formatFileSize(pendingConvert.image.size_bytes),
                    to: convertOptionLabel(pendingConvert.format),
                  })
                : ""}
              {pendingConvert?.format === "jpeg" && ` ${t("ip.convert_jpeg_alpha_note")}`}
            </AlertDialogDescription>
          </AlertDialogHeader>

          <div className="space-y-1.5 py-1">
            <div className="flex items-center justify-between text-xs">
              <span className="text-muted-foreground">{t("ip.set_quality")}</span>
              <span className="font-mono tabular-nums text-foreground">{convertQuality}</span>
            </div>
            <input
              type="range"
              min={1}
              max={100}
              value={convertQuality}
              disabled={converting}
              onChange={(e) => setConvertQuality(Number(e.target.value))}
              className="w-full accent-brand"
            />
          </div>

          <AlertDialogFooter>
            <AlertDialogCancel disabled={converting}>{t("common.cancel")}</AlertDialogCancel>
            <AlertDialogAction
              disabled={converting}
              onClick={(event) => {
                event.preventDefault();
                if (!pendingConvert || converting) return;
                const { image, format } = pendingConvert;
                const label = convertOptionLabel(format);
                setConverting(true);
                const toastId = toast.loading(t("ip.converting", { format: label }));
                void convertImage(image.id, format, convertQuality)
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
                  .catch((err) => {
                    toast.error(t("ip.action_failed", { error: String(err) }), { id: toastId });
                  })
                  .finally(() => {
                    setConverting(false);
                  });
              }}
            >
              {t("ip.convert_confirm")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      {/* 删除单张图片确认 */}
      <AlertDialog
        open={confirmDeleteImage !== null}
        onOpenChange={(open) => !open && setConfirmDeleteImage(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("ip.delete_image_title")}</AlertDialogTitle>
            <AlertDialogDescription>{t("ip.delete_image_desc")}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("common.cancel")}</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              onClick={() => {
                if (!confirmDeleteImage) return;
                void removeImage(confirmDeleteImage.id)
                  .then(() => toast.success(t("ip.image_deleted")))
                  .catch((err) => toast.error(t("ip.action_failed", { error: String(err) })));
              }}
            >
              {t("common.delete")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      {/* 删除会话确认（保留已收藏图片） */}
      <AlertDialog
        open={confirmDeleteSession !== null}
        onOpenChange={(open) => !open && setConfirmDeleteSession(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("ip.delete_session_title")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("ip.delete_session_desc", { title: confirmDeleteSession?.title ?? "" })}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("common.cancel")}</AlertDialogCancel>
            <AlertDialogAction
              variant="destructive"
              onClick={() => {
                if (!confirmDeleteSession) return;
                void removeSession(confirmDeleteSession.id)
                  .then(() => toast.success(t("ip.session_deleted")))
                  .catch((err) => toast.error(t("ip.action_failed", { error: String(err) })));
              }}
            >
              {t("common.delete")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
