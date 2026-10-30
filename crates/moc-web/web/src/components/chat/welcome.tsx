// 新会话欢迎页：问候 + 工作目录选择 + 内嵌输入框。首条消息发出后切到会话视图。

import {
  CheckIcon,
  ChevronDownIcon,
  FolderOpenIcon,
  FolderPlusIcon,
  GitBranchIcon,
} from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useGitBranch } from "@/lib/use-git-branch";
import { cn } from "@/lib/utils";
import { setDraftCwd } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions, workspaceName } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { ComposerBar } from "./composer-bar";
import { InspectorToggle, SidebarReveal } from "./session-header";

function greeting(): string {
  const hour = new Date().getHours();
  if (hour < 6) return "夜深了，还在忙什么？";
  if (hour < 12) return "早上好，今天做点什么？";
  if (hour < 18) return "下午好，想一起完成什么？";
  return "晚上好，接着做点什么？";
}

function WorkspaceSwitch({ sessionId }: { sessionId: string }) {
  const cwd = useTranscript((state) => state.bySession[sessionId]?.info.cwd ?? "");
  const branch = useGitBranch(cwd || undefined);
  const workspaces = usePrefs((state) => state.workspaces);
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const isDraft = sessionId === "draft-session";

  // 手动添加的目录在前，其余按最近会话出现的顺序去重。
  const dirs = [
    ...new Set([...workspaces, ...allSessions({ list, extra }).map((session) => session.cwd)]),
  ].filter(Boolean);

  const addDirectory = () =>
    useUi.getState().set({
      folderPicker: {
        open: true,
        start: cwd || undefined,
        onPick: (path) => {
          usePrefs.getState().addWorkspace(path);
          setDraftCwd(path);
        },
      },
    });

  return (
    <div className="flex items-center justify-center gap-1 text-[13px] text-muted-foreground">
      <DropdownMenu>
        <DropdownMenuTrigger asChild disabled={!isDraft}>
          <button
            type="button"
            className="inline-flex h-7 min-w-0 items-center gap-1.5 rounded-lg px-2 transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none data-[state=open]:bg-accent"
            title={cwd}
          >
            <FolderOpenIcon className="size-3.5 shrink-0" />
            <span className="max-w-[min(48vw,360px)] truncate font-medium text-foreground">
              {cwd ? workspaceName(cwd) : "选择工作目录"}
            </span>
            {isDraft && <ChevronDownIcon className="size-3 shrink-0 opacity-60" />}
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="center" className="w-80 p-1">
          <DropdownMenuLabel className="px-2 py-1.5 text-[11px] font-medium text-text-tertiary">
            工作目录
          </DropdownMenuLabel>
          <div className="max-h-72 overflow-y-auto">
            {dirs.map((dir) => (
              <DropdownMenuItem
                key={dir}
                onSelect={() => setDraftCwd(dir)}
                className="gap-2.5 rounded-lg px-2 py-1.5"
              >
                <span className="flex min-w-0 flex-1 flex-col">
                  <span className="truncate text-[13px] text-foreground">{workspaceName(dir)}</span>
                  <span className="truncate font-mono text-[11px] text-text-tertiary">{dir}</span>
                </span>
                <CheckIcon className={cn("size-4 text-brand", dir !== cwd && "invisible")} />
              </DropdownMenuItem>
            ))}
          </div>
          {dirs.length > 0 && <DropdownMenuSeparator />}
          <DropdownMenuItem onSelect={addDirectory} className="gap-2.5 rounded-lg px-2 py-1.5">
            <FolderPlusIcon className="size-4" />
            <span className="text-[13px]">打开其他目录…</span>
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      {branch && (
        <span className="inline-flex min-w-0 items-center gap-1 px-1 text-text-tertiary">
          <GitBranchIcon className="size-3.5 shrink-0" />
          <span className="max-w-[160px] truncate font-mono text-[12px]">{branch}</span>
        </span>
      )}
    </div>
  );
}

export function WelcomeView({ sessionId }: { sessionId: string }) {
  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <header className="flex h-12 shrink-0 items-center gap-2 px-3">
        <SidebarReveal />
        <div className="ml-auto">
          <InspectorToggle sessionId={sessionId} />
        </div>
      </header>
      <div className="flex min-h-0 flex-1 flex-col items-center overflow-y-auto px-6 pb-[12vh]">
        <div className="my-auto flex w-full max-w-[720px] flex-col gap-6">
          <div className="flex flex-col items-center gap-3 text-center">
            <h1 className="text-[26px] font-semibold tracking-[-0.01em] text-foreground">
              {greeting()}
            </h1>
            <WorkspaceSwitch sessionId={sessionId} />
          </div>
          <ComposerBar sessionId={sessionId} inline />
          <p className="text-center text-xs text-text-tertiary">
            <kbd className="font-sans">/</kbd> 命令 · <kbd className="font-sans">!</kbd> 执行 shell
            · 粘贴或拖入图片 · <kbd className="font-sans">Ctrl K</kbd> 命令面板
          </p>
        </div>
      </div>
    </div>
  );
}
