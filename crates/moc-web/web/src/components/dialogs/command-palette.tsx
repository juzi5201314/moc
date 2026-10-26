// 命令面板（Ctrl+K）：会话、工作区、斜杠命令、设置页。

import { FolderPlus, MessageSquare, Settings, SquareSlash } from "lucide-react";
import { useMemo } from "react";
import { toast } from "sonner";
import { BUILTIN_COMMANDS, runSlash } from "@/components/composer/commands";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { useComposer } from "@/store/composer";
import { openSession } from "@/store/lifecycle";
import { allSessions, groupWorkspaces, useSessions, workspaceName } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { requestComposerFocus, useUi } from "@/store/ui";

/** 需要确认对话框的命令只能在 composer 里执行。 */
const COMPOSER_ONLY: Record<string, true> = { "clm-experimental": true };

export function CommandPalette() {
  const open = useUi((state) => state.palette);
  const set = useUi((state) => state.set);
  const active = useTabs((state) => state.active);
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const pinned = useSessions((state) => state.pinned);

  const sessions = useMemo(() => allSessions({ list, extra }), [list, extra]);
  const workspaces = useMemo(() => groupWorkspaces(sessions, pinned), [sessions, pinned]);

  const close = () => set({ palette: false });

  const runCommand = (name: string, usage: string) => {
    close();
    if (!active) {
      toast.error("先打开一个会话");
      return;
    }
    // 带必填参数的命令填进 composer，让用户补全参数。
    if (usage.includes("<")) {
      useComposer.getState().setText(active, `/${name} `);
      requestComposerFocus();
      return;
    }
    void runSlash(active, name, "", { confirmClm: () => {}, plugins: [] });
  };

  return (
    <CommandDialog
      open={open}
      onOpenChange={(next) => set({ palette: next })}
      title="命令面板"
      description="搜索会话、工作区与命令"
    >
      <CommandInput placeholder="搜索会话、工作区、命令…" />
      <CommandList>
        <CommandEmpty>没有匹配项</CommandEmpty>
        <CommandGroup heading="会话">
          {sessions
            .filter((session) => !session.parent_session_id)
            .map((session) => (
              <CommandItem
                key={session.id}
                value={`session ${session.title ?? ""} ${session.cwd} ${session.id}`}
                onSelect={() => {
                  close();
                  void openSession(session.id);
                }}
              >
                <MessageSquare aria-hidden />
                <span className="truncate">{session.title || "未命名会话"}</span>
                <span className="ml-auto truncate text-muted-foreground text-xs">
                  {workspaceName(session.cwd)}
                </span>
              </CommandItem>
            ))}
        </CommandGroup>
        <CommandGroup heading="工作区">
          {workspaces.map((workspace) => (
            <CommandItem
              key={workspace.cwd}
              value={`workspace ${workspace.cwd}`}
              onSelect={() =>
                set({ palette: false, newSession: { open: true, cwd: workspace.cwd } })
              }
            >
              <FolderPlus aria-hidden />
              <span className="truncate">在 {workspaceName(workspace.cwd)} 新建会话</span>
              <span className="ml-auto truncate text-muted-foreground text-xs">
                {workspace.cwd}
              </span>
            </CommandItem>
          ))}
        </CommandGroup>
        <CommandGroup heading="命令">
          {BUILTIN_COMMANDS.filter((command) => !Object.hasOwn(COMPOSER_ONLY, command.name)).map(
            (command) => (
              <CommandItem
                key={command.name}
                value={`command ${command.usage} ${command.description}`}
                onSelect={() => runCommand(command.name, command.usage)}
              >
                <SquareSlash aria-hidden />
                <span className="font-mono">{command.usage}</span>
                <span className="ml-auto text-muted-foreground text-xs">{command.description}</span>
              </CommandItem>
            ),
          )}
        </CommandGroup>
        <CommandGroup heading="设置">
          <CommandItem
            value="settings 界面 主题 显示"
            onSelect={() => set({ palette: false, settings: { open: true, tab: "界面" } })}
          >
            <Settings aria-hidden />
            设置 · 界面
          </CommandItem>
          <CommandItem
            value="settings 配置"
            onSelect={() => set({ palette: false, settings: { open: true } })}
          >
            <Settings aria-hidden />
            设置 · 全部配置
          </CommandItem>
          <CommandItem
            value="shortcuts 快捷键 帮助"
            onSelect={() => set({ palette: false, help: true })}
          >
            <Settings aria-hidden />
            快捷键帮助
          </CommandItem>
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
