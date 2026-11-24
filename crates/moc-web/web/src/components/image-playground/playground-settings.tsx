// SettingsDialog 中的「Image Playground」独立设置分区：
// 本地图片存放目录、自动压缩格式与质量（含 WebP / AVIF / JPEG / PNG / APNG）、动图 GIF 策略、
// Edit 上传参考图保护性压缩与默认编辑编码方式。

import { FolderOpenIcon, Loader2Icon } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { SettingRow } from "@/components/settings/field";
import { Card } from "@/components/settings/sections";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useT } from "@/lib/i18n";
import {
  type AnimatedOutputFormat,
  type EditEncoding,
  type OutputCompressionFormat,
  openStorageDir,
  type PlaygroundConfig,
} from "@/lib/image-playground-api";
import { useImagePlayground } from "@/store/image-playground";

const STATIC_FORMATS: { value: OutputCompressionFormat; label: string }[] = [
  { value: "original", label: "Original" },
  { value: "webp", label: "WebP (lossy)" },
  { value: "webp_lossless", label: "WebP (lossless)" },
  { value: "avif", label: "AVIF" },
  { value: "jpeg", label: "JPEG" },
  { value: "png", label: "PNG" },
  { value: "apng", label: "APNG" },
];

const ANIMATED_FORMATS: { value: AnimatedOutputFormat; label: string }[] = [
  { value: "original", label: "Original (GIF)" },
  { value: "webp_animated", label: "WebP (animated)" },
  { value: "apng", label: "APNG" },
];

const EDIT_ENCODINGS: { value: EditEncoding; label: string }[] = [
  { value: "multipart", label: "multipart (image / image[])" },
  { value: "multipart_repeat_image", label: "multipart (repeat image)" },
  { value: "json", label: "application/json (images[])" },
];

