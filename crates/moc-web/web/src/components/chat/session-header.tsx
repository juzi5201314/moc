// 会话顶栏：标题（双击/菜单重命名）+ 目录与分支面包屑 + 只读状态 + 检查面板与更多操作。

import {
  CopyIcon,
  FolderIcon,
  GitBranchIcon,
  ListTodoIcon,
  LockIcon,
  MinimizeIcon,
  MoreHorizontalIcon,
  PanelRightIcon,
  PencilIcon,
  SquarePenIcon,
  Trash2Icon,
  UnlockIcon,
} from "lucide-react";
import { useRef, useState } from "react";
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
import { useT } from "@/lib/i18n";
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
  const t = useT();
  const collapsed = useLayout((state) => state.sidebarCollapsed);
  const overlay = useLayout((state) => state.sidebarOverlay);
  const narrow = useNarrow();
  if (!(narrow ? !overlay : collapsed)) return null;
  return (
    <div className="flex items-center gap-0.5">
      <IconButton label={t("hd.new")} shortcut="Alt N" onClick={() => createQuickSession()}>
        <SquarePenIcon />
      </IconButton>
    </div>
  );
}

export function InspectorToggle({ sessionId }: { sessionId: string }) {
  const t = useT();
  const open = useLayout((state) => state.rightbarOpen);
  const pending = useTranscript((state) => attentionCount(state, sessionId));
  const narrow = useNarrow();
  // 宽屏面板打开时，开关已在面板标签条右端，主舞台不再重复一个。
  if (open && !narrow) return null;
  return (
    <div className="relative">
      <IconButton
        label={open ? t("hd.inspector_close") : t("hd.inspector_open")}
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
  const t = useT();
  const view = useTranscript((state) => state.bySession[sessionId]);
  const branch = useGitBranch(view?.info.cwd);
  const [editing, setEditing] = useState<string | null>(null);
  const renamingFromMenu = useRef(false);
  const titleButton = useRef<HTMLButtonElement>(null);
  const [compacting, setCompacting] = useState(false);

  if (!view) return null;
  const title = view.info.title || t("hd.untitled");
  const isDraft = sessionId === "draft-session";

  const commitTitle = () => {
    const next = editing?.trim();
    setEditing(null);
    if (next && next !== title && !isDraft) {
      renameSession(sessionId, next).catch((error) =>
        toast.error(t("hd.rename_failed", { error: errorText(error) })),
      );
    }
  };

  const compact = async () => {
    setCompacting(true);
    try {
      const result = await compactSession(sessionId);
      toast.success(
        t("slash.compacted", {
          before: result.tokens_before.toLocaleString(),
          after: result.tokens_after.toLocaleString(),
        }),
      );
    } catch (error) {
      toast.error(t("slash.compact_failed", { error: errorText(error) }));
    } finally {
      setCompacting(false);
    }
  };

  return (
    <header className="flex h-12 shrink-0 items-center gap-2 px-4">
      <SidebarReveal />

      <div className="flex min-w-0 flex-1 items-center gap-2.5">
        {editing !== null ? (
          <input
            // biome-ignore lint/a11y/noAutofocus: 进入重命名后立即编辑
            autoFocus
            aria-label={t("hd.rename")}
            value={editing}
            onChange={(event) => setEditing(event.target.value)}
            onFocus={(event) => event.currentTarget.select()}
            onKeyDown={(event) => {
              if (event.nativeEvent.isComposing) return;
              if (event.key === "Enter") commitTitle();
              else if (event.key === "Escape") setEditing(null);
              else return;
              event.preventDefault();
              requestAnimationFrame(() => titleButton.current?.focus());
            }}
            onBlur={commitTitle}
            className="h-7 w-[min(360px,50%)] rounded-md bg-accent px-2 text-sm font-medium outline-none ring-1 ring-ring"
          />
        ) : (
          <button
            ref={titleButton}
            type="button"
            onDoubleClick={() => !isDraft && setEditing(title)}
            className="min-w-0 truncate rounded-md px-1 text-sm font-medium text-foreground"
            title={isDraft ? title : t("hd.rename_hint", { title })}
          >
            {title}
          </button>
        )}

        {/* 目录与分支各自封顶宽度且不让位：空间不足时先截断标题，免得两枚 chip 被挤成「m…」。 */}
        <div className="hidden shrink-0 items-center gap-2.5 text-xs text-muted-foreground md:flex">
          <span className="flex min-w-0 items-center gap-1" title={view.info.cwd}>
            <FolderIcon className="size-3.5 shrink-0" />
            <span className="max-w-[160px] truncate">{workspaceName(view.info.cwd)}</span>
          </span>
          {branch && (
            <span className="flex min-w-0 items-center gap-1" title={t("hd.branch", { branch })}>
              <GitBranchIcon className="size-3.5 shrink-0" />
              <span className="max-w-[160px] truncate font-mono text-xs">{branch}</span>
            </span>
          )}
        </div>

        {view.readOnly && (
          <Tooltip>
            <TooltipTrigger asChild>
              <span className="flex shrink-0 items-center gap-1 rounded-md bg-warn/12 px-1.5 py-0.5 text-2xs font-medium text-warn">
                <LockIcon className="size-3" />
                {view.holder
                  ? t("hd.readonly_by", { frontend: view.holder.frontend })
                  : t("hd.readonly")}
              </span>
            </TooltipTrigger>
            <TooltipContent>
              {view.holder
                ? t("hd.lease_by", {
                    name: view.holder.process_name ?? view.holder.frontend,
                    pid: view.holder.pid,
                  })
                : t("hd.readonly_tip")}
            </TooltipContent>
          </Tooltip>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-0.5">
        {!isDraft && (
          <IconButton
            label={t("hd.jobs")}
            onClick={() => useUi.getState().set({ jobsSession: sessionId })}
          >
            <ListTodoIcon />
          </IconButton>
        )}
        {view.readOnly && !view.info.parent_session_id && (
          <Button
            variant="ghost"
            size="sm"
            className="h-7 text-xs"
            onClick={() =>
              reacquire(sessionId).catch((error) =>
                toast.error(t("cb.acquire_failed", { error: errorText(error) })),
              )
            }
          >
            <UnlockIcon className="size-3.5" />
            {t("cb.acquire")}
          </Button>
        )}
        {!isDraft && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <IconButton label={t("hd.more")}>
                <MoreHorizontalIcon />
              </IconButton>
            </DropdownMenuTrigger>
            <DropdownMenuContent
              align="end"
              className="w-52"
              onCloseAutoFocus={(event) => {
                if (!renamingFromMenu.current) return;
                event.preventDefault();
                renamingFromMenu.current = false;
                setEditing(title);
              }}
            >
              <DropdownMenuItem
                onSelect={() => {
                  renamingFromMenu.current = true;
                }}
              >
                <PencilIcon />
                {t("hd.rename")}
              </DropdownMenuItem>
              <DropdownMenuItem
                disabled={view.readOnly || view.running || compacting}
                onSelect={() => void compact()}
              >
                <MinimizeIcon />
                {t("hd.compact")}
              </DropdownMenuItem>
              <DropdownMenuItem
                onSelect={() => {
                  void navigator.clipboard.writeText(sessionId);
                  toast.success(t("hd.id_copied"));
                }}
              >
                <CopyIcon />
                {t("hd.copy_id")}
              </DropdownMenuItem>
              {!view.readOnly && (
                <DropdownMenuItem
                  disabled={view.running}
                  onSelect={() =>
                    releaseSession(sessionId)
                      .then(() => toast.success(t("hd.lease_released")))
                      .catch((error) =>
                        toast.error(t("hd.release_failed", { error: errorText(error) })),
                      )
                  }
                >
                  <UnlockIcon />
                  {t("hd.release_lease")}
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem
                variant="destructive"
                onSelect={() => useUi.getState().set({ confirmDelete: { id: sessionId, title } })}
              >
                <Trash2Icon />
                {t("hd.delete")}
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
        <InspectorToggle sessionId={sessionId} />
      </div>
    </header>
  );
}
