// 左侧工作区栏：品牌 + 全局入口（新会话 / 搜索 / 插件）+ 按目录分组的会话树 + 设置。

import {
  ChevronRightIcon,
  CopyIcon,
  FolderPlusIcon,
  MoreHorizontalIcon,
  PanelLeftCloseIcon,
  PencilIcon,
  PlusIcon,
  PuzzleIcon,
  SearchIcon,
  SettingsIcon,
  SquarePenIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { type ReactNode, useMemo, useState } from "react";
import { toast } from "sonner";
import { IconButton, Kbd } from "@/components/common/icon-button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { SessionInfo } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useLayout } from "@/store/layout";
import { createQuickSession, openSession, renameSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, groupWorkspaces, useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { attentionCount, useTranscript } from "@/store/transcript";
import { openSettings, useUi } from "@/store/ui";

/** 每个工作区默认展示的会话数；更多的折叠在「显示更多」后面。 */
const VISIBLE_SESSIONS = 6;

const ROW = "flex h-8 w-full items-center gap-2 rounded-lg px-2 text-[13px] transition-colors";

function NavRow({
  icon,
  label,
  hint,
  onClick,
}: {
  icon: ReactNode;
  label: string;
  hint?: ReactNode;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        ROW,
        "group/nav text-sidebar-foreground/85 hover:bg-sidebar-accent hover:text-sidebar-foreground",
      )}
    >
      <span className="flex size-4 items-center justify-center text-muted-foreground group-hover/nav:text-foreground [&_svg]:size-4">
        {icon}
      </span>
      <span className="flex-1 truncate text-left">{label}</span>
      {hint && (
        <span className="opacity-0 transition-opacity group-hover/nav:opacity-100">{hint}</span>
      )}
    </button>
  );
}

