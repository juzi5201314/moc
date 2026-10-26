// 左侧栏：品牌 + 新建 + 搜索、收件箱、工作区 → 会话树、底部工具条。

import {
  ChevronDown,
  ChevronRight,
  FolderPlus,
  GitFork,
  Inbox,
  Monitor,
  Moon,
  MoreHorizontal,
  Pin,
  PinOff,
  Plus,
  Search,
  Settings,
  Sun,
} from "lucide-react";
import { useMemo, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { SessionStatusIcon } from "@/components/common/session-status";
import { Button } from "@/components/ui/button";
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
import { Input } from "@/components/ui/input";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { SessionInfo } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { openSession, releaseSession } from "@/store/lifecycle";
import { type Theme, usePrefs } from "@/store/prefs";
import {
  allSessions,
  childrenByParent,
  groupWorkspaces,
  useSessions,
  type Workspace,
  workspaceName,
} from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { attentionCount, useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { useSessionActions } from "./session-actions";

const THEME_NEXT: Record<Theme, Theme> = { system: "light", light: "dark", dark: "system" };
const THEME_LABEL: Record<Theme, string> = { system: "跟随系统", light: "亮色", dark: "暗色" };

export function Sidebar() {
  const [query, setQuery] = useState("");
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const pinned = useSessions((state) => state.pinned);
  const sessions = useMemo(() => allSessions({ list, extra }), [list, extra]);
  const workspaces = useMemo(
    () => groupWorkspaces(sessions, pinned, query),
    [sessions, pinned, query],
  );
  const children = useMemo(() => childrenByParent(sessions), [sessions]);
  const setUi = useUi((state) => state.set);

  return (
    <aside className="flex h-full flex-col bg-panel" aria-label="会话侧栏">
      <div className="flex items-center gap-2 px-3 pt-3 pb-2">
        <span className="font-semibold text-base tracking-tight">moc</span>
        <div className="flex-1" />
        <IconButton
          label="添加工作区"
          onClick={() =>
            setUi({
              folderPicker: {
                open: true,
                onPick: (path) => useSessions.getState().pin(path),
              },
            })
          }
        >
          <FolderPlus />
        </IconButton>
      </div>
      <div className="px-3">
        <Button
          className="w-full justify-start gap-2"
          onClick={() => setUi({ newSession: { open: true } })}
        >
          <Plus />
          新建会话
          <span className="ml-auto text-xs opacity-60">Alt+N</span>
        </Button>
        <div className="relative mt-2">
          <Search
            className="-translate-y-1/2 pointer-events-none absolute top-1/2 left-2.5 size-3.5 text-muted-foreground"
            aria-hidden
          />
          <Input
            id="sidebar-search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索会话或路径"
            aria-label="搜索会话"
            className="h-8 bg-background pl-8"
          />
        </div>
      </div>
      <InboxSection />
      <nav className="mt-2 min-h-0 flex-1 overflow-y-auto px-2 pb-2" aria-label="工作区">
        {workspaces.length === 0 ? (
          <p className="px-2 py-6 text-center text-muted-foreground text-xs">
            {query ? "没有匹配的会话" : "还没有会话，新建一个开始吧"}
          </p>
        ) : (
          workspaces.map((workspace) => (
            <WorkspaceGroup key={workspace.cwd} workspace={workspace} childMap={children} />
          ))
        )}
      </nav>
      <SidebarFooter />
    </aside>
  );
}

function InboxSection() {
  const sessionsById = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const items = useTranscript((state) => {
    const ids = new Set([
      ...Object.keys(state.permissions),
      ...Object.keys(state.asks),
      ...Object.keys(state.bySession),
    ]);
    const result: { id: string; count: number }[] = [];
    for (const id of ids) {
      const count = attentionCount(state, id);
      if (count > 0) result.push({ id, count });
    }
    return JSON.stringify(result);
  });
  const parsed = JSON.parse(items) as { id: string; count: number }[];
  if (parsed.length === 0) return null;
  const titleOf = (id: string) =>
    sessionsById.find((item) => item.id === id)?.title ??
    extra[id]?.title ??
    useTranscript.getState().bySession[id]?.info.title ??
    id;
  return (
    <section
      className="mx-3 mt-3 rounded-lg border border-warn/40 bg-warn/5 p-1.5"
      aria-label="收件箱"
    >
      <div className="flex items-center gap-1.5 px-1.5 pb-1 font-medium text-warn text-xs">
        <Inbox className="size-3.5" aria-hidden />
        需要处理
      </div>
      {parsed.map((item) => (
        <button
          type="button"
          key={item.id}
          onClick={() => void openSession(item.id)}
          className="flex w-full items-center gap-2 rounded-md px-1.5 py-1 text-left text-sm hover:bg-accent"
        >
          <span className="min-w-0 flex-1 truncate">{titleOf(item.id) || "未命名会话"}</span>
          <span className="rounded-full bg-warn/20 px-1.5 text-warn text-xs tabular-nums">
            {item.count}
          </span>
        </button>
      ))}
    </section>
  );
}

function WorkspaceGroup({
  workspace,
  childMap,
}: {
  workspace: Workspace;
  childMap: Record<string, SessionInfo[]>;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const setUi = useUi((state) => state.set);
  const { pin, unpin } = useSessions.getState();
  const name = workspaceName(workspace.cwd);
  return (
    <div className="mb-1">
      <div className="group flex items-center gap-1 rounded-md px-1 py-1 hover:bg-accent/60">
        <button
          type="button"
          onClick={() => setCollapsed((value) => !value)}
          aria-expanded={!collapsed}
          title={workspace.cwd}
          className="flex min-w-0 flex-1 items-center gap-1 text-left font-medium text-muted-foreground text-xs"
        >
          {collapsed ? (
            <ChevronRight className="size-3.5 shrink-0" aria-hidden />
          ) : (
            <ChevronDown className="size-3.5 shrink-0" aria-hidden />
          )}
          <span className="truncate">{name}</span>
          {workspace.pinned ? (
            <Pin className="size-3 shrink-0 opacity-60" aria-label="已固定" />
          ) : null}
        </button>
        <div className="flex opacity-0 transition-opacity focus-within:opacity-100 group-hover:opacity-100">
          <IconButton
            label={`在 ${name} 新建会话`}
            size="icon-xs"
            onClick={() => setUi({ newSession: { open: true, cwd: workspace.cwd } })}
          >
            <Plus />
          </IconButton>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon-xs" aria-label={`${name} 菜单`}>
                <MoreHorizontal />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem
                onSelect={() => setUi({ newSession: { open: true, cwd: workspace.cwd } })}
              >
                <Plus />
                在此新建会话
              </DropdownMenuItem>
              <DropdownMenuItem
                onSelect={() => (workspace.pinned ? unpin(workspace.cwd) : pin(workspace.cwd))}
              >
                {workspace.pinned ? <PinOff /> : <Pin />}
                {workspace.pinned ? "取消固定" : "固定工作区"}
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>
      {collapsed ? null : workspace.sessions.length === 0 ? (
        <p className="py-1 pl-6 text-muted-foreground text-xs">暂无会话</p>
      ) : (
        <ul className="space-y-px">
          {workspace.sessions.map((session) => (
            <li key={session.id}>
              <SessionRow session={session} depth={0} />
              {childMap[session.id]?.map((child) => (
                <SessionRow key={child.id} session={child} depth={1} />
              ))}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function SessionRow({ session, depth }: { session: SessionInfo; depth: number }) {
  const active = useTabs((state) => state.active === session.id);
  const loaded = useTranscript((state) => Boolean(state.bySession[session.id]));
  const actions = useSessionActions();
  const holder = session.lock;
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <div
          className={cn(
            "group flex items-center gap-1 rounded-md pr-1 text-sm",
            active ? "bg-accent text-accent-foreground" : "hover:bg-accent/60",
          )}
          style={{ paddingLeft: depth ? 28 : 20 }}
        >
          <button
            type="button"
            onClick={() => void openSession(session.id)}
            onDoubleClick={() => actions.rename(session)}
            aria-current={active ? "page" : undefined}
            className="flex min-w-0 flex-1 items-center gap-1.5 py-1.5 text-left"
          >
            {session.forked_from ? (
              <GitFork className="size-3 shrink-0 text-muted-foreground" aria-label="分叉会话" />
            ) : null}
            <span className="min-w-0 flex-1 truncate">{session.title || "未命名会话"}</span>
            <SessionStatusIcon sessionId={session.id} />
            {holder && !loaded ? (
              <span className="shrink-0 text-muted-foreground text-xs" title={`pid ${holder.pid}`}>
                {holder.frontend}
              </span>
            ) : null}
            <span className="shrink-0 text-muted-foreground text-xs group-hover:hidden">
              {relativeTime(session.updated_at_ms)}
            </span>
          </button>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon-xs"
                aria-label={`${session.title || "会话"} 菜单`}
                className="hidden group-hover:inline-flex data-[state=open]:inline-flex"
              >
                <MoreHorizontal />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={() => void openSession(session.id)}>
                打开
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => actions.rename(session)}>重命名</DropdownMenuItem>
              <DropdownMenuItem
                disabled={!loaded}
                onSelect={() => void actions.run(() => releaseSession(session.id), "已释放写权限")}
              >
                释放
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem variant="destructive" onSelect={() => actions.remove(session)}>
                删除
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onSelect={() => void openSession(session.id)}>打开</ContextMenuItem>
        <ContextMenuItem onSelect={() => actions.rename(session)}>重命名</ContextMenuItem>
        <ContextMenuItem
          disabled={!loaded}
          onSelect={() => void actions.run(() => releaseSession(session.id), "已释放写权限")}
        >
          释放
        </ContextMenuItem>
        <ContextMenuSeparator />
        <ContextMenuItem variant="destructive" onSelect={() => actions.remove(session)}>
          删除
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}

function SidebarFooter() {
  const theme = usePrefs((state) => state.theme);
  const setPrefs = usePrefs((state) => state.set);
  const status = useConnection((state) => state.status);
  const setUi = useUi((state) => state.set);
  const ThemeIcon = theme === "system" ? Monitor : theme === "light" ? Sun : Moon;
  const statusLabel =
    status === "online" ? "已连接" : status === "connecting" ? "连接中" : "未连接";
  return (
    <div className="flex items-center gap-1 border-t px-2 py-2">
      <IconButton label="设置" side="top" onClick={() => setUi({ settings: { open: true } })}>
        <Settings />
      </IconButton>
      <IconButton
        label={`主题：${THEME_LABEL[theme]}`}
        side="top"
        onClick={() => setPrefs({ theme: THEME_NEXT[theme] })}
      >
        <ThemeIcon />
      </IconButton>
      <div className="flex-1" />
      <output
        className="flex items-center gap-1.5 text-muted-foreground text-xs"
        aria-live="polite"
      >
        <span
          className={cn(
            "size-2 rounded-full",
            status === "online"
              ? "bg-success"
              : status === "connecting"
                ? "animate-pulse bg-warn"
                : "bg-destructive",
          )}
          aria-hidden
        />
        {statusLabel}
      </output>
    </div>
  );
}
