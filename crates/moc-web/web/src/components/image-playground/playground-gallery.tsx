// 画廊视图：平铺大图按时间倒序展示全部会话中生成的图片。

import { PanelLeftOpenIcon, SearchIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { useT } from "@/lib/i18n";
import { useNarrow } from "@/lib/use-narrow";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";
import { useLayout } from "@/store/layout";
import { ImageCard } from "./image-card";

type FilterMode = "all" | "animated";

export function PlaygroundGalleryView() {
  const t = useT();
  const galleryIds = useImagePlayground((s) => s.galleryIds);
  const imagesById = useImagePlayground((s) => s.imagesById);
  const collapsed = useLayout((s) => s.sidebarCollapsed);
  const overlay = useLayout((s) => s.sidebarOverlay);
  const narrow = useNarrow();

  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<FilterMode>("all");

  const items = useMemo(() => {
    const q = query.trim().toLowerCase();
    return galleryIds
      .map((id) => imagesById[id])
      .filter((img): img is NonNullable<typeof img> => Boolean(img))
      .filter((img) => {
        if (filter === "animated" && !img.is_animated) return false;
        if (!q) return true;
        return `${img.prompt ?? ""} ${img.model ?? ""} ${img.filename}`.toLowerCase().includes(q);
      });
  }, [galleryIds, imagesById, query, filter]);

  const listIds = useMemo(() => items.map((i) => i.id), [items]);
  const showReveal = narrow ? !overlay : collapsed;

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border-subtle px-3">
        {showReveal && (
          <IconButton label={t("hd.sidebar")} onClick={() => useLayout.getState().toggleSidebar()}>
            <PanelLeftOpenIcon />
          </IconButton>
        )}
        <h1 className="text-sm font-medium text-foreground">{t("ip.nav_gallery")}</h1>
        <span className="font-mono text-xs tabular-nums text-text-tertiary">{items.length}</span>

        <div className="ml-auto flex items-center gap-2">
          <div className="flex rounded-lg bg-muted p-0.5">
            {(["all", "animated"] as const).map((mode) => (
              <button
                key={mode}
                type="button"
                onClick={() => setFilter(mode)}
                className={cn(
                  "h-6 rounded-md px-2 text-xs transition-colors",
                  filter === mode
                    ? "bg-background text-foreground shadow-xs"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                {mode === "all" ? t("common.all") : "GIF / Anim"}
              </button>
            ))}
          </div>

          <label className="hidden h-7 w-52 items-center gap-1.5 rounded-lg bg-muted/70 px-2.5 sm:flex">
            <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={t("ip.search_images")}
              className="min-w-0 flex-1 bg-transparent text-xs outline-none placeholder:text-text-tertiary"
            />
          </label>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto p-5">
        {items.length === 0 ? (
          <div className="flex h-full flex-col items-center justify-center gap-1 text-center">
            <p className="text-sm text-muted-foreground">
              {galleryIds.length === 0 ? t("ip.gallery_empty") : t("sb.no_match")}
            </p>
          </div>
        ) : (
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5">
            {items.map((img) => (
              <ImageCard key={img.id} image={img} listIds={listIds} aspectSquare />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