export function WorkspaceSidebar() {
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const workspaces = usePrefs((state) => state.workspaces);
  const draft = useSessions((state) => state.draftSession);
  const active = useTabs((state) => state.active);
  const status = useConnection((state) => state.status);
  const server = useConnection((state) => state.server);

  const [query, setQuery] = useState("");
  const [searching, setSearching] = useState(false);
  const [folded, setFolded] = useState<Record<string, boolean>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const groups = useMemo(() => {
    const dirs = draft?.cwd ? [...workspaces, draft.cwd] : workspaces;
    return groupWorkspaces(allSessions({ list, extra }), dirs, query);
  }, [list, extra, workspaces, draft?.cwd, query]);

  const addWorkspace = () =>
    useUi.getState().set({
      folderPicker: {
        open: true,
        onPick: (path) => {
          usePrefs.getState().addWorkspace(path);
          createQuickSession(path);
        },
      },
    });

  const closeSearch = () => {
    setSearching(false);
    setQuery("");
  };

  return (
    <nav className="flex h-full w-full flex-col text-sidebar-foreground select-none">
      <div className="flex h-12 shrink-0 items-center justify-between pr-2 pl-4">
        <div className="flex items-center gap-2">
          <span className="flex size-[22px] items-center justify-center rounded-[7px] bg-foreground text-[11px] font-bold tracking-tight text-background">
            m
          </span>
          <span className="text-[15px] font-semibold tracking-tight">moc</span>
        </div>
        <IconButton
          label="收起侧栏"
          shortcut="Ctrl B"
          onClick={() => useLayout.getState().toggleSidebar()}
        >
          <PanelLeftCloseIcon />
        </IconButton>
      </div>

      <div className="shrink-0 space-y-px px-2">
        <NavRow
          icon={<SquarePenIcon />}
          label="新会话"
          hint={<Kbd>Alt N</Kbd>}
          onClick={() => createQuickSession()}
        />
        {searching ? (
          <div className={cn(ROW, "bg-sidebar-accent")}>
            <SearchIcon className="size-4 shrink-0 text-muted-foreground" />
            <input
              // biome-ignore lint/a11y/noAutofocus: 搜索行由用户点击展开，立即可输入
              autoFocus
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => event.key === "Escape" && closeSearch()}
              placeholder="筛选会话与目录"
              className="min-w-0 flex-1 bg-transparent text-[13px] outline-none"
            />
            <button
              type="button"
              aria-label="关闭筛选"
              onClick={closeSearch}
              className="text-muted-foreground hover:text-foreground"
            >
              <XIcon className="size-3.5" />
            </button>
          </div>
        ) : (
          <NavRow
            icon={<SearchIcon />}
            label="搜索"
            hint={<Kbd>Ctrl K</Kbd>}
            onClick={() => setSearching(true)}
          />
        )}
        <NavRow icon={<PuzzleIcon />} label="插件" onClick={() => openSettings("plugins")} />
      </div>

      <div className="mt-4 flex h-7 shrink-0 items-center justify-between pr-2 pl-4">
        <span className="text-xs font-medium text-text-tertiary">工作区</span>
        <IconButton label="添加工作区目录" size="xs" onClick={addWorkspace}>
          <FolderPlusIcon />
        </IconButton>
      </div>

      {/* 原生滚动容器：Radix ScrollArea 的 table 布局会让 truncate 失效 */}
      <div className="min-h-0 flex-1 overflow-x-hidden overflow-y-auto px-2 pb-2">
        {groups.length === 0 ? (
          <div className="px-2 py-8 text-center text-xs leading-5 text-text-tertiary">
            {query ? "没有匹配的会话" : "添加一个工作区目录开始"}
          </div>
        ) : (
          groups.map((group) => {
            const isFolded = Boolean(folded[group.cwd]);
            const showAll = Boolean(expanded[group.cwd]) || Boolean(query);
            const visible = showAll ? group.sessions : group.sessions.slice(0, VISIBLE_SESSIONS);
            const hidden = group.sessions.length - visible.length;
            const hasDraft = draft?.cwd === group.cwd;
            return (
              <div key={group.cwd} className="mb-1">
                <div
                  className={cn(ROW, "group/ws cursor-pointer pr-1 hover:bg-sidebar-accent")}
                  title={group.cwd}
                >
                  <button
                    type="button"
                    className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
                    onClick={() => setFolded((state) => ({ ...state, [group.cwd]: !isFolded }))}
                  >
                    <ChevronRightIcon
                      className={cn(
                        "size-3.5 shrink-0 text-text-tertiary transition-transform duration-150",
                        !isFolded && "rotate-90",
                      )}
                    />
                    <span className="truncate font-medium text-sidebar-foreground/90">
                      {group.name}
                    </span>
                  </button>
                  <div className="flex shrink-0 items-center opacity-0 transition-opacity group-hover/ws:opacity-100 has-[[data-state=open]]:opacity-100">
                    <IconButton
                      label="在此目录新建会话"
                      size="xs"
                      onClick={() => createQuickSession(group.cwd)}
                    >
                      <PlusIcon />
                    </IconButton>
                    <WorkspaceMenu cwd={group.cwd} removable={workspaces.includes(group.cwd)} />
                  </div>
                </div>

                {!isFolded && (
                  <div className="mt-px space-y-px">
                    {hasDraft && (
                      <button
                        type="button"
                        onClick={() => useTabs.getState().add("draft-session")}
                        className={cn(
                          ROW,
                          "pl-[26px]",
                          active === "draft-session"
                            ? "bg-sidebar-accent text-sidebar-foreground"
                            : "text-muted-foreground hover:bg-sidebar-accent",
                        )}
                      >
                        <span className="truncate italic">新会话</span>
                      </button>
                    )}
                    {visible.map((session) => (
                      <SessionRow
                        key={session.id}
                        session={session}
                        active={session.id === active}
                      />
                    ))}
                    {hidden > 0 && (
                      <button
                        type="button"
                        onClick={() => setExpanded((state) => ({ ...state, [group.cwd]: true }))}
                        className={cn(
                          ROW,
                          "pl-[26px] text-xs text-text-tertiary hover:text-foreground",
                        )}
                      >
                        显示更多（{hidden}）
                      </button>
                    )}
                    {expanded[group.cwd] && !query && group.sessions.length > VISIBLE_SESSIONS && (
                      <button
                        type="button"
                        onClick={() => setExpanded((state) => ({ ...state, [group.cwd]: false }))}
                        className={cn(
                          ROW,
                          "pl-[26px] text-xs text-text-tertiary hover:text-foreground",
                        )}
                      >
                        收起
                      </button>
                    )}
                    {!hasDraft && group.sessions.length === 0 && (
                      <div className="py-1.5 pl-[26px] text-xs text-text-tertiary">暂无会话</div>
                    )}
                  </div>
                )}
              </div>
            );
          })
        )}
      </div>

      <div className="flex shrink-0 items-center gap-1 border-t border-border-subtle px-2 py-2">
        <button
          type="button"
          onClick={() => openSettings()}
          className={cn(ROW, "flex-1 text-sidebar-foreground/85 hover:bg-sidebar-accent")}
        >
          <SettingsIcon className="size-4 text-muted-foreground" />
          <span>设置</span>
        </button>
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="flex size-8 items-center justify-center">
              <span
                className={cn(
                  "size-2 rounded-full",
                  status === "online"
                    ? "bg-success"
                    : status === "connecting"
                      ? "animate-pulse bg-warn"
                      : "bg-destructive",
                )}
              />
            </span>
          </TooltipTrigger>
          <TooltipContent side="top">
            {status === "online"
              ? `已连接 ${server?.server.name ?? "moc-core"} ${server?.server.version ?? ""}`
              : status === "connecting"
                ? "正在连接…"
                : "连接已断开，正在重试"}
          </TooltipContent>
        </Tooltip>
      </div>
    </nav>
  );
}

