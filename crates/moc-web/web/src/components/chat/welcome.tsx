// 新会话欢迎页：品牌标题 + 内嵌输入框 + 工作目录选择。

import { CheckIcon, ChevronDownIcon, FolderOpenIcon, FolderPlusIcon } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { setDraftCwd } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions, workspaceName } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { ComposerBar } from "./composer-bar";
import { GitBranchPicker } from "./git-branch-picker";
import { InspectorToggle, SidebarReveal } from "./session-header";
import { McpPopover } from "./status-popovers";

function WorkspaceSwitch({ sessionId }: { sessionId: string }) {
  const t = useT();
  const cwd = useTranscript((state) => state.bySession[sessionId]?.info.cwd ?? "");
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
    <div className="flex min-w-0 flex-wrap items-center gap-1 text-ui text-muted-foreground">
      <DropdownMenu>
        <DropdownMenuTrigger asChild disabled={!isDraft}>
          <button
            type="button"
            className="inline-flex h-8 min-w-0 items-center gap-1.5 rounded-lg px-2.5 transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none data-[state=open]:bg-accent"
            title={cwd}
          >
            <FolderOpenIcon className="size-3.5 shrink-0" />
            <span className="max-w-[min(48vw,360px)] truncate font-medium text-foreground">
              {cwd ? workspaceName(cwd) : t("wel.pick_dir")}
            </span>
            {isDraft && <ChevronDownIcon className="size-3 shrink-0 opacity-60" />}
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="start"
          className="w-80 max-w-[var(--radix-dropdown-menu-content-available-width)] p-1"
        >
          <DropdownMenuLabel className="px-2 py-1.5 text-2xs font-medium text-text-tertiary">
            {t("wel.dirs")}
          </DropdownMenuLabel>
          <div className="max-h-72 overflow-y-auto">
            {dirs.map((dir) => (
              <DropdownMenuItem
                key={dir}
                onSelect={() => setDraftCwd(dir)}
                className="gap-2.5 rounded-lg px-2 py-1.5"
              >
                <span className="flex min-w-0 flex-1 flex-col">
                  <span className="truncate text-ui text-foreground">{workspaceName(dir)}</span>
                  <span className="truncate font-mono text-2xs text-text-tertiary">{dir}</span>
                </span>
                <CheckIcon className={cn("size-4 text-brand", dir !== cwd && "invisible")} />
              </DropdownMenuItem>
            ))}
          </div>
          {dirs.length > 0 && <DropdownMenuSeparator />}
          <DropdownMenuItem onSelect={addDirectory} className="gap-2.5 rounded-lg px-2 py-1.5">
            <FolderPlusIcon className="size-4" />
            <span className="text-ui">{t("wel.open_other")}</span>
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <GitBranchPicker cwd={cwd || undefined} variant="welcome" />
    </div>
  );
}

export function WelcomeView({ sessionId }: { sessionId: string }) {
  const cwd = useTranscript((state) => state.bySession[sessionId]?.info.cwd ?? "");
  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <header className="flex h-12 shrink-0 items-center gap-2 px-3">
        <SidebarReveal />
        <div className="ml-auto">
          <InspectorToggle sessionId={sessionId} />
        </div>
      </header>
      <div className="flex min-h-0 flex-1 flex-col items-center overflow-y-auto px-4 pt-4 pb-16 sm:px-6">
        <div className="my-auto flex w-full max-w-[688px] flex-col">
          <ComposerBar sessionId={sessionId} inline />
          <div className="mt-1 flex flex-wrap items-center justify-between gap-2 px-1">
            <WorkspaceSwitch sessionId={sessionId} />
            <McpPopover sessionId={sessionId} cwd={cwd} side="bottom" align="end" />
          </div>
        </div>
      </div>
    </div>
  );
}