export function ImagePlaygroundSettingsSection() {
  const t = useT();
  const config = useImagePlayground((s) => s.config);
  const resolvedDir = useImagePlayground((s) => s.resolvedStorageDir);
  const refreshConfig = useImagePlayground((s) => s.refreshConfig);
  const updateConfig = useImagePlayground((s) => s.updateConfig);

  const [dirDraft, setDirDraft] = useState("");
  const [maxDimDraft, setMaxDimDraft] = useState("2048");
  const [maxMbDraft, setMaxMbDraft] = useState("4");
  const [qualityDraft, setQualityDraft] = useState(85);
  const [animQualityDraft, setAnimQualityDraft] = useState(80);
  const committedQualityRef = useRef(85);
  const committedAnimQualityRef = useRef(80);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void refreshConfig().catch(() => undefined);
  }, [refreshConfig]);

  useEffect(() => {
    if (!config) return;
    setDirDraft(config.storage_dir);
    setMaxDimDraft(String(config.input_compress.max_dimension));
    setMaxMbDraft(String(config.input_compress.max_bytes_mb));
    setQualityDraft(config.compression.quality);
    setAnimQualityDraft(config.compression.animated_quality);
    committedQualityRef.current = config.compression.quality;
    committedAnimQualityRef.current = config.compression.animated_quality;
  }, [config]);

  if (!config) {
    return (
      <div className="flex justify-center py-16 text-text-tertiary">
        <Loader2Icon className="size-5 animate-spin" />
      </div>
    );
  }

  const commit = async (next: PlaygroundConfig) => {
    setSaving(true);
    try {
      await updateConfig(next);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
      setDirDraft(config.storage_dir);
      setMaxDimDraft(String(config.input_compress.max_dimension));
      setMaxMbDraft(String(config.input_compress.max_bytes_mb));
      setQualityDraft(config.compression.quality);
      setAnimQualityDraft(config.compression.animated_quality);
      committedQualityRef.current = config.compression.quality;
      committedAnimQualityRef.current = config.compression.animated_quality;
    } finally {
      setSaving(false);
    }
  };

  const commitQuality = (nextValue: number) => {
    if (nextValue === committedQualityRef.current) return;
    committedQualityRef.current = nextValue;
    void commit({
      ...config,
      compression: { ...config.compression, quality: nextValue },
    });
  };

  const commitAnimQuality = (nextValue: number) => {
    if (nextValue === committedAnimQualityRef.current) return;
    committedAnimQualityRef.current = nextValue;
    void commit({
      ...config,
      compression: { ...config.compression, animated_quality: nextValue },
    });
  };

  const hasStaticQuality =
    config.compression.format === "webp" ||
    config.compression.format === "avif" ||
    config.compression.format === "jpeg";

  return (
    <div className="space-y-6">
      {/* 本地存储 */}
      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">{t("ip.set_storage")}</h3>
        <Card>
          <SettingRow
            label={t("ip.set_storage_dir")}
            description={
              resolvedDir
                ? t("ip.set_storage_dir_resolved", { path: resolvedDir })
                : t("ip.set_storage_dir_desc")
            }
          >
            <div className="flex items-center gap-1.5">
              <Input
                value={dirDraft}
                onChange={(e) => setDirDraft(e.target.value)}
                onBlur={() => {
                  const trimmed = dirDraft.trim();
                  if (trimmed && trimmed !== config.storage_dir) {
                    void commit({ ...config, storage_dir: trimmed });
                  }
                }}
                onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
                disabled={saving}
                className="h-8 flex-1 rounded-lg font-mono text-xs shadow-none"
                spellCheck={false}
              />
              <Button
                size="sm"
                variant="outline"
                className="h-8 shrink-0 px-2.5 shadow-none"
                onClick={() =>
                  void openStorageDir().catch((err) =>
                    toast.error(t("ip.reveal_failed", { error: String(err) })),
                  )
                }
                title={t("ip.set_open_dir")}
              >
                <FolderOpenIcon className="size-3.5" />
              </Button>
            </div>
          </SettingRow>
        </Card>
      </section>

      {/* 接收生成图自动压缩 */}
      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">{t("ip.set_output_compress")}</h3>
        <Card>
          <SettingRow
            label={t("ip.set_static_format")}
            description={
              config.compression.format === "avif"
                ? `${t("ip.set_static_format_desc")} ${t("ip.convert_avif_note")}`
                : t("ip.set_static_format_desc")
            }
          >
            <Select
              value={config.compression.format}
              onValueChange={(value) =>
                void commit({
                  ...config,
                  compression: {
                    ...config.compression,
                    format: value as OutputCompressionFormat,
                  },
                })
              }
            >
              <SelectTrigger size="sm" className="h-8 w-full rounded-lg text-ui shadow-none">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {STATIC_FORMATS.map((opt) => (
                  <SelectItem key={opt.value} value={opt.value}>
                    {opt.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingRow>

          {hasStaticQuality && (
            <SettingRow label={t("ip.set_quality")} description={t("ip.set_quality_desc")}>
              <div className="flex items-center gap-3">
                <input
                  type="range"
                  min={1}
                  max={100}
                  value={qualityDraft}
                  onChange={(e) => setQualityDraft(Number(e.target.value))}
                  onPointerUp={() => commitQuality(qualityDraft)}
                  onKeyUp={() => commitQuality(qualityDraft)}
                  onBlur={() => commitQuality(qualityDraft)}
                  className="flex-1 accent-brand"
                />
                <span className="w-9 text-right font-mono text-xs tabular-nums text-foreground">
                  {qualityDraft}
                </span>
              </div>
            </SettingRow>
          )}

          <SettingRow label={t("ip.set_anim_format")} description={t("ip.set_anim_format_desc")}>
            <Select
              value={config.compression.animated_format}
              onValueChange={(value) =>
                void commit({
                  ...config,
                  compression: {
                    ...config.compression,
                    animated_format: value as AnimatedOutputFormat,
                  },
                })
              }
            >
              <SelectTrigger size="sm" className="h-8 w-full rounded-lg text-ui shadow-none">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {ANIMATED_FORMATS.map((opt) => (
                  <SelectItem key={opt.value} value={opt.value}>
                    {opt.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingRow>

          {config.compression.animated_format === "webp_animated" && (
            <SettingRow
              label={t("ip.set_anim_quality")}
              description={t("ip.set_anim_quality_desc")}
            >
              <div className="flex items-center gap-3">
                <input
                  type="range"
                  min={1}
                  max={100}
                  value={animQualityDraft}
                  onChange={(e) => setAnimQualityDraft(Number(e.target.value))}
                  onPointerUp={() => commitAnimQuality(animQualityDraft)}
                  onKeyUp={() => commitAnimQuality(animQualityDraft)}
                  onBlur={() => commitAnimQuality(animQualityDraft)}
                  className="flex-1 accent-brand"
                />
                <span className="w-9 text-right font-mono text-xs tabular-nums text-foreground">
                  {animQualityDraft}
                </span>
              </div>
            </SettingRow>
          )}
        </Card>
      </section>

      {/* Edit 上传参考图保护压缩与编码 */}
      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">{t("ip.set_edit_group")}</h3>
        <Card>
          <SettingRow
            label={t("ip.set_input_compress")}
            description={t("ip.set_input_compress_desc")}
          >
            <div className="flex justify-end">
              <Switch
                checked={config.input_compress.enabled}
                onCheckedChange={(enabled) =>
                  void commit({
                    ...config,
                    input_compress: { ...config.input_compress, enabled },
                  })
                }
              />
            </div>
          </SettingRow>

          {config.input_compress.enabled && (
            <>
              <SettingRow
                label={t("ip.set_input_max_dim")}
                description={t("ip.set_input_max_dim_desc")}
              >
                <Input
                  value={maxDimDraft}
                  onChange={(e) => setMaxDimDraft(e.target.value)}
                  onBlur={() => {
                    const n = Number.parseInt(maxDimDraft, 10);
                    if (Number.isFinite(n) && n >= 0 && n !== config.input_compress.max_dimension) {
                      void commit({
                        ...config,
                        input_compress: { ...config.input_compress, max_dimension: n },
                      });
                    }
                  }}
                  onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
                  className="h-8 rounded-lg font-mono text-xs tabular-nums shadow-none"
                />
              </SettingRow>
              <SettingRow
                label={t("ip.set_input_max_mb")}
                description={t("ip.set_input_max_mb_desc")}
              >
                <Input
                  value={maxMbDraft}
                  onChange={(e) => setMaxMbDraft(e.target.value)}
                  onBlur={() => {
                    const n = Number.parseFloat(maxMbDraft);
                    if (Number.isFinite(n) && n > 0 && n !== config.input_compress.max_bytes_mb) {
                      void commit({
                        ...config,
                        input_compress: { ...config.input_compress, max_bytes_mb: n },
                      });
                    }
                  }}
                  onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
                  className="h-8 rounded-lg font-mono text-xs tabular-nums shadow-none"
                />
              </SettingRow>
            </>
          )}

          <SettingRow
            label={t("ip.set_edit_encoding")}
            description={t("ip.set_edit_encoding_desc")}
          >
            <Select
              value={config.default_edit_encoding}
              onValueChange={(value) =>
                void commit({
                  ...config,
                  default_edit_encoding: value as EditEncoding,
                })
              }
            >
              <SelectTrigger size="sm" className="h-8 w-full rounded-lg text-ui shadow-none">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {EDIT_ENCODINGS.map((opt) => (
                  <SelectItem key={opt.value} value={opt.value}>
                    {opt.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </SettingRow>
        </Card>
      </section>
    </div>
  );
}
