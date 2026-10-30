// 会话顶栏：标题（双击/菜单重命名）+ 目录与分支面包屑 + 只读状态 + 检查面板与更多操作。

import {
  CopyIcon,
  FolderIcon,
  GitBranchIcon,
  LockIcon,
  MinimizeIcon,
  MoreHorizontalIcon,
  PanelLeftOpenIcon,
  PanelRightIcon,
  PencilIcon,
  SquarePenIcon,
  Trash2Icon,
  UnlockIcon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { IconButton } from "@/components/common/icon-button";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useGitBranch } from "@/lib/use-git-branch";
import { useNarrow } from "@/lib/use-narrow";
import { useLayout } from "@/store/layout";
import {
  compactSession,
  createQuickSession,
  reacquire,
  releaseSession,
  renameSession,
} from "@/store/lifecycle";
import { workspaceName } from "@/store/sessions";
import { attentionCount, useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 侧栏收起时，顶栏左侧补上展开与新建入口。 */
export function SidebarReveal() {
  const collapsed = useLayout((state) => state.sidebarCollapsed);
  const overlay = useLayout((state) => state.sidebarOverlay);
  const narrow = useNarrow();
  if (!(narrow ? !overlay : collapsed)) return null;
  return (
    <div className="flex items-center gap-0.5">
      <IconButton
        label="展开侧栏"
        shortcut="Ctrl B"
        onClick={() => useLayout.getState().toggleSidebar()}
      >
        <PanelLeftOpenIcon />
      </IconButton>
      <IconButton label="新会话" shortcut="Alt N" onClick={() => createQuickSession()}>
        <SquarePenIcon />
      </IconButton>
    </div>
  );
}

export function InspectorToggle({ sessionId }: { sessionId: string }) {
  const open = useLayout((state) => state.rightbarOpen);
  const pending = useTranscript((state) => attentionCount(state, sessionId));
  return (
    <div className="relative">
      <IconButton
        label={open ? "收起检查面板" : "打开检查面板"}
        shortcut="Alt R"
        active={open}
        onClick={() => useLayout.getState().toggleRightbar()}
      >
        <PanelRightIcon />
      </IconButton>
      {pending > 0 && !open && (
        <span className="pointer-events-none absolute top-1 right-1 size-1.5 rounded-full bg-warn" />
      )}
    </div>
  );
}

export function SessionHeader({ sessionId }: { sessionId: string }) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const branch = useGitBranch(view?.info.cwd);
  const [editing, setEditing] = useState<string | null>(null);
  const [compacting, setCompacting] = useState(false);

  if (!view) return null;
  const title = view.info.title || "未命名会话";
  const isDraft = sessionId === "draft-session";

  const commitTitle = () => {
    const next = editing?.trim();
    setEditing(null);
    if (next && next !== title && !isDraft) {
      renameSession(sessionId, next).catch((error) =>
        toast.error(`重命名失败：${errorText(error)}`),
      );
    }
  };

  const compact = async () => {
    setCompacting(true);
    try {
      const result = await compactSession(sessionId);
      toast.success(
        `上下文已压缩：${result.tokens_before.toLocaleString()} → ${result.tokens_after.toLocaleString()} tokens`,
      );
    } catch (error) {
      toast.error(`压缩失败：${errorText(error)}`);
    } finally {
      setCompacting(false);
    }
  };

  return (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border-subtle px-3">
      <SidebarReveal />

      <div className="flex min-w-0 flex-1 items-center gap-2.5">
        {editing !== null ? (
          <input
            // biome-ignore lint/a11y/noAutofocus: 进入重命名后立即编辑
            autoFocus
            value={editing}
            onChange={(event) => setEditing(event.target.value)}
            onFocus={(event) => event.currentTarget.select()}
            onKeyDown={(event) => {
              if (event.key === "Enter") commitTitle();
              if (event.key === "Escape") setEditing(null);
            }}
            onBlur={commitTitle}
            className="h-7 w-[min(360px,50%)] rounded-md bg-accent px-2 text-sm font-medium outline-none ring-1 ring-ring"
          />
        ) : (
          <button
            type="button"
            onDoubleClick={() => !isDraft && setEditing(title)}
            className="min-w-0 truncate rounded-md px-1 text-[15px] font-medium text-foreground"
            title={isDraft ? title : `${title}（双击重命名）`}
          >
            {title}
          </button>
        )}

        <div className="hidden min-w-0 items-center gap-2.5 text-[12.5px] text-text-tertiary md:flex">
          <span className="flex min-w-0 items-center gap-1" title={view.info.cwd}>
            <FolderIcon className="size-3.5 shrink-0" />
            <span className="truncate">{workspaceName(view.info.cwd)}</span>
          </span>
          {branch && (
            <span className="flex min-w-0 items-center gap-1" title={`分支 ${branch}`}>
              <GitBranchIcon className="size-3.5 shrink-0" />
              <span className="max-w-[160px] truncate font-mono text-[12px]">{branch}</span>
            </span>
          )}
        </div>

        {view.readOnly && (
          <Tooltip>
            <TooltipTrigger asChild>
              <span className="flex shrink-0 items-center gap-1 rounded-md bg-warn/12 px-1.5 py-0.5 text-[11px] font-medium text-warn">
                <LockIcon className="size-3" />
                只读{view.holder ? ` · ${view.holder.frontend}` : ""}
              </span>
            </TooltipTrigger>
            <TooltipContent>
              {view.holder
                ? `写租约由 ${view.holder.process_name ?? view.holder.frontend}（pid ${view.holder.pid}）持有`
                : "只读打开：可以查看历史，不能发送提示"}
            </TooltipContent>
          </Tooltip>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-0.5">
        {view.readOnly && !view.info.parent_session_id && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 text-xs"
            onClick={() =>
              reacquire(sessionId).catch((error) =>
                toast.error(`获取写权限失败：${errorText(error)}`),
              )
            }
          >
            <UnlockIcon className="size-3.5" />
            获取写权限
          </Button>
        )}
        {!isDraft && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <IconButton label="更多操作">
                <MoreHorizontalIcon />
              </IconButton>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-52">
              <DropdownMenuItem onSelect={() => setEditing(title)}>
                <PencilIcon />
                重命名
              </DropdownMenuItem>
              <DropdownMenuItem
                disabled={view.readOnly || view.running || compacting}
                onSelect={() => void compact()}
              >
                <MinimizeIcon />
                压缩上下文
              </DropdownMenuItem>
              <DropdownMenuItem
                onSelect={() => {
                  void navigator.clipboard.writeText(sessionId);
                  toast.success("已复制会话 ID");
                }}
              >
                <CopyIcon />
                复制会话 ID
              </DropdownMenuItem>
              {!view.readOnly && (
                <DropdownMenuItem
                  disabled={view.running}
                  onSelect={() =>
                    releaseSession(sessionId)
                      .then(() => toast.success("已释放写租约，其他进程可以接管"))
                      .catch((error) => toast.error(`释放失败：${errorText(error)}`))
                  }
                >
                  <UnlockIcon />
                  释放写租约
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem
                variant="destructive"
                onSelect={() => useUi.getState().set({ confirmDelete: { id: sessionId, title } })}
              >
                <Trash2Icon />
                删除会话
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
        <InspectorToggle sessionId={sessionId} />
      </div>
    </header>
  );
}
