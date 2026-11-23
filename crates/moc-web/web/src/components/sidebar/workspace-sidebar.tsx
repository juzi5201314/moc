// 左侧会话栏：搜索、新会话、按目录分组的会话树，以及底部全局设置。

import {
  ArrowUpRightIcon,
  ChevronRightIcon,
  CopyIcon,
  FolderPlusIcon,
  MoreHorizontalIcon,
  PencilIcon,
  PlusIcon,
  SearchIcon,
  SquarePenIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { Fragment, type ReactNode, useMemo, useRef, useState } from "react";
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
import { allSessions, groupWorkspaces, useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { attentionCount, useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { selectWorkflowBadges, useWorkflow, workflowRuns } from "@/store/workflow";

/** 每个工作区默认展示的会话数；更多的折叠在「显示更多」后面。 */
const VISIBLE_SESSIONS = 6;
/** 搜索时每个工作区直接展开的匹配数上限；其余仍走「显示更多」。 */
const SEARCH_VISIBLE_SESSIONS = 20;
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
      label="workflow"
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
      <TooltipContent side="top">
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
  const draft = useSessions((state) => state.draftSession);
  const active = useTabs((state) => state.active);

  const [query, setQuery] = useState("");
  const [searching, setSearching] = useState(false);
  const searchTrigger = useRef<HTMLButtonElement>(null);
  const [folded, setFolded] = useState<Record<string, boolean>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const groups = useMemo(() => {
    const dirs = draft?.cwd ? [...workspaces, draft.cwd] : workspaces;
    return groupWorkspaces(allSessions({ list, extra }), dirs, query);
  }, [list, extra, workspaces, draft?.cwd, query]);

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
    searchTrigger.current?.focus();
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
              onChange={(event) => setQuery(event.target.value)}
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
                    onClick={() => void openSession(hit.session.id)}
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
              const expandedGroup = Boolean(expanded[group.cwd]);
              // 搜索时先截到每组 SEARCH_VISIBLE_SESSIONS 条，避免一次查询挂满整棵树。
              const cap = expandedGroup
                ? Number.POSITIVE_INFINITY
                : query
                  ? SEARCH_VISIBLE_SESSIONS
                  : VISIBLE_SESSIONS;
              const visible = group.sessions.slice(0, cap);
              const hidden = group.sessions.length - visible.length;
              const hasDraft = draft?.cwd === group.cwd;
              return (
                <div key={group.cwd} className="mb-2">
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
                    <div className="flex shrink-0 items-center opacity-0 transition-opacity group-hover/ws:opacity-100 group-focus-within/ws:opacity-100 has-[[data-state=open]]:opacity-100 pointer-coarse:opacity-100">
                      <IconButton
                        label={t("sb.new_here")}
                        size="xs"
                        onClick={() => createQuickSession(group.cwd)}
                      >
                        <PlusIcon />
                      </IconButton>
                      <WorkspaceMenu cwd={group.cwd} removable={workspaces.includes(group.cwd)} />
                    </div>
                  </div>

                  {!isFolded && (
                    <div className="mt-0.5 space-y-0.5">
                      {hasDraft && (
                        <button
                          type="button"
                          onClick={() => useTabs.getState().add("draft-session")}
                          className={cn(
                            ROW,
                            "pl-[30px]",
                            active === "draft-session"
                              ? "bg-sidebar-accent text-sidebar-foreground"
                              : "text-muted-foreground hover:bg-sidebar-accent",
                          )}
                        >
                          <span className="truncate italic">{t("hd.new")}</span>
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
                            "pl-[30px] text-ui text-text-tertiary hover:text-foreground",
                          )}
                        >
                          {t("sb.more", { n: hidden })}
                        </button>
                      )}
                      {expanded[group.cwd] &&
                        !query &&
                        group.sessions.length > VISIBLE_SESSIONS && (
                          <button
                            type="button"
                            onClick={() =>
                              setExpanded((state) => ({ ...state, [group.cwd]: false }))
                            }
                            className={cn(
                              ROW,
                              "pl-[30px] text-ui text-text-tertiary hover:text-foreground",
                            )}
                          >
                            {t("th.collapse")}
                          </button>
                        )}
                      {!hasDraft && group.sessions.length === 0 && (
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

function WorkspaceMenu({ cwd, removable }: { cwd: string; removable: boolean }) {
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
        {removable && (
          <DropdownMenuItem
            variant="destructive"
            onSelect={() => usePrefs.getState().removeWorkspace(cwd)}
          >
            <XIcon />
            {t("sb.remove_ws")}
          </DropdownMenuItem>
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

function SessionRow({ session, active }: { session: SessionInfo; active: boolean }) {
  const t = useT();
  const running = useTranscript((state) => Boolean(state.activity[session.id]?.running));
  const attention = useTranscript((state) => attentionCount(state, session.id) > 0);
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
      onSelect: () => void openSession(session.id),
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
      <div className={cn(ROW, "h-12 bg-sidebar-accent pl-[30px]")}>
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
          className="min-w-0 flex-1 bg-transparent text-sm outline-none"
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
        <div
          className={cn(
            "group/session relative flex h-12 w-full items-center gap-2.5 rounded-lg pr-1.5 pl-3 transition-colors data-[state=open]:bg-sidebar-accent has-[[data-state=open]]:bg-sidebar-accent",
            active ? "bg-sidebar-accent" : "hover:bg-sidebar-accent/70",
          )}
        >
          <button
            ref={sessionButton}
            type="button"
            onClick={() => void openSession(session.id)}
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
                className={cn(
                  "rounded-full",
                  attention ? "size-1.5 bg-warn" : "size-1 bg-text-tertiary/50",
                )}
              />
            )}
          </span>
          <span className="pointer-events-none flex min-w-0 flex-1 flex-col group-hover/session:pr-7 group-focus-within/session:pr-7 group-has-[[data-state=open]]/session:pr-7 pointer-coarse:pr-7">
            <span className="truncate text-sm leading-5 text-sidebar-foreground">{title}</span>
            <span
              className={cn(
                "truncate text-ui leading-[18px] tabular-nums",
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
          <div className="pointer-events-none absolute top-1.5 right-1.5 opacity-0 transition-opacity group-hover/session:pointer-events-auto group-hover/session:opacity-100 group-focus-within/session:pointer-events-auto group-focus-within/session:opacity-100 group-has-[[data-state=open]]/session:pointer-events-auto group-has-[[data-state=open]]/session:opacity-100 pointer-coarse:pointer-events-auto pointer-coarse:opacity-100">
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <button
                  type="button"
                  aria-label={t("sb.session_actions")}
                  className="flex size-6 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
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
