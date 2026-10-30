// Ctrl K 命令面板：常用动作（带快捷键提示）、设置分区直达、按标题或目录搜索会话。

import {
  BoxesIcon,
  CpuIcon,
  KeyboardIcon,
  ListChecksIcon,
  MessageSquareIcon,
  MoonIcon,
  PanelLeftIcon,
  PanelRightIcon,
  PlusIcon,
  SettingsIcon,
  SunIcon,
} from "lucide-react";
import { type ReactNode, useMemo } from "react";
import { Kbd } from "@/components/common/icon-button";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { relativeTime } from "@/lib/format";
import { cyclePlan } from "@/lib/shortcuts";
import { useLayout } from "@/store/layout";
import { createQuickSession, openSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions, workspaceName } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { openSettings, type SettingsSection, useUi } from "@/store/ui";

const SETTINGS_ENTRIES: { section: SettingsSection; label: string; icon: ReactNode }[] = [
  { section: "providers", label: "供应商与模型", icon: <CpuIcon /> },
  { section: "roles", label: "模型角色", icon: <BoxesIcon /> },
  { section: "general", label: "通用设置", icon: <SettingsIcon /> },
  { section: "shortcuts", label: "快捷键", icon: <KeyboardIcon /> },
];

function Item({
  value,
  icon,
  label,
  hint,
  keys,
  onSelect,
}: {
  value?: string;
  icon: ReactNode;
  label: string;
  hint?: string;
  keys?: string;
  onSelect: () => void;
}) {
  return (
    <CommandItem
      value={value}
      onSelect={() => {
        useUi.getState().set({ palette: false });
        onSelect();
      }}
      className="h-9 gap-2.5 rounded-lg px-2.5 text-[13px] [&_svg]:size-4 [&_svg]:text-muted-foreground"
    >
      {icon}
      <span className="min-w-0 truncate">{label}</span>
      {hint && <span className="min-w-0 shrink truncate text-xs text-text-tertiary">{hint}</span>}
      {keys && (
        <span className="ml-auto flex shrink-0 gap-1">
          {keys.split(" ").map((key) => (
            <Kbd key={key}>{key}</Kbd>
          ))}
        </span>
      )}
    </CommandItem>
  );
}

export function CommandPalette() {
  const palette = useUi((state) => state.palette);
  const list = useSessions((state) => state.list);
  const extra = useSessions((state) => state.extra);
  const sessions = useMemo(() => allSessions({ list, extra }), [list, extra]);
  const theme = usePrefs((state) => state.theme);
  const activeId = useTabs((state) => state.active);
  const activeInfo = useTranscript((state) =>
    activeId ? state.bySession[activeId]?.info : undefined,
  );

  return (
    <CommandDialog
      open={palette}
      onOpenChange={(open) => useUi.getState().set({ palette: open })}
      title="命令面板"
      description="搜索动作、设置或会话"
      showCloseButton={false}
      className="top-[18%] translate-y-0 gap-0 rounded-2xl sm:max-w-[560px]"
    >
      <CommandInput placeholder="搜索动作、设置或会话…" />
      <CommandList className="max-h-[min(420px,60vh)] px-1.5 pb-1.5">
        <CommandEmpty className="py-10 text-center text-sm text-text-tertiary">
          没有匹配项
        </CommandEmpty>

        <CommandGroup heading="动作">
          <Item
            icon={<PlusIcon />}
            label="新建会话"
            hint={activeInfo ? workspaceName(activeInfo.cwd) : undefined}
            keys="Alt N"
            onSelect={() => createQuickSession(activeInfo?.cwd)}
          />
          {activeId && (
            <>
              <Item
                icon={<CpuIcon />}
                label="切换模型"
                keys="Alt P"
                onSelect={() => useUi.getState().set({ modelPicker: activeId })}
              />
              <Item
                icon={<ListChecksIcon />}
                label={activeInfo?.plan_active ? "退出计划状态" : "进入计划状态"}
                keys="Alt J"
                onSelect={cyclePlan}
              />
            </>
          )}
          <Item
            icon={<PanelLeftIcon />}
            label="展开 / 收起侧栏"
            keys="Ctrl B"
            onSelect={() => useLayout.getState().toggleSidebar()}
          />
          <Item
            icon={<PanelRightIcon />}
            label="展开 / 收起检查面板"
            keys="Alt R"
            onSelect={() => useLayout.getState().toggleRightbar()}
          />
          <Item
            icon={theme === "dark" ? <SunIcon /> : <MoonIcon />}
            label={theme === "dark" ? "切换到浅色主题" : "切换到深色主题"}
            onSelect={() => usePrefs.getState().set({ theme: theme === "dark" ? "light" : "dark" })}
          />
        </CommandGroup>

        <CommandGroup heading="设置">
          {SETTINGS_ENTRIES.map((entry) => (
            <Item
              key={entry.section}
              value={`设置 ${entry.label}`}
              icon={entry.icon}
              label={entry.label}
              onSelect={() => openSettings(entry.section)}
            />
          ))}
        </CommandGroup>

        {sessions.length > 0 && (
          <CommandGroup heading="会话">
            {sessions.map((session) => (
              <Item
                key={session.id}
                // cmdk 按 value 去重与过滤：带上 id 防止同名会话互相吞掉。
                value={`${session.title || "未命名会话"} ${session.cwd} ${session.id}`}
                icon={<MessageSquareIcon />}
                label={session.title || "未命名会话"}
                hint={`${workspaceName(session.cwd)} · ${relativeTime(session.updated_at_ms)}`}
                onSelect={() => void openSession(session.id)}
              />
            ))}
          </CommandGroup>
        )}
      </CommandList>
    </CommandDialog>
  );
}