function WorkspaceMenu({ cwd, removable }: { cwd: string; removable: boolean }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          aria-label="工作区操作"
          className="flex size-6 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <MoreHorizontalIcon className="size-3.5" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-48">
        <DropdownMenuItem
          onSelect={() => {
            void navigator.clipboard.writeText(cwd);
            toast.success("已复制路径");
          }}
        >
          <CopyIcon />
          复制路径
        </DropdownMenuItem>
        {removable && (
          <DropdownMenuItem
            variant="destructive"
            onSelect={() => usePrefs.getState().removeWorkspace(cwd)}
          >
            <XIcon />
            从列表移除
          </DropdownMenuItem>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function SessionRow({ session, active }: { session: SessionInfo; active: boolean }) {
  const running = useTranscript((state) => Boolean(state.activity[session.id]?.running));
  const attention = useTranscript((state) => attentionCount(state, session.id) > 0);
  const [renaming, setRenaming] = useState<string | null>(null);
  const title = session.title || "未命名会话";

  const commitRename = () => {
    const next = renaming?.trim();
    setRenaming(null);
    if (next && next !== title) {
      renameSession(session.id, next).catch((error: unknown) =>
        toast.error(`重命名失败：${error instanceof Error ? error.message : String(error)}`),
      );
    }
  };

  if (renaming !== null) {
    return (
      <div className={cn(ROW, "bg-sidebar-accent pl-[26px]")}>
        <input
          // biome-ignore lint/a11y/noAutofocus: 进入重命名后立即编辑
          autoFocus
          value={renaming}
          onChange={(event) => setRenaming(event.target.value)}
          onFocus={(event) => event.currentTarget.select()}
          onKeyDown={(event) => {
            if (event.key === "Enter") commitRename();
            if (event.key === "Escape") setRenaming(null);
          }}
          onBlur={commitRename}
          className="min-w-0 flex-1 bg-transparent text-[13px] outline-none"
        />
      </div>
    );
  }

  return (
    <div
      className={cn(
        ROW,
        "group/session relative pr-1 pl-[26px] has-[[data-state=open]]:bg-sidebar-accent",
        active
          ? "bg-sidebar-accent text-sidebar-foreground"
          : "text-sidebar-foreground/75 hover:bg-sidebar-accent/70 hover:text-sidebar-foreground",
      )}
    >
      <button
        type="button"
        onClick={() => void openSession(session.id)}
        onDoubleClick={() => setRenaming(title)}
        className="absolute inset-0 rounded-lg"
        aria-label={title}
        aria-current={active || undefined}
      />
      <span className="pointer-events-none min-w-0 flex-1 truncate" title={title}>
        {title}
      </span>
      {/* 时间与菜单共用一格：用 visibility 切换，菜单打开时锚点仍保有盒子，Radix 才能正确定位 */}
      <span className="relative flex h-6 min-w-6 shrink-0 items-center justify-end">
        <span className="pointer-events-none flex items-center group-hover/session:invisible group-has-[[data-state=open]]/session:invisible">
          {running ? (
            <span className="moc-dots flex gap-[3px]">
              <span className="size-1 rounded-full bg-brand" />
              <span className="size-1 rounded-full bg-brand" />
              <span className="size-1 rounded-full bg-brand" />
            </span>
          ) : attention ? (
            <span className="size-1.5 rounded-full bg-warn" title="等待你的操作" />
          ) : (
            <span className="text-[11px] tabular-nums text-text-tertiary">
              {relativeTime(session.updated_at_ms)}
            </span>
          )}
        </span>
        <div className="invisible absolute inset-y-0 right-0 flex items-center group-hover/session:visible group-has-[[data-state=open]]/session:visible">
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                aria-label="会话操作"
                className="flex size-6 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
              >
                <MoreHorizontalIcon className="size-3.5" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="w-44">
              <DropdownMenuItem onSelect={() => setRenaming(title)}>
                <PencilIcon />
                重命名
              </DropdownMenuItem>
              <DropdownMenuItem
                onSelect={() => {
                  void navigator.clipboard.writeText(session.id);
                  toast.success("已复制会话 ID");
                }}
              >
                <CopyIcon />
                复制会话 ID
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                variant="destructive"
                onSelect={() => useUi.getState().set({ confirmDelete: { id: session.id, title } })}
              >
                <Trash2Icon />
                删除
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </span>
    </div>
  );
}
