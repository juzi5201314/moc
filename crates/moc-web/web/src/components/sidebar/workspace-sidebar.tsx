import {
  BlocksIcon,
  CheckIcon,
  CopyIcon,
  FolderIcon,
  FolderPlusIcon,
  GitForkIcon,
  Loader2Icon,
  MessageSquarePlusIcon,
  MoreHorizontalIcon,
  PanelLeftCloseIcon,
  PanelLeftOpenIcon,
  PencilIcon,
  PlusIcon,
  SearchIcon,
  SettingsIcon,
  SlidersHorizontalIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { toast } from "sonner";
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
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useLayout } from "@/store/layout";
import {
  createQuickSession,
  deleteSession,
  forkSession,
  openSession,
  renameSession,
} from "@/store/lifecycle";
import { allSessions, groupWorkspaces, useSessions, workspaceName } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { ScrollableTitle } from "./scrollable-title";

/** 每个工作区默认展示的会话数；超出折叠为「展开其余 N 个会话」。 */
const SESSION_WATERMARK_BASE = 5;

/** 侧栏里的紧凑图标按钮：工作区标题行与折叠态共用。 */
const ICON_BUTTON = cn(
  "flex size-7 items-center justify-center rounded-md text-muted-foreground transition-colors",
  "hover:bg-sidebar-accent hover:text-sidebar-accent-foreground",
);

