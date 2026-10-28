import { BotIcon, MoonIcon, PlusIcon, SettingsIcon, SunIcon } from "lucide-react";
import { useMemo } from "react";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { createQuickSession, openSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions } from "@/store/sessions";
import { useUi } from "@/store/ui";

export function CommandPalette() {
  const palette = useUi((state) => state.palette);
  const setUi = useUi((state) => state.set);
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const sessions = useMemo(() => allSessions({ list, extra }), [list, extra]);
  const theme = usePrefs((state) => state.theme);
  const setPrefs = usePrefs((state) => state.set);

  return (
    <CommandDialog open={palette} onOpenChange={(open) => setUi({ palette: open })}>
      <CommandInput placeholder="输入指令或搜索会话..." />
      <CommandList>
        <CommandEmpty>未找到结果</CommandEmpty>

        <CommandGroup heading="快捷动作">
          <CommandItem
            onSelect={() => {
              setUi({ palette: false });
              void createQuickSession();
            }}
          >
            <PlusIcon className="mr-2 size-4" />
            <span>新建会话</span>
          </CommandItem>
          <CommandItem
            onSelect={() => {
              setUi({ palette: false });
              setPrefs({ theme: theme === "dark" ? "light" : "dark" });
            }}
          >
            {theme === "dark" ? (
              <SunIcon className="mr-2 size-4" />
            ) : (
              <MoonIcon className="mr-2 size-4" />
            )}
            <span>切换色彩主题</span>
          </CommandItem>
          <CommandItem
            onSelect={() => {
              setUi({ palette: false, settings: { open: true } });
            }}
          >
            <SettingsIcon className="mr-2 size-4" />
            <span>打开设置</span>
          </CommandItem>
        </CommandGroup>

        <CommandGroup heading="切换会话">
          {sessions.map((session) => (
            <CommandItem
              key={session.id}
              onSelect={() => {
                setUi({ palette: false });
                void openSession(session.id);
              }}
            >
              <BotIcon className="mr-2 size-4" />
              <span>{session.title || "未命名会话"}</span>
              <span className="ml-auto text-xs text-muted-foreground">{session.cwd}</span>
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
