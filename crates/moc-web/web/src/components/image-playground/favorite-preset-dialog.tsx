// 新建 / 编辑「提示词与多图组合收藏」弹窗：
// 支持纯提示词收藏，或将多张参考图 + 提示词打包为一个收藏单位，并可归入指定收藏文件夹。

import { CheckIcon, ImagePlusIcon, SparklesIcon, UploadIcon, XIcon } from "lucide-react";
import { nanoid } from "nanoid";
import { useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useT } from "@/lib/i18n";
import { pgImageUrl, pgUploadUrl, type UploadRefInput } from "@/lib/image-playground-api";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";

const ROOT_FOLDER = "__root__";

type PendingUploadRef = {
  id: string;
  ref: UploadRefInput;
};

type PendingFileRef = {
  id: string;
  file: File;
  previewUrl: string;
};

export function FavoritePresetDialog() {
  const t = useT();
  const editor = useImagePlayground((s) => s.presetEditor);
  const closePresetEditor = useImagePlayground((s) => s.closePresetEditor);
  const favoriteFolders = useImagePlayground((s) => s.favoriteFolders);
  const favoriteIds = useImagePlayground((s) => s.favoriteIds);
  const galleryIds = useImagePlayground((s) => s.galleryIds);
  const imagesById = useImagePlayground((s) => s.imagesById);
  const uploadFavorites = useImagePlayground((s) => s.uploadFavorites);
  const createFavoritePreset = useImagePlayground((s) => s.createFavoritePreset);
  const updateFavoritePreset = useImagePlayground((s) => s.updateFavoritePreset);

  const [title, setTitle] = useState("");
  const [prompt, setPrompt] = useState("");
  const [imageIds, setImageIds] = useState<string[]>([]);
  const [pendingUploads, setPendingUploads] = useState<PendingUploadRef[]>([]);
  const [pendingFiles, setPendingFiles] = useState<PendingFileRef[]>([]);
  const [folderId, setFolderId] = useState<string | null>(null);
  const [pickingImages, setPickingImages] = useState(false);
  const [uploading, setUploading] = useState(false);
  const [saving, setSaving] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!editor) {
      setPendingUploads([]);
      setPendingFiles([]);
      return;
    }
    if (editor.editing) {
      setTitle(editor.editing.title);
      setPrompt(editor.editing.prompt);
      setImageIds(editor.editing.image_ids);
      setFolderId(editor.editing.folder_id);
      setPendingUploads([]);
      setPendingFiles([]);
      setPickingImages(false);
      return;
    }
    setTitle(editor.initialTitle ?? "");
    setPrompt(editor.initialPrompt ?? "");
    setImageIds(editor.initialImageIds ?? []);
    setFolderId(editor.initialFolderId ?? null);
    setPendingUploads((editor.initialUploads ?? []).map((ref) => ({ id: nanoid(), ref })));
    const nextFiles = (editor.initialFiles ?? []).map((file) => ({
      id: nanoid(),
      file,
      previewUrl: URL.createObjectURL(file),
    }));
    setPendingFiles(nextFiles);
    setPickingImages(false);
    return () => {
      for (const item of nextFiles) URL.revokeObjectURL(item.previewUrl);
    };
  }, [editor]);

  const candidateImages = useMemo(() => {
    const seen = new Set<string>();
    const list = [];
    for (const id of [...favoriteIds, ...galleryIds]) {
      if (seen.has(id)) continue;
      const img = imagesById[id];
      if (img && img.format.toLowerCase() !== "avif") {
        seen.add(id);
        list.push(img);
      }
    }
    return list;
  }, [favoriteIds, galleryIds, imagesById]);

  const toggleImage = (id: string) => {
    setImageIds((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  };

  const removePendingUpload = (id: string) => {
    setPendingUploads((prev) => prev.filter((item) => item.id !== id));
  };

  const removePendingFile = (id: string) => {
    setPendingFiles((prev) => {
      const target = prev.find((item) => item.id === id);
      if (target) URL.revokeObjectURL(target.previewUrl);
      return prev.filter((item) => item.id !== id);
    });
  };

  const handleUpload = async (files: File[]) => {
    const imgs = files.filter((f) => f.type.startsWith("image/"));
    if (imgs.length === 0) return;
    setUploading(true);
    try {
      const added = await uploadFavorites(imgs, folderId);
      setImageIds((prev) => {
        const next = [...prev];
        for (const img of added) {
          if (!next.includes(img.id)) next.push(img.id);
        }
        return next;
      });
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    } finally {
      setUploading(false);
    }
  };

  const totalRefCount = imageIds.length + pendingUploads.length + pendingFiles.length;

  const handleSave = async () => {
    const trimmedPrompt = prompt.trim();
    if (!trimmedPrompt && totalRefCount === 0) {
      toast.error(t("ip.preset_empty_err"));
      return;
    }
    setSaving(true);
    try {
      const nextImageIds = [...imageIds];
      if (pendingFiles.length > 0) {
        const uploaded = await uploadFavorites(
          pendingFiles.map((item) => item.file),
          folderId,
        );
        for (const img of uploaded) {
          if (!nextImageIds.includes(img.id)) nextImageIds.push(img.id);
        }
        // 上传成功后立刻同步进 imageIds 并清空待传文件，避免后续保存报错重试时重复上传。
        setImageIds(nextImageIds);
        for (const item of pendingFiles) URL.revokeObjectURL(item.previewUrl);
        setPendingFiles([]);
      }
      if (editor?.editing) {
        await updateFavoritePreset(editor.editing.id, {
          title: title.trim() || undefined,
          prompt: trimmedPrompt,
          image_ids: nextImageIds,
          folder_id: folderId,
        });
        toast.success(t("ip.preset_updated"));
      } else {
        await createFavoritePreset({
          title: title.trim() || undefined,
          prompt: trimmedPrompt,
          image_ids: nextImageIds,
          source_uploads:
            pendingUploads.length > 0 ? pendingUploads.map((item) => item.ref) : undefined,
          folder_id: folderId,
        });
        toast.success(t("ip.preset_saved"));
      }
      closePresetEditor();
    } catch (error) {
      toast.error(t("ip.action_failed", { error: String(error) }));
    } finally {
      setSaving(false);
    }
  };

  const isEditing = Boolean(editor?.editing);

  return (
    <Dialog open={Boolean(editor)} onOpenChange={(open) => !open && closePresetEditor()}>
      <DialogContent className="max-w-lg rounded-2xl border-0 bg-popover shadow-[var(--elevation-dialog)]">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2 text-base">
            <SparklesIcon className="size-4 text-brand" />
            {isEditing ? t("ip.preset_edit_title") : t("ip.preset_new_title")}
          </DialogTitle>
          <DialogDescription className="text-xs text-muted-foreground">
            {t("ip.preset_desc")}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-1">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <div className="space-y-1">
              <span className="text-xs font-medium text-muted-foreground">
                {t("ip.preset_title_label")}
              </span>
              <Input
                value={title}
                onChange={(e) => setTitle(e.target.value)}
                placeholder={t("ip.preset_title_ph")}
                className="h-8 text-xs shadow-none"
              />
            </div>

            <div className="space-y-1">
              <span className="text-xs font-medium text-muted-foreground">
                {t("ip.folder_label")}
              </span>
              <Select
                value={folderId ?? ROOT_FOLDER}
                onValueChange={(v) => setFolderId(v === ROOT_FOLDER ? null : v)}
              >
                <SelectTrigger className="h-8 text-xs shadow-none">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={ROOT_FOLDER}>{t("ip.folder_root")}</SelectItem>
                  {favoriteFolders.map((f) => (
                    <SelectItem key={f.id} value={f.id}>
                      {f.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          <div className="space-y-1">
            <span className="text-xs font-medium text-muted-foreground">{t("ip.prompt")}</span>
            <textarea
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              rows={4}
              placeholder={t("ip.preset_prompt_ph")}
              className="w-full resize-y rounded-lg border border-input bg-transparent px-3 py-2 text-ui outline-none transition-colors placeholder:text-text-tertiary focus:border-ring"
            />
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs font-medium text-muted-foreground">
                {t("ip.preset_refs_label", { n: totalRefCount })}
              </span>
              <div className="flex items-center gap-1.5">
                <Button
                  type="button"
                  size="xs"
                  variant={pickingImages ? "secondary" : "outline"}
                  className="h-7 shadow-none"
                  onClick={() => setPickingImages((v) => !v)}
                >
                  <ImagePlusIcon className="size-3.5" />
                  {t("ip.preset_pick_images")}
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
                    void handleUpload(files);
                  }}
                />
                <Button
                  type="button"
                  size="xs"
                  variant="outline"
                  disabled={uploading}
                  className="h-7 shadow-none"
                  onClick={() => fileInputRef.current?.click()}
                >
                  <UploadIcon className="size-3.5" />
                  {t("ip.upload_favorite")}
                </Button>
              </div>
            </div>

            {totalRefCount > 0 && (
              <div className="flex flex-wrap gap-2">
                {imageIds.map((id) => {
                  const img = imagesById[id];
                  if (!img) return null;
                  return (
                    <div
                      key={id}
                      className="group relative size-14 overflow-hidden rounded-lg bg-muted/40 ring-1 ring-inset ring-border-subtle"
                    >
                      <img
                        src={pgImageUrl(id)}
                        alt={img.filename}
                        className="size-full object-cover"
                      />
                      <button
                        type="button"
                        onClick={() => toggleImage(id)}
                        aria-label={t("common.delete")}
                        className="absolute top-0.5 right-0.5 flex size-4 items-center justify-center rounded-full bg-black/65 text-white opacity-0 transition-opacity group-hover:opacity-100"
                      >
                        <XIcon className="size-2.5" />
                      </button>
                    </div>
                  );
                })}
                {pendingUploads.map((up) => (
                  <div
                    key={up.id}
                    className="group relative size-14 overflow-hidden rounded-lg bg-muted/40 ring-1 ring-inset ring-border-subtle"
                  >
                    <img
                      src={pgUploadUrl(up.ref.upload_id)}
                      alt={up.ref.filename ?? up.ref.upload_id}
                      className="size-full object-cover"
                    />
                    <button
                      type="button"
                      onClick={() => removePendingUpload(up.id)}
                      aria-label={t("common.delete")}
                      className="absolute top-0.5 right-0.5 flex size-4 items-center justify-center rounded-full bg-black/65 text-white opacity-0 transition-opacity group-hover:opacity-100"
                    >
                      <XIcon className="size-2.5" />
                    </button>
                  </div>
                ))}
                {pendingFiles.map((pf) => (
                  <div
                    key={pf.id}
                    className="group relative size-14 overflow-hidden rounded-lg bg-muted/40 ring-1 ring-inset ring-border-subtle"
                  >
                    <img
                      src={pf.previewUrl}
                      alt={pf.file.name || "image"}
                      className="size-full object-cover"
                    />
                    <button
                      type="button"
                      onClick={() => removePendingFile(pf.id)}
                      aria-label={t("common.delete")}
                      className="absolute top-0.5 right-0.5 flex size-4 items-center justify-center rounded-full bg-black/65 text-white opacity-0 transition-opacity group-hover:opacity-100"
                    >
                      <XIcon className="size-2.5" />
                    </button>
                  </div>
                ))}
              </div>
            )}

            {pickingImages && (
              <div className="rounded-xl bg-muted/40 p-2">
                {candidateImages.length === 0 ? (
                  <p className="py-4 text-center text-xs text-text-tertiary">
                    {t("ip.fav_empty_hint")}
                  </p>
                ) : (
                  <div className="grid max-h-44 grid-cols-5 gap-1.5 overflow-y-auto p-0.5">
                    {candidateImages.map((img) => {
                      const active = imageIds.includes(img.id);
                      return (
                        <button
                          key={img.id}
                          type="button"
                          onClick={() => toggleImage(img.id)}
                          title={img.prompt || img.filename}
                          className={cn(
                            "relative aspect-square overflow-hidden rounded-lg bg-muted ring-1 ring-inset",
                            active ? "ring-2 ring-brand" : "ring-border-subtle hover:opacity-90",
                          )}
                        >
                          <img
                            src={pgImageUrl(img.id)}
                            alt={img.filename}
                            loading="lazy"
                            className="size-full object-cover"
                          />
                          {active && (
                            <span className="absolute top-1 right-1 flex size-4 items-center justify-center rounded-full bg-brand text-brand-foreground">
                              <CheckIcon className="size-2.5" />
                            </span>
                          )}
                        </button>
                      );
                    })}
                  </div>
                )}
              </div>
            )}
          </div>
        </div>

        <DialogFooter>
          <Button type="button" variant="outline" size="sm" onClick={closePresetEditor}>
            {t("common.cancel")}
          </Button>
          <Button type="button" size="sm" disabled={saving} onClick={() => void handleSave()}>
            {t("common.save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
