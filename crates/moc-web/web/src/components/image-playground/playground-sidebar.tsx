// Image Playground 左侧导航栏：三大平行视图（生成 / 画廊 / 收藏）+ 会话列表 + 底部设置入口。

import {
  ArrowLeftIcon,
  ImagesIcon,
  MoreHorizontalIcon,
  PanelLeftCloseIcon,
  PencilIcon,
  SearchIcon,
  ServerIcon,
  SettingsIcon,
  SparklesIcon,
  SquarePenIcon,
  StarIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { type ReactNode, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { IconButton } from "@/components/common/icon-button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { relativeTime } from "@/lib/format";
import { useT } from "@/lib/i18n";
import type { PlaygroundSessionSummary } from "@/lib/image-playground-api";
import { cn } from "@/lib/utils";
import { type PlaygroundView, useImagePlayground } from "@/store/image-playground";
import { useLayout } from "@/store/layout";
import { openSettings } from "@/store/ui";

const ROW = "flex h-8 w-full items-center gap-2 rounded-md px-2 text-ui transition-colors";

function NavRow({
  icon,
  label,
  count,
  active,
  onClick,
}: {
  icon: ReactNode;
  label: string;
  count?: number;
  active?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cn(
        ROW,
        "group/nav text-sidebar-foreground hover:bg-sidebar-accent",
        active && "bg-sidebar-accent font-medium",
      )}
    >
      <span className="flex size-4 items-center justify-center text-muted-foreground group-hover/nav:text-foreground [&_svg]:size-[15px]">
        {icon}
      </span>
      <span className="flex-1 truncate text-left">{label}</span>
      {count !== undefined && count > 0 && (
        <span className="shrink-0 font-mono text-2xs tabular-nums text-text-tertiary">{count}</span>
      )}
    </button>
  );
}

export function PlaygroundSidebar({ onBackToAgent }: { onBackToAgent: () => void }) {
  const t = useT();
  const view = useImagePlayground((s) => s.view);
  const setView = useImagePlayground((s) => s.setView);
  const sessions = useImagePlayground((s) => s.sessions);
  const activeSessionId = useImagePlayground((s) => s.activeSessionId);
  const galleryCount = useImagePlayground((s) => s.galleryIds.length);
  const favCount = useImagePlayground((s) => s.favoriteIds.length + s.favoritePresets.length);
  const newSession = useImagePlayground((s) => s.newSession);

  const [query, setQuery] = useState("");
  const [searching, setSearching] = useState(false);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return sessions;
    return sessions.filter((s) => s.title.toLowerCase().includes(q));
  }, [sessions, query]);

  const switchView = (next: PlaygroundView) => {
    setView(next);
  };

  return (
    <nav className="flex h-full w-full min-w-0 flex-col overflow-hidden bg-sidebar text-sidebar-foreground select-none">
      {/* 顶栏 */}
      <div className="flex h-11 shrink-0 items-center justify-between gap-1 pr-2 pl-2">
        <div className="flex min-w-0 items-center gap-1">
          <IconButton label={t("ip.back_agent")} size="md" onClick={onBackToAgent}>
            <ArrowLeftIcon />
          </IconButton>
          <h2 className="truncate text-sm font-medium">Image Playground</h2>
        </div>
        <div className="flex shrink-0 items-center gap-0.5">
          <IconButton
            label={t("common.search")}
            size="md"
            active={searching}
            onClick={() => {
              setSearching((v) => !v);
              setQuery("");
            }}
          >
            <SearchIcon />
          </IconButton>
          <IconButton
            label={t("layout.close_sidebar")}
            size="md"
            onClick={() => useLayout.getState().toggleSidebar()}
          >
            <PanelLeftCloseIcon />
          </IconButton>
        </div>
      </div>

      {searching && (
        <div className="shrink-0 px-2 pb-1">
          <div className={cn(ROW, "bg-sidebar-accent")}>
            <SearchIcon className="size-4 shrink-0 text-muted-foreground" />
            <input
              // biome-ignore lint/a11y/noAutofocus: 搜索行由用户点击展开
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setSearching(false);
                  setQuery("");
                }
              }}
              placeholder={t("ip.search_sessions")}
              className="min-w-0 flex-1 bg-transparent text-ui outline-none"
            />
            <IconButton
              label={t("common.close")}
              size="xs"
              onClick={() => {
                setSearching(false);
                setQuery("");
              }}
            >
              <XIcon />
            </IconButton>
          </div>
        </div>
      )}

      {/* 三大平行视图导航 + 会话列表 */}
      <div className="min-h-0 flex-1 overflow-x-hidden overflow-y-auto px-2 pb-2">
        <div className="space-y-0.5">
          <NavRow
            icon={<SparklesIcon />}
            label={t("ip.nav_generate")}
            active={view === "generate"}
            onClick={() => switchView("generate")}
          />
          <NavRow
            icon={<ImagesIcon />}
            label={t("ip.nav_gallery")}
            count={galleryCount}
            active={view === "gallery"}
            onClick={() => switchView("gallery")}
          />
          <NavRow
            icon={<StarIcon />}
            label={t("ip.nav_favorites")}
            count={favCount}
            active={view === "favorites"}
            onClick={() => switchView("favorites")}
          />
        </div>

        <div className="mt-3 flex h-8 items-center justify-between pl-2">
          <span className="text-xs text-text-tertiary">{t("ip.sessions")}</span>
          <IconButton
            label={t("ip.new_session")}
            onClick={() =>
              void newSession().catch((err) =>
                toast.error(t("ip.action_failed", { error: String(err) })),
              )
            }
          >
            <SquarePenIcon />
          </IconButton>
        </div>

        {filtered.length === 0 ? (
          <div className="px-2 py-6 text-center text-xs text-text-tertiary">
            {query ? t("sb.no_match") : t("ip.no_sessions")}
          </div>
        ) : (
          <div className="space-y-px">
            {filtered.map((session) => (
              <PlaygroundSessionRow
                key={session.id}
                session={session}
                active={view === "generate" && session.id === activeSessionId}
              />
            ))}
          </div>
        )}
      </div>

      {/* 底部栏 */}
      <div className="flex h-11 shrink-0 items-center justify-between border-t border-border-subtle pr-2 pl-4">
        <span className="text-xs font-medium text-muted-foreground">moc · studio</span>
        <div className="flex items-center gap-0.5">
          <IconButton
            label={t("sd.providers")}
            side="top"
            size="md"
            onClick={() => openSettings("providers")}
          >
            <ServerIcon />
          </IconButton>
          <IconButton
            label={t("ip.settings")}
            side="top"
            size="md"
            onClick={() => openSettings("image_playground")}
          >
            <SettingsIcon />
          </IconButton>
        </div>
      </div>
    </nav>
  );
}

