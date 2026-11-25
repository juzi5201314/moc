// 左侧会话栏：搜索、新会话、按目录分组的会话树，以及底部全局设置。

import {
  ArrowUpRightIcon,
  ChevronRightIcon,
  ChevronsUpIcon,
  CopyIcon,
  FolderPlusIcon,
  MoreHorizontalIcon,
  PencilIcon,
  PlusIcon,
  RotateCcwIcon,
  SearchIcon,
  SquarePenIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { type DragEvent, Fragment, type ReactNode, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { useShallow } from "zustand/react/shallow";
import { IconButton } from "@/components/common/icon-button";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { relativeTime } from "@/lib/format";
import { t, useT } from "@/lib/i18n";
import { useSessionSearch } from "@/lib/use-session-search";
import { cn } from "@/lib/utils";
import { ErrorCode, type RpcError } from "@/protocol/errors";
import type { ConnectionStatus } from "@/protocol/rpc";
import { type InitializeResult, PROTOCOL_VERSION, type SessionInfo } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { createQuickSession, openSession, renameSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { type DropEdge, moveSessionId, useSessionMeta } from "@/store/session-meta";
import { allSessions, groupWorkspaces, useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { attentionCount, useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { selectWorkflowBadges, useWorkflow, workflowRuns } from "@/store/workflow";
import { SEARCH_VISIBLE_SESSIONS, SessionPagingRow } from "./session-paging";

/** 每个工作区默认展示的会话数；更多的按段折叠在「More」后面。 */
const VISIBLE_SESSIONS = 6;
/** 侧栏内容匹配展示的会话数上限。 */
const SEARCH_CONTENT_ITEMS = 8;

const ROW = "flex h-9 w-full items-center gap-2.5 rounded-lg px-2.5 text-sm transition-colors";

/** 状态点提示：握手错误优先于通用状态文案，版本不兼容给出双端版本与动作。 */
function connectionTip(
  status: ConnectionStatus,
  handshakeError: RpcError | null,
  everOnline: boolean,
  server: InitializeResult | null,
): string {
  if (status === "online") {
    return t("sb.online", {
      name: server?.server.name ?? "moc-core",
      version: server?.server.version ?? "",
    });
  }
  if (handshakeError) {
    if (handshakeError.code === ErrorCode.UnsupportedProtocolVersion) {
      const serverVersion = (handshakeError.data as { server_version?: string } | undefined)
        ?.server_version;
      return t("conn.version_mismatch", {
        server: serverVersion ?? t("sb.unknown"),
        client: PROTOCOL_VERSION,
      });
    }
    return t("sb.handshake_failed", { error: handshakeError.message });
  }
  if (status === "connecting") return t("term.connecting");
  return everOnline ? t("sb.reconnecting") : t("sb.offline");
}

/** workflow 入口：待审批优先开审批窗，否则开 run 列表。 */
function WorkflowNavRow() {
  const t = useT();
  const badges = useWorkflow(useShallow(selectWorkflowBadges));
  const active = useTabs((state) => state.active);
  if (badges.approvals === 0 && badges.runs === 0) return null;
  return (
    <NavRow
      icon={<WorkflowIcon />}
      label={t("wfr.nav_label")}
      hint={
        <span className="text-ui text-muted-foreground">
          {badges.approvals > 0 ? t("sb.badge_approval", { n: badges.approvals }) : ""}
          {badges.runs > 0
            ? `${badges.approvals > 0 ? "·" : ""}${t("sb.badge_run", { n: badges.runs })}`
            : ""}
        </span>
      }
      onClick={() => {
        const wf = useWorkflow.getState();
        if (wf.approvals.length > 0) {
          useWorkflow.setState({ approvalOpen: true });
        } else {
          useWorkflow.setState({ runsOpen: true });
          if (active) {
            void workflowRuns(active)
              .then((runs) => useWorkflow.setState({ runList: runs }))
              .catch((error) => toast.error(t("slash.runs_failed", { error: String(error) })));
          }
        }
      }}
    />
  );
}

function WorkflowIcon() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5">
      <title>workflow</title>
      <circle cx="8" cy="4" r="2" />
      <circle cx="4" cy="12" r="2" />
      <circle cx="12" cy="12" r="2" />
      <path d="M8 6v2M8 8l-2.5 2M8 8l2.5 2" />
    </svg>
  );
}

function NavRow({
  icon,
  label,
  hint,
  active,
  onClick,
}: {
  icon: ReactNode;
  label: string;
  hint?: ReactNode;
  active?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active || undefined}
      className={cn(
        ROW,
        "group/nav text-sidebar-foreground hover:bg-sidebar-accent",
        active && "bg-sidebar-accent",
      )}
    >
      <span className="flex size-4 items-center justify-center text-muted-foreground group-hover/nav:text-foreground [&_svg]:size-[15px]">
        {icon}
      </span>
      <span className="flex-1 truncate text-left">{label}</span>
      {hint && <span className="shrink-0">{hint}</span>}
    </button>
  );
}

/** 侧栏底部的连接状态：绿点 + moc，悬停给出连接详情。 */
export function ConnectionBadge() {
  const status = useConnection((state) => state.status);
  const server = useConnection((state) => state.server);
  const handshakeError = useConnection((state) => state.handshakeError);
  const everOnline = useConnection((state) => state.everOnline);
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex h-7 items-center gap-2 text-sm font-medium text-sidebar-foreground">
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
          moc
        </span>
      </TooltipTrigger>
      <TooltipContent side="right" align="end" className="pointer-events-none">
        {connectionTip(status, handshakeError, everOnline, server)}
      </TooltipContent>
    </Tooltip>
  );
}

export function WorkspaceSidebar() {
  const t = useT();
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const workspaces = usePrefs((state) => state.workspaces);
  const orderByCwd = useSessionMeta((state) => state.orderByCwd);
  const active = useTabs((state) => state.active);

  const [query, setQuery] = useState("");
  const [searching, setSearching] = useState(false);
  const searchTrigger = useRef<HTMLButtonElement>(null);
  const [folded, setFolded] = useState<Record<string, boolean>>({});
  const [extraShown, setExtraShown] = useState<Record<string, number>>({});
  const groupHeaders = useRef<Record<string, HTMLDivElement | null>>({});

  type DragSource = { cwd: string; id: string };
  type DropTarget = { cwd: string; id: string; edge: DropEdge };
  const dragSourceRef = useRef<DragSource | null>(null);
  const dropTargetRef = useRef<DropTarget | null>(null);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<DropTarget | null>(null);

  const updateDropTarget = (next: DropTarget | null) => {
    const prev = dropTargetRef.current;
    if (prev?.cwd === next?.cwd && prev?.id === next?.id && prev?.edge === next?.edge) return;
    dropTargetRef.current = next;
    setDropTarget(next);
  };

  const clearDrag = () => {
    dragSourceRef.current = null;
    setDraggingId(null);
    updateDropTarget(null);
  };

  const commitDrop = (cwd: string, groupIds: readonly string[]) => {
    const source = dragSourceRef.current;
    const target = dropTargetRef.current;
    clearDrag();
    if (!source || !target || source.cwd !== cwd || target.cwd !== cwd) return;
    useSessionMeta.getState().reorder(cwd, groupIds, source.id, target.id, target.edge);
  };

  const groups = useMemo(
    () => groupWorkspaces(allSessions({ list, extra }), workspaces, query, orderByCwd),
    [list, extra, workspaces, query, orderByCwd],
  );

  // 正文搜索补充标题/目录匹配不到的会话；标题已命中的不重复展示。
  const { hits: contentHits } = useSessionSearch(query, SEARCH_CONTENT_ITEMS);
  const shownContent = useMemo(() => {
    if (!query.trim()) return [];
    const known = new Set(groups.flatMap((group) => group.sessions.map((s) => s.id)));
    return contentHits.filter((hit) => !known.has(hit.session.id));
  }, [contentHits, groups, query]);

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
    setExtraShown({});
    searchTrigger.current?.focus();
  };

  const collapseGroup = (cwd: string) => {
    setExtraShown((state) => {
      if (!state[cwd]) return state;
      const next = { ...state };
      delete next[cwd];
      return next;
    });
    requestAnimationFrame(() => {
      groupHeaders.current[cwd]?.scrollIntoView?.({ block: "nearest" });
    });
  };

  return (
    <nav className="flex h-full w-full min-w-0 flex-col overflow-hidden bg-sidebar text-sidebar-foreground select-none">
      <div className="flex h-12 shrink-0 items-center justify-between gap-2 pr-2.5 pl-5">
        <h2 className="truncate text-sm font-medium">{t("sb.sessions")}</h2>
        <div className="flex shrink-0 items-center gap-0.5">
          <IconButton
            ref={searchTrigger}
            label={t("common.search")}
            size="md"
            active={searching}
            aria-expanded={searching}
            onClick={() => (searching ? closeSearch() : setSearching(true))}
          >
            <SearchIcon />
          </IconButton>
        </div>
      </div>
      {searching && (
        <div className="shrink-0 px-2 pb-1">
          <div className={cn(ROW, "bg-sidebar-accent")}>
            <SearchIcon className="size-4 shrink-0 text-muted-foreground" />
            <input
              // biome-ignore lint/a11y/noAutofocus: 搜索行由用户点击展开，立即可输入
              autoFocus
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setExtraShown({});
              }}
              onKeyDown={(event) => event.key === "Escape" && closeSearch()}
              aria-label={t("sb.filter_ph")}
              placeholder={t("sb.filter_ph")}
              className="min-w-0 flex-1 bg-transparent text-sm outline-none"
            />
            <IconButton label={t("sb.filter_close")} size="xs" onClick={closeSearch}>
              <XIcon />
            </IconButton>
          </div>
        </div>
      )}

      {/* 原生滚动容器：Radix ScrollArea 的 table 布局会让 truncate 失效 */}
      <div className="min-h-0 flex-1 overflow-x-hidden overflow-y-auto px-2.5 pb-3">
        <div className="space-y-0.5">
          <NavRow
            icon={<SquarePenIcon />}
            label={t("hd.new")}
            active={active === "draft-session"}
            onClick={() => createQuickSession()}
          />
          <WorkflowNavRow />
        </div>
        <div className="mt-4 mb-0.5 flex h-8 items-center justify-between pl-2.5">
          <span className="text-xs text-text-tertiary">{t("sb.workspaces")}</span>
          <IconButton label={t("sb.add_workspace")} onClick={addWorkspace}>
            <FolderPlusIcon />
          </IconButton>
        </div>
        {groups.length === 0 && shownContent.length === 0 ? (
          <div className="px-2 py-8 text-center text-xs leading-5 text-text-tertiary">
            {query ? t("sb.no_match") : t("sb.empty")}
          </div>
        ) : (
          <>
            {shownContent.length > 0 && (
              <div className="mb-1">
                <div className="flex h-7 items-center px-2 text-xs font-medium text-text-tertiary">
                  {t("sb.content_hits")}
                </div>
                {shownContent.map((hit) => (
                  <button
                    key={hit.session.id}
                    type="button"
                    onClick={() => void openSession(hit.session.id).catch(() => undefined)}
                    className={cn(
                      ROW,
                      "h-auto flex-col items-start gap-0 py-2 pl-[30px] text-sidebar-foreground/75 hover:bg-sidebar-accent/70 hover:text-sidebar-foreground",
                    )}
                  >
                    <span className="w-full truncate">{hit.session.title || t("hd.untitled")}</span>
                    <span className="w-full truncate text-xs text-text-tertiary">
                      {hit.matches[0]?.snippet}
                    </span>
                  </button>
                ))}
              </div>
            )}
            {groups.map((group) => {
              const isFolded = Boolean(folded[group.cwd]);
              const base = query.trim() ? SEARCH_VISIBLE_SESSIONS : VISIBLE_SESSIONS;
              const extraCount = extraShown[group.cwd] ?? 0;
              const visible = group.sessions.slice(0, base + extraCount);
              const hidden = group.sessions.length - visible.length;
              const canCollapse = !isFolded && extraCount > 0 && visible.length > base;
              const canDrag = !query.trim() && group.sessions.length > 1;
              const groupIds = group.sessions.map((item) => item.id);
              const hasCustomOrder = Boolean(orderByCwd[group.cwd]?.length);
              return (
                <div key={group.cwd} className="mb-2">
                  <div
                    ref={(node) => {
                      groupHeaders.current[group.cwd] = node;
                    }}
                    className="sticky top-0 z-10 bg-sidebar"
                  >
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
                      <div className="flex shrink-0 items-center">
                        <div className="flex shrink-0 items-center opacity-0 transition-opacity group-hover/ws:opacity-100 group-focus-within/ws:opacity-100 has-[[data-state=open]]:opacity-100 pointer-coarse:opacity-100">
                          <IconButton
                            label={t("sb.new_here")}
                            size="xs"
                            onClick={() => createQuickSession(group.cwd)}
                          >
                            <PlusIcon />
                          </IconButton>
                          <WorkspaceMenu
                            cwd={group.cwd}
                            removable={workspaces.includes(group.cwd)}
                            sessionCount={group.sessions.length}
                            hasCustomOrder={hasCustomOrder}
                          />
                        </div>
                        {canCollapse && (
                          <IconButton
                            label={t("th.collapse")}
                            size="xs"
                            onClick={() => collapseGroup(group.cwd)}
                          >
                            <ChevronsUpIcon />
                          </IconButton>
                        )}
                      </div>
                    </div>
                  </div>

                  {!isFolded && (
                    /* biome-ignore lint/a11y/noStaticElementInteractions: 容器仅承接会话行间隙的原生拖放落点 */
                    <div
                      className="mt-0.5 space-y-0.5"
                      onDragOver={(event) => {
                        if (dragSourceRef.current?.cwd !== group.cwd) return;
                        event.preventDefault();
                        if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
                      }}
                      onDragLeave={(event) => {
                        if (
                          !event.currentTarget.contains(event.relatedTarget as Node | null) &&
                          dropTargetRef.current?.cwd === group.cwd
                        ) {
                          updateDropTarget(null);
                        }
                      }}
                      onDrop={(event) => {
                        if (dragSourceRef.current?.cwd !== group.cwd) return;
                        event.preventDefault();
                        commitDrop(group.cwd, groupIds);
                      }}
                    >
                      {visible.map((session) => (
                        <SessionRow
                          key={session.id}
                          session={session}
                          active={session.id === active}
                          draggable={canDrag}
                          dragging={draggingId === session.id}
                          dropEdge={
                            dropTarget?.cwd === group.cwd && dropTarget.id === session.id
                              ? dropTarget.edge
                              : null
                          }
                          onDragStart={(event) => {
                            if (!canDrag) {
                              event.preventDefault();
                              return;
                            }
                            if (
                              event.target instanceof Element &&
                              event.target.closest("[data-session-menu]")
                            ) {
                              event.preventDefault();
                              return;
                            }
                            dragSourceRef.current = { cwd: group.cwd, id: session.id };
                            setDraggingId(session.id);
                            if (event.dataTransfer) {
                              event.dataTransfer.effectAllowed = "move";
                              event.dataTransfer.setData("application/x-moc-session", session.id);
                              const rect = event.currentTarget.getBoundingClientRect();
                              event.dataTransfer.setDragImage?.(
                                event.currentTarget,
                                event.clientX - rect.left,
                                event.clientY - rect.top,
                              );
                            }
                          }}
                          onDragOver={(event) => {
                            const source = dragSourceRef.current;
                            if (!source || source.cwd !== group.cwd) return;
                            event.preventDefault();
                            event.stopPropagation();
                            if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
                            if (source.id === session.id) {
                              updateDropTarget(null);
                              return;
                            }
                            const rect = event.currentTarget.getBoundingClientRect();
                            const edge: DropEdge =
                              event.clientY < rect.top + rect.height / 2 ? "before" : "after";
                            if (moveSessionId(groupIds, source.id, session.id, edge)) {
                              updateDropTarget({ cwd: group.cwd, id: session.id, edge });
                            } else {
                              updateDropTarget(null);
                            }
                          }}
                          onDrop={(event) => {
                            if (dragSourceRef.current?.cwd !== group.cwd) return;
                            event.preventDefault();
                            event.stopPropagation();
                            commitDrop(group.cwd, groupIds);
                          }}
                          onDragEnd={clearDrag}
                        />
                      ))}
                      <SessionPagingRow
                        hidden={hidden}
                        canCollapse={canCollapse}
                        onMore={(step) =>
                          setExtraShown((state) => ({
                            ...state,
                            [group.cwd]: (state[group.cwd] ?? 0) + step,
                          }))
                        }
                        onCollapse={() => collapseGroup(group.cwd)}
                        className="pr-2.5 pl-[30px]"
                      />
                      {group.sessions.length === 0 && (
                        <div className="py-1.5 pl-[30px] text-ui text-text-tertiary">
                          {t("sb.no_sessions")}
                        </div>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
          </>
        )}
      </div>

      <div className="flex h-12 shrink-0 items-center pr-2.5 pl-5">
        <ConnectionBadge />
      </div>
    </nav>
  );
}

function WorkspaceMenu({
  cwd,
  removable,
  sessionCount,
  hasCustomOrder,
}: {
  cwd: string;
  removable: boolean;
  sessionCount: number;
  hasCustomOrder: boolean;
}) {
  const t = useT();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          aria-label={t("sb.ws_actions")}
          className="flex size-6 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <MoreHorizontalIcon className="size-3.5" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-48">
        <DropdownMenuItem
          onSelect={() => {
            void navigator.clipboard.writeText(cwd);
            toast.success(t("sb.path_copied"));
          }}
        >
          <CopyIcon />
          {t("sb.copy_path")}
        </DropdownMenuItem>
        {hasCustomOrder && (
          <DropdownMenuItem onSelect={() => useSessionMeta.getState().resetOrder(cwd)}>
            <RotateCcwIcon />
            {t("sb.reset_order")}
          </DropdownMenuItem>
        )}
        {sessionCount > 0 ? (
          <DropdownMenuItem
            variant="destructive"
            onSelect={() => useUi.getState().set({ confirmDeleteWorkspace: { cwd } })}
          >
            <Trash2Icon />
            {t("sb.delete_ws")}
          </DropdownMenuItem>
        ) : (
          removable && (
            <DropdownMenuItem
              variant="destructive"
              onSelect={() => usePrefs.getState().removeWorkspace(cwd)}
            >
              <XIcon />
              {t("sb.remove_ws")}
            </DropdownMenuItem>
          )
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** 会话行菜单项：「…」下拉与右键菜单共用一份。 */
type SessionAction = {
  key: string;
  icon: ReactNode;
  label: string;
  onSelect: () => void;
  destructive?: boolean;
  separated?: boolean;
};

function SessionRow({
  session,
  active,
  draggable,
  dragging,
  dropEdge,
  onDragStart,
  onDragOver,
  onDrop,
  onDragEnd,
}: {
  session: SessionInfo;
  active: boolean;
  draggable: boolean;
  dragging: boolean;
  dropEdge: DropEdge | null;
  onDragStart: (event: DragEvent<HTMLDivElement>) => void;
  onDragOver: (event: DragEvent<HTMLDivElement>) => void;
  onDrop: (event: DragEvent<HTMLDivElement>) => void;
  onDragEnd: () => void;
}) {
  const t = useT();
  const running = useTranscript((state) => Boolean(state.activity[session.id]?.running));
  const attention = useTranscript((state) => attentionCount(state, session.id) > 0);
  const unread = useSessionMeta((state) => Boolean(state.unread[session.id]));
  const [renaming, setRenaming] = useState<string | null>(null);
  const renamingFromMenu = useRef(false);
  const sessionButton = useRef<HTMLButtonElement>(null);
  const title = session.title || t("hd.untitled");

  const commitRename = () => {
    const next = renaming?.trim();
    setRenaming(null);
    if (next && next !== title) {
      renameSession(session.id, next).catch((error: unknown) =>
        toast.error(
          t("hd.rename_failed", {
            error: error instanceof Error ? error.message : String(error),
          }),
        ),
      );
    }
  };

  const actions: SessionAction[] = [
    {
      key: "open",
      icon: <ArrowUpRightIcon />,
      label: t("sb.open"),
      onSelect: () => void openSession(session.id).catch(() => undefined),
    },
    {
      key: "rename",
      icon: <PencilIcon />,
      label: t("hd.rename"),
      onSelect: () => {
        renamingFromMenu.current = true;
      },
    },
    {
      key: "copy",
      icon: <CopyIcon />,
      label: t("hd.copy_id"),
      onSelect: () => {
        void navigator.clipboard.writeText(session.id);
        toast.success(t("hd.id_copied"));
      },
    },
    {
      key: "delete",
      icon: <Trash2Icon />,
      label: t("common.delete"),
      destructive: true,
      separated: true,
      onSelect: () => useUi.getState().set({ confirmDelete: { id: session.id, title } }),
    },
  ];
  // 等菜单关闭再挂载输入框，避免菜单的焦点管理抢回焦点。
  const onMenuCloseAutoFocus = (event: Event) => {
    if (!renamingFromMenu.current) return;
    event.preventDefault();
    renamingFromMenu.current = false;
    setRenaming(title);
  };

  if (renaming !== null) {
    return (
      <div className={cn(ROW, "h-11 bg-sidebar-accent pl-[30px]")}>
        <input
          // biome-ignore lint/a11y/noAutofocus: 进入重命名后立即编辑
          autoFocus
          aria-label={t("hd.rename")}
          value={renaming}
          onChange={(event) => setRenaming(event.target.value)}
          onFocus={(event) => event.currentTarget.select()}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === "Enter") commitRename();
            else if (event.key === "Escape") setRenaming(null);
            else return;
            event.preventDefault();
            requestAnimationFrame(() => sessionButton.current?.focus());
          }}
          onBlur={commitRename}
          className="min-w-0 flex-1 bg-transparent text-ui outline-none"
        />
      </div>
    );
  }

  const status = running
    ? t("sb.running")
    : attention
      ? t("sb.attention")
      : relativeTime(session.updated_at_ms);

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        {/* biome-ignore lint/a11y/noStaticElementInteractions: 行内按钮负责点击与键盘焦点，外层仅承接原生拖放排序 */}
        <div
          draggable={draggable}
          onDragStart={onDragStart}
          onDragOver={onDragOver}
          onDrop={onDrop}
          onDragEnd={onDragEnd}
          className={cn(
            "group/session relative flex h-11 w-full items-center gap-2.5 rounded-lg pr-1.5 pl-3 transition-colors data-[state=open]:bg-sidebar-accent has-[[data-state=open]]:bg-sidebar-accent",
            active ? "bg-sidebar-accent" : "hover:bg-sidebar-accent/70",
            dragging && "opacity-45",
          )}
        >
          {dropEdge && (
            <span
              aria-hidden="true"
              className={cn(
                "pointer-events-none absolute inset-x-2 z-20 h-0.5 rounded-full bg-brand",
                dropEdge === "before" ? "-top-px" : "-bottom-px",
              )}
            />
          )}
          <button
            ref={sessionButton}
            type="button"
            draggable={draggable}
            onClick={() => void openSession(session.id).catch(() => undefined)}
            onDoubleClick={() => setRenaming(title)}
            className="absolute inset-0 rounded-lg"
            aria-label={title}
            title={title}
            aria-current={active || undefined}
          />
          <span
            aria-hidden="true"
            className="pointer-events-none flex size-2.5 shrink-0 items-center justify-center self-start mt-[9px]"
          >
            {running ? (
              <span className="size-2.5 rounded-full border border-text-tertiary/40 border-t-foreground motion-safe:animate-spin" />
            ) : (
              <span
                data-indicator={attention ? "attention" : unread ? "unread" : "idle"}
                className={cn(
                  "rounded-full",
                  attention
                    ? "size-1.5 bg-warn"
                    : unread
                      ? "size-1.5 bg-brand"
                      : "size-1 bg-text-tertiary/50",
                )}
              />
            )}
          </span>
          <span className="pointer-events-none flex min-w-0 flex-1 flex-col group-hover/session:pr-7 group-focus-within/session:pr-7 group-has-[[data-state=open]]/session:pr-7 pointer-coarse:pr-7">
            <span className="truncate text-ui text-sidebar-foreground">{title}</span>
            <span
              className={cn(
                "truncate text-xs tabular-nums",
                attention && !running
                  ? "text-warn"
                  : active
                    ? "text-muted-foreground"
                    : "text-text-tertiary group-hover/session:text-muted-foreground group-focus-within/session:text-muted-foreground group-has-[[data-state=open]]/session:text-muted-foreground group-data-[state=open]/session:text-muted-foreground",
              )}
            >
              {status}
            </span>
          </span>
          <div
            data-session-menu
            className="pointer-events-none absolute top-0.5 right-1.5 opacity-0 transition-opacity group-hover/session:pointer-events-auto group-hover/session:opacity-100 group-focus-within/session:pointer-events-auto group-focus-within/session:opacity-100 group-has-[[data-state=open]]/session:pointer-events-auto group-has-[[data-state=open]]/session:opacity-100 pointer-coarse:pointer-events-auto pointer-coarse:opacity-100"
          >
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button
                  type="button"
                  draggable={false}
                  aria-label={t("sb.session_actions")}
                  className="flex size-6 items-center justify-center text-muted-foreground transition-colors outline-none hover:text-foreground focus-visible:text-foreground focus-visible:outline-none data-[state=open]:text-foreground"
                >
                  <MoreHorizontalIcon className="size-4" />
                </button>
              </DropdownMenuTrigger>
              <DropdownMenuContent
                align="start"
                className="w-48"
                onCloseAutoFocus={onMenuCloseAutoFocus}
              >
                {actions.map((action) => (
                  <Fragment key={action.key}>
                    {action.separated && <DropdownMenuSeparator />}
                    <DropdownMenuItem
                      variant={action.destructive ? "destructive" : "default"}
                      onSelect={action.onSelect}
                    >
                      {action.icon}
                      {action.label}
                    </DropdownMenuItem>
                  </Fragment>
                ))}
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent className="w-48" onCloseAutoFocus={onMenuCloseAutoFocus}>
        {actions.map((action) => (
          <Fragment key={action.key}>
            {action.separated && <ContextMenuSeparator />}
            <ContextMenuItem
              variant={action.destructive ? "destructive" : "default"}
              onSelect={action.onSelect}
            >
              {action.icon}
              {action.label}
            </ContextMenuItem>
          </Fragment>
        ))}
      </ContextMenuContent>
    </ContextMenu>
  );
}