export function WorkspaceSidebar() {
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const workspaces = useSessions((state) => state.workspaces);
  const draftSession = useSessions((state) => state.draftSession);
  const addWorkspace = useSessions((state) => state.addWorkspace);
  const removeWorkspace = useSessions((state) => state.removeWorkspace);

  const sidebarCollapsed = useLayout((state) => state.sidebarCollapsed);
  const toggleSidebar = useLayout((state) => state.toggleSidebar);
  const active = useTabs((state) => state.active);
  const activity = useTranscript((state) => state.activity);
  const permissions = useTranscript((state) => state.permissions);
  const asks = useTranscript((state) => state.asks);

  const openSettings = () => useUi.getState().set({ settings: { open: true } });

  // 搜索状态 (由工作区标题行的搜索按钮切换展开)
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const searchInputRef = useRef<HTMLInputElement>(null);

  // 视图与排序配置
  const [groupBy, setGroupBy] = useState<"workspace" | "flat">("workspace");
  const [orderBy, setOrderBy] = useState<"updated" | "manual">("updated");

  // 折叠状态记录 (以 cwd 为 key)
  const [collapsedWorkspaces, setCollapsedWorkspaces] = useState<Record<string, boolean>>({});
  // 水位线展开记录 (true 表示已展开全部会话)
  const [expandedWatermarks, setExpandedWatermarks] = useState<Record<string, boolean>>({});

  // 重命名状态
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");

  // 删除确认对话框
  const [deletingId, setDeletingId] = useState<string | null>(null);

  const sessions = useMemo(() => allSessions({ list, extra }, orderBy), [list, extra, orderBy]);
  const groupedWorkspaces = useMemo(
    () => groupWorkspaces(sessions, workspaces, searchQuery, orderBy),
    [sessions, workspaces, searchQuery, orderBy],
  );

  const handleToggleSearch = () => {
    if (sidebarCollapsed) {
      toggleSidebar();
      setSearchOpen(true);
      setTimeout(() => searchInputRef.current?.focus(), 50);
      return;
    }
    setSearchOpen((prev) => {
      const next = !prev;
      if (next) {
        setTimeout(() => searchInputRef.current?.focus(), 50);
      } else {
        setSearchQuery("");
      }
      return next;
    });
  };

  const handleToggleWorkspaceFold = (cwd: string) => {
    setCollapsedWorkspaces((prev) => ({
      ...prev,
      [cwd]: !prev[cwd],
    }));
  };

  const handleToggleWatermark = (cwd: string) => {
    setExpandedWatermarks((prev) => ({
      ...prev,
      [cwd]: !prev[cwd],
    }));
  };

  const handleAddWorkspace = () => {
    useUi.getState().set({
      folderPicker: {
        open: true,
        onPick: (path) => {
          addWorkspace(path);
          createQuickSession(path);
          toast.success(`已添加工作区: ${workspaceName(path)}`);
        },
      },
    });
  };

  const handleStartRename = (id: string, currentTitle: string) => {
    setRenamingId(id);
    setRenameValue(currentTitle);
  };

  const handleCommitRename = async (id: string) => {
    const trimmed = renameValue.trim();
    setRenamingId(null);
    if (trimmed) {
      await renameSession(id, trimmed);
    }
  };

  const handleCopyTitle = (title: string) => {
    void navigator.clipboard.writeText(title);
    toast.success("已复制会话标题");
  };

  const handleCopyId = (id: string) => {
    void navigator.clipboard.writeText(id);
    toast.success("已复制会话 ID");
  };

  const handleFork = (id: string) => {
    void forkSession(id);
  };

  const handleDeleteConfirm = async () => {
    if (deletingId) {
      const id = deletingId;
      setDeletingId(null);
      await deleteSession(id);
      toast.success("会话已删除");
    }
  };

  return (
    <div className="flex h-full w-full flex-col bg-sidebar text-sidebar-foreground select-none">
      {/* 1. 品牌行：只有 moc 字样与折叠开关 */}
      <div
        className={cn(
          "flex shrink-0 items-center",
          sidebarCollapsed ? "h-11 justify-center px-1" : "h-12 justify-between px-3",
        )}
      >
        {!sidebarCollapsed && (
          <span className="text-[17px] font-semibold tracking-tight text-foreground">moc</span>
        )}

        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              onClick={toggleSidebar}
              className={ICON_BUTTON}
              aria-label={sidebarCollapsed ? "展开工作区侧栏" : "收起工作区侧栏"}
            >
              {sidebarCollapsed ? (
                <PanelLeftOpenIcon className="size-4" />
              ) : (
                <PanelLeftCloseIcon className="size-4" />
              )}
            </button>
          </TooltipTrigger>
          <TooltipContent side={sidebarCollapsed ? "right" : "bottom"}>
            {sidebarCollapsed ? "展开工作区侧栏" : "收起工作区侧栏"}
          </TooltipContent>
        </Tooltip>
      </div>

      {/* 2. 新会话与插件入口 */}
      <div
        className={cn(
          "shrink-0",
          sidebarCollapsed ? "flex flex-col items-center gap-1 px-1" : "space-y-1 px-3",
        )}
      >
        {sidebarCollapsed ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="outline"
                size="icon-sm"
                aria-label="新会话"
                onClick={() => createQuickSession()}
              >
                <MessageSquarePlusIcon />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="right">新会话</TooltipContent>
          </Tooltip>
        ) : (
          <Button
            type="button"
            variant="outline"
            onClick={() => createQuickSession()}
            className="h-10 w-full justify-center gap-2 rounded-md border-border bg-background text-sm font-medium shadow-none hover:bg-accent"
          >
            <MessageSquarePlusIcon className="size-4" />
            新会话
          </Button>
        )}

        {/* 插件面板尚未接入 WebUI，这里只占位，保持后续入口的位置稳定 */}
        <button
          type="button"
          disabled
          aria-label="插件"
          title="插件面板即将支持"
          className={cn(
            "flex h-9 items-center gap-2 rounded-md text-sm text-muted-foreground/70",
            sidebarCollapsed ? "w-9 justify-center" : "w-full px-2",
          )}
        >
          <BlocksIcon className="size-4" />
          {!sidebarCollapsed && <span>插件</span>}
        </button>
      </div>

      {/* 3. 工作区标题行：搜索 / 视图与排序 / 添加目录 */}
      <div
        className={cn(
          "mt-2 flex h-8 shrink-0 items-center",
          sidebarCollapsed ? "justify-center" : "justify-between px-3",
        )}
      >
        {!sidebarCollapsed && <span className="text-sm text-[var(--text-tertiary)]">工作区</span>}

        <div className={cn("flex items-center", sidebarCollapsed ? "flex-col gap-1" : "gap-0.5")}>
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={handleToggleSearch}
                className={cn(
                  ICON_BUTTON,
                  searchOpen && "bg-sidebar-accent text-sidebar-accent-foreground",
                )}
                aria-label="搜索会话"
              >
                <SearchIcon className="size-3.5" />
              </button>
            </TooltipTrigger>
            <TooltipContent side={sidebarCollapsed ? "right" : "bottom"}>搜索会话</TooltipContent>
          </Tooltip>

          {!sidebarCollapsed && (
            <DropdownMenu>
              <Tooltip>
                <TooltipTrigger asChild>
                  <DropdownMenuTrigger asChild>
                    <button type="button" className={ICON_BUTTON} aria-label="视图与排序选项">
                      <SlidersHorizontalIcon className="size-3.5" />
                    </button>
                  </DropdownMenuTrigger>
                </TooltipTrigger>
                <TooltipContent side="bottom">视图与排序选项</TooltipContent>
              </Tooltip>
              <DropdownMenuContent align="end" className="w-44 text-xs">
                <DropdownMenuLabel className="text-[11px] font-medium text-muted-foreground">
                  分组方式
                </DropdownMenuLabel>
                <DropdownMenuItem onClick={() => setGroupBy("workspace")} className="gap-2">
                  <FolderIcon className="size-3.5" />
                  <span className="flex-1">工作区</span>
                  {groupBy === "workspace" && <CheckIcon className="size-3 text-primary" />}
                </DropdownMenuItem>
                <DropdownMenuItem onClick={() => setGroupBy("flat")} className="gap-2">
                  <span className="flex-1">平铺列表</span>
                  {groupBy === "flat" && <CheckIcon className="size-3 text-primary" />}
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuLabel className="text-[11px] font-medium text-muted-foreground">
                  排序方式
                </DropdownMenuLabel>
                <DropdownMenuItem onClick={() => setOrderBy("updated")} className="gap-2">
                  <span className="flex-1">更新时间</span>
                  {orderBy === "updated" && <CheckIcon className="size-3 text-primary" />}
                </DropdownMenuItem>
                <DropdownMenuItem onClick={() => setOrderBy("manual")} className="gap-2">
                  <span className="flex-1">手动排序</span>
                  {orderBy === "manual" && <CheckIcon className="size-3 text-primary" />}
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          )}

          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={handleAddWorkspace}
                className={ICON_BUTTON}
                aria-label="添加工作区目录"
              >
                <FolderPlusIcon className="size-3.5" />
              </button>
            </TooltipTrigger>
            <TooltipContent side={sidebarCollapsed ? "right" : "bottom"}>
              添加工作区目录
            </TooltipContent>
          </Tooltip>
        </div>
      </div>

      {/* 4. 搜索框 (点击搜索图标后展开显示，非永久常驻) */}
      {searchOpen && !sidebarCollapsed && (
        <div className="mt-1 flex shrink-0 items-center gap-1.5 rounded-md bg-sidebar-accent/60 px-2 py-1">
          <SearchIcon className="size-3 shrink-0 text-muted-foreground" />
          <input
            ref={searchInputRef}
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder="搜索会话..."
            className="flex-1 bg-transparent text-xs outline-none placeholder:text-muted-foreground"
          />
          {searchQuery && (
            <button
              type="button"
              onClick={() => setSearchQuery("")}
              className="text-muted-foreground hover:text-foreground"
              aria-label="清空搜索"
            >
              <XIcon className="size-3" />
            </button>
          )}
        </div>
      )}

      {/* 5. 工作区 / 会话树。这里用原生滚动容器而不是 ScrollArea：Radix 的 viewport
          内层是 `display: table`，会把 `truncate` 的会话标题撑到整行宽度，时间戳被挤出可视区。 */}
      <div
        className={cn(
          "mt-1 min-h-0 flex-1 overflow-x-hidden overflow-y-auto px-2",
          sidebarCollapsed && "hidden",
        )}
      >
        {groupBy === "workspace" ? (
          <div className="pb-2">
            {groupedWorkspaces.map((workspace) => {
              const isCollapsed = Boolean(collapsedWorkspaces[workspace.cwd]);
              const isWatermarkExpanded = Boolean(expandedWatermarks[workspace.cwd]);

              // 检查当前是否有空白草稿属于该工作区
              const hasDraftInWorkspace = draftSession && draftSession.cwd === workspace.cwd;

              const totalSessions = workspace.sessions;
              const hasOverflow = totalSessions.length > SESSION_WATERMARK_BASE;
              const visibleSessions =
                isWatermarkExpanded || !hasOverflow
                  ? totalSessions
                  : totalSessions.slice(0, SESSION_WATERMARK_BASE);
              const hiddenCount = totalSessions.length - visibleSessions.length;

              // 当前会话（或空白草稿）所在的工作区，文件夹图标用主色标出
              const hasActiveSession =
                active === "draft-session"
                  ? draftSession?.cwd === workspace.cwd
                  : totalSessions.some((session) => session.id === active);

              return (
                <div key={workspace.cwd} className="mb-1">
                  {/* 工作区文件夹行 */}
                  <div
                    role="treeitem"
                    tabIndex={0}
                    onClick={() => handleToggleWorkspaceFold(workspace.cwd)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        handleToggleWorkspaceFold(workspace.cwd);
                      }
                    }}
                    className="group/ws flex h-[34px] cursor-pointer items-center gap-2 rounded-md px-2 text-sm font-medium text-sidebar-foreground transition-colors hover:bg-sidebar-accent/60"
                  >
                    <FolderIcon
                      className={cn(
                        "size-4 shrink-0",
                        hasActiveSession ? "text-primary" : "text-muted-foreground",
                      )}
                    />
                    <span className="truncate text-foreground/90">{workspace.name}</span>

                    {/* 悬停操作按钮：+ 新建会话，··· 更多 */}
                    <div
                      role="toolbar"
                      aria-label="工作区操作"
                      className="ml-auto flex shrink-0 items-center gap-0.5 opacity-0 transition-opacity group-hover/ws:opacity-100"
                      onClick={(e) => e.stopPropagation()}
                      onKeyDown={(e) => e.stopPropagation()}
                    >
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <button
                            type="button"
                            onClick={() => createQuickSession(workspace.cwd)}
                            className="flex size-5 items-center justify-center rounded text-muted-foreground hover:bg-sidebar-accent hover:text-foreground"
                            aria-label="在当前工作区新建会话"
                          >
                            <PlusIcon className="size-3.5" />
                          </button>
                        </TooltipTrigger>
                        <TooltipContent side="top">新建会话</TooltipContent>
                      </Tooltip>

                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <button
                            type="button"
                            className="flex size-5 items-center justify-center rounded text-muted-foreground hover:bg-sidebar-accent hover:text-foreground"
                            aria-label="工作区操作"
                          >
                            <MoreHorizontalIcon className="size-3.5" />
                          </button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end" className="w-40 text-[13px]">
                          <DropdownMenuItem
                            onClick={() => {
                              void navigator.clipboard.writeText(workspace.cwd);
                              toast.success("已复制工作区完整路径");
                            }}
                          >
                            复制完整路径
                          </DropdownMenuItem>
                          {workspaces.includes(workspace.cwd) && (
                            <DropdownMenuItem
                              onClick={() => {
                                removeWorkspace(workspace.cwd);
                                toast.success("已从工作区列表中移除");
                              }}
                              className="text-destructive focus:text-destructive"
                            >
                              删除工作区
                            </DropdownMenuItem>
                          )}
                        </DropdownMenuContent>
                      </DropdownMenu>
                    </div>
                  </div>

                  {/* 展开的会话列表 */}
                  {!isCollapsed && (
                    <div className="mt-0.5 ml-5 space-y-0.5">
                      {/* 如果有空白本地草稿属于当前工作区，优先渲染草稿行 */}
                      {hasDraftInWorkspace && (
                        <div
                          role="treeitem"
                          tabIndex={0}
                          onClick={() => useTabs.getState().add("draft-session")}
                          onKeyDown={(e) => {
                            if (e.key === "Enter" || e.key === " ") {
                              e.preventDefault();
                              useTabs.getState().add("draft-session");
                            }
                          }}
                          className={cn(
                            "flex h-8 cursor-pointer items-center rounded-md px-2 text-sm transition-colors",
                            active === "draft-session"
                              ? "bg-sidebar-accent font-medium text-sidebar-accent-foreground"
                              : "text-foreground/75 hover:bg-sidebar-accent/50 hover:text-foreground",
                          )}
                        >
                          <span className="truncate">新会话</span>
                        </div>
                      )}

                      {/* 渲染真实会话 */}
                      {visibleSessions.map((session) => {
                        const isSelected = active === session.id;
                        const isRunning = Boolean(activity[session.id]?.running);
                        const hasPendingInteraction = Boolean(
                          permissions[session.id]?.length || asks[session.id]?.length,
                        );

                        return (
                          <div
                            key={session.id}
                            role="treeitem"
                            tabIndex={0}
                            onClick={() => void openSession(session.id)}
                            onKeyDown={(e) => {
                              if (e.key === "Enter" || e.key === " ") {
                                e.preventDefault();
                                void openSession(session.id);
                              }
                            }}
                            className={cn(
                              "group/session relative flex h-8 cursor-pointer items-center gap-1.5 rounded-md px-2 text-sm transition-colors",
                              isSelected
                                ? "bg-sidebar-accent font-medium text-sidebar-accent-foreground"
                                : "text-foreground/75 hover:bg-sidebar-accent/50 hover:text-foreground",
                            )}
                          >
                            {/* 运行中 / 待确认才出现的前导标记，静止会话不占位 */}
                            {isRunning ? (
                              <Loader2Icon className="size-3 shrink-0 animate-spin text-primary" />
                            ) : hasPendingInteraction ? (
                              <span className="size-1.5 shrink-0 rounded-full bg-amber-500" />
                            ) : null}

                            {renamingId === session.id ? (
                              <input
                                type="text"
                                value={renameValue}
                                onChange={(e) => setRenameValue(e.target.value)}
                                onKeyDown={(e) => {
                                  if (e.key === "Enter") void handleCommitRename(session.id);
                                  if (e.key === "Escape") setRenamingId(null);
                                }}
                                onBlur={() => void handleCommitRename(session.id)}
                                onClick={(e) => e.stopPropagation()}
                                className="h-5 min-w-0 flex-1 rounded border border-primary bg-background px-1 text-xs outline-none"
                              />
                            ) : (
                              <ScrollableTitle
                                text={session.title || "未命名会话"}
                                className="min-w-0 flex-1"
                              />
                            )}

                            {/* 右侧：常驻相对时间；悬停时换成操作菜单（原位覆盖，不改变布局）
                                这里用 visibility 而不是 display，否则菜单打开时锚点元素失去盒子，
                                Radix 会把弹出层重新定位到视口左上角。 */}
                            <span className="shrink-0 text-[11px] text-[var(--text-tertiary)] group-hover/session:invisible">
                              {relativeTime(session.updated_at_ms)}
                            </span>

                            <div
                              role="toolbar"
                              aria-label="会话操作"
                              className="invisible absolute top-1/2 right-1 flex -translate-y-1/2 items-center group-hover/session:visible"
                              onClick={(e) => e.stopPropagation()}
                              onKeyDown={(e) => e.stopPropagation()}
                            >
                              <DropdownMenu>
                                <DropdownMenuTrigger asChild>
                                  <button
                                    type="button"
                                    className="flex size-5 items-center justify-center rounded text-muted-foreground hover:bg-sidebar-accent hover:text-foreground"
                                    aria-label="会话操作菜单"
                                  >
                                    <MoreHorizontalIcon className="size-3.5" />
                                  </button>
                                </DropdownMenuTrigger>
                                <DropdownMenuContent align="end" className="w-40 text-[13px]">
                                  <DropdownMenuItem
                                    onClick={() =>
                                      handleStartRename(session.id, session.title || "未命名会话")
                                    }
                                    className="gap-2"
                                  >
                                    <PencilIcon className="size-3.5" />
                                    <span>重命名</span>
                                  </DropdownMenuItem>
                                  <DropdownMenuItem
                                    onClick={() => handleFork(session.id)}
                                    className="gap-2"
                                  >
                                    <GitForkIcon className="size-3.5" />
                                    <span>分叉会话</span>
                                  </DropdownMenuItem>
                                  <DropdownMenuItem
                                    onClick={() => handleCopyTitle(session.title || "未命名会话")}
                                    className="gap-2"
                                  >
                                    <CopyIcon className="size-3.5" />
                                    <span>复制标题</span>
                                  </DropdownMenuItem>
                                  <DropdownMenuItem
                                    onClick={() => handleCopyId(session.id)}
                                    className="gap-2"
                                  >
                                    <CopyIcon className="size-3.5" />
                                    <span>复制 ID</span>
                                  </DropdownMenuItem>
                                  <DropdownMenuSeparator />
                                  <DropdownMenuItem
                                    onClick={() => setDeletingId(session.id)}
                                    className="gap-2 text-destructive focus:text-destructive"
                                  >
                                    <Trash2Icon className="size-3.5" />
                                    <span>删除</span>
                                  </DropdownMenuItem>
                                </DropdownMenuContent>
                              </DropdownMenu>
                            </div>
                          </div>
                        );
                      })}

                      {/* 水位线截断按钮：展开其余 N 个会话 / 收起 */}
                      {hasOverflow && (
                        <button
                          type="button"
                          onClick={() => handleToggleWatermark(workspace.cwd)}
                          className="flex h-8 w-full items-center rounded-md px-2 text-[12px] text-[var(--text-tertiary)] transition-colors hover:text-foreground"
                        >
                          {isWatermarkExpanded ? "收起" : `展开其余 ${hiddenCount} 个会话`}
                        </button>
                      )}
                    </div>
                  )}
                </div>
              );
            })}

            {groupedWorkspaces.length === 0 && (
              <div className="py-8 text-center text-xs text-muted-foreground">暂无会话或工作区</div>
            )}
          </div>
        ) : (
          // 平铺列表模式
          <div className="space-y-0.5 pb-2">
            {sessions.map((session) => (
              <div
                key={session.id}
                role="treeitem"
                tabIndex={0}
                onClick={() => void openSession(session.id)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    void openSession(session.id);
                  }
                }}
                className={cn(
                  "group/session flex h-8 cursor-pointer items-center gap-1.5 rounded-md px-2 text-sm transition-colors",
                  active === session.id
                    ? "bg-sidebar-accent font-medium text-sidebar-accent-foreground"
                    : "text-foreground/75 hover:bg-sidebar-accent/50 hover:text-foreground",
                )}
              >
                <ScrollableTitle text={session.title || "未命名会话"} className="min-w-0 flex-1" />
                <span className="shrink-0 text-[11px] text-[var(--text-tertiary)]">
                  {relativeTime(session.updated_at_ms)}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 6. 底部入口：设置 */}
      <div
        className={cn(
          "shrink-0 border-t border-border/60",
          sidebarCollapsed ? "flex flex-col items-center px-1 py-2" : "p-2",
        )}
      >
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              onClick={openSettings}
              aria-label="设置"
              className={cn(
                "flex items-center gap-2 rounded-md text-sm font-medium text-foreground/80 transition-colors hover:bg-sidebar-accent hover:text-foreground",
                sidebarCollapsed ? "size-9 justify-center" : "h-10 w-full px-2",
              )}
            >
              <SettingsIcon className="size-4 text-muted-foreground" />
              {!sidebarCollapsed && <span className="truncate">设置</span>}
            </button>
          </TooltipTrigger>
          <TooltipContent side={sidebarCollapsed ? "right" : "top"}>设置</TooltipContent>
        </Tooltip>
      </div>

      {/* 删除会话确认弹窗 */}
      <AlertDialog open={Boolean(deletingId)} onOpenChange={(open) => !open && setDeletingId(null)}>
        <AlertDialogContent className="sm:max-w-md">
          <AlertDialogHeader>
            <AlertDialogTitle className="text-sm font-semibold">删除会话</AlertDialogTitle>
            <AlertDialogDescription className="text-xs text-muted-foreground">
              删除后会话记录与本地上下文将无法恢复。确定要删除该会话吗？
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel className="text-xs">取消</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => void handleDeleteConfirm()}
              className="text-xs bg-destructive text-destructive-foreground hover:bg-destructive/90"
            >
              确认删除
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