function PlaygroundSessionRow({
  session,
  active,
}: {
  session: PlaygroundSessionSummary;
  active: boolean;
}) {
  const t = useT();
  const running = useImagePlayground((s) => Boolean(s.runningBySession[session.id]));
  const selectSession = useImagePlayground((s) => s.selectSession);
  const renameSession = useImagePlayground((s) => s.renameSession);
  const setConfirmDeleteSession = useImagePlayground((s) => s.setConfirmDeleteSession);

  const [renaming, setRenaming] = useState<string | null>(null);
  const renamingFromMenu = useRef(false);

  const commitRename = () => {
    const next = renaming?.trim();
    setRenaming(null);
    if (next && next !== session.title) {
      void renameSession(session.id, next).catch((err) =>
        toast.error(t("ip.action_failed", { error: String(err) })),
      );
    }
  };

  if (renaming !== null) {
    return (
      <div className={cn(ROW, "h-11 bg-sidebar-accent pl-3")}>
        <input
          // biome-ignore lint/a11y/noAutofocus: 重命名输入框
          autoFocus
          value={renaming}
          onChange={(e) => setRenaming(e.target.value)}
          onFocus={(e) => e.currentTarget.select()}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return;
            if (e.key === "Enter") commitRename();
            else if (e.key === "Escape") setRenaming(null);
          }}
          onBlur={commitRename}
          className="min-w-0 flex-1 bg-transparent text-ui outline-none"
        />
      </div>
    );
  }

  return (
    <div
      className={cn(
        ROW,
        "group/session relative h-11 gap-2 pr-1 pl-2.5 has-[[data-state=open]]:bg-sidebar-accent",
        active
          ? "bg-sidebar-accent text-sidebar-foreground"
          : "text-sidebar-foreground/75 hover:bg-sidebar-accent/70 hover:text-sidebar-foreground",
      )}
    >
      <button
        type="button"
        onClick={() => void selectSession(session.id)}
        onDoubleClick={() => setRenaming(session.title)}
        className="absolute inset-0 rounded-md"
        aria-label={session.title}
        title={session.title}
      />
      <span className="pointer-events-none flex size-3 shrink-0 items-center justify-center">
        {running ? (
          <span className="size-2.5 rounded-full border border-text-tertiary/40 border-t-foreground motion-safe:animate-spin" />
        ) : (
          <span className="size-1 rounded-full bg-text-tertiary/40" />
        )}
      </span>
      <span className="pointer-events-none flex min-w-0 flex-1 flex-col leading-tight">
        <span className="truncate text-sidebar-foreground">{session.title}</span>
        <span className="mt-0.5 truncate text-xs tabular-nums text-text-tertiary">
          {running
            ? t("ip.generating")
            : `${relativeTime(session.updated_at_ms)}${session.image_count > 0 ? ` · ${session.image_count}` : ""}`}
        </span>
      </span>
      <div className="pointer-events-none relative shrink-0 opacity-0 transition-opacity group-hover/session:pointer-events-auto group-hover/session:opacity-100 group-focus-within/session:pointer-events-auto group-focus-within/session:opacity-100 group-has-[[data-state=open]]/session:pointer-events-auto group-has-[[data-state=open]]/session:opacity-100 pointer-coarse:pointer-events-auto pointer-coarse:opacity-100">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              aria-label={t("sb.session_actions")}
              className="flex size-6 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
            >
              <MoreHorizontalIcon className="size-3.5" />
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            align="start"
            className="w-40"
            onCloseAutoFocus={(e) => {
              if (!renamingFromMenu.current) return;
              e.preventDefault();
              renamingFromMenu.current = false;
              setRenaming(session.title);
            }}
          >
            <DropdownMenuItem onSelect={() => (renamingFromMenu.current = true)}>
              <PencilIcon />
              {t("hd.rename")}
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              variant="destructive"
              onSelect={() => setConfirmDeleteSession(session)}
            >
              <Trash2Icon />
              {t("common.delete")}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </div>
  );
}
