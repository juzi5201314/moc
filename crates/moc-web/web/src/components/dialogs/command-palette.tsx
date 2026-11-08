// Ctrl K 命令面板：常用动作（带快捷键提示）、设置分区直达、按标题/目录/消息正文搜索会话。
// cmdk 过滤关闭（shouldFilter）：会话多时不能为它挂全部 CommandItem，
// 标题/目录命中与正文搜索命中都自己过滤后再限量渲染。

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
import { type ReactNode, useMemo, useState } from "react";
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
import { SEARCH_MIN_CHARS, useSessionSearch } from "@/lib/use-session-search";
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

/** 标题/目录命中一次最多挂载的会话行数；超出的提示继续输入。 */
const MAX_SESSION_ITEMS = 40;
/** 正文命中返回与展示的上限。 */
const MAX_CONTENT_ITEMS = 10;

const ITEM_CLASS =
  "h-9 gap-2.5 rounded-lg px-2.5 text-ui [&_svg]:size-4 [&_svg]:text-muted-foreground";

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
      className={ITEM_CLASS}
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

type ActionEntry = {
  icon: ReactNode;
  label: string;
  hint?: string;
  keys?: string;
  onSelect: () => void;
};

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

  const [query, setQuery] = useState("");
  const needle = query.trim().toLowerCase();
  const match = (text: string) => !needle || text.toLowerCase().includes(needle);
  const { hits: contentHits } = useSessionSearch(query, MAX_CONTENT_ITEMS);

  const actions: ActionEntry[] = [
    {
      icon: <PlusIcon />,
      label: "新建会话",
      hint: activeInfo ? workspaceName(activeInfo.cwd) : undefined,
      keys: "Alt N",
      onSelect: () => createQuickSession(activeInfo?.cwd),
    },
    ...(activeId
      ? [
          {
            icon: <CpuIcon />,
            label: "切换模型",
            keys: "Alt P",
            onSelect: () => useUi.getState().set({ modelPicker: activeId }),
          },
          {
            icon: <ListChecksIcon />,
            label: activeInfo?.plan_active ? "退出计划状态" : "进入计划状态",
            keys: "Alt J",
            onSelect: cyclePlan,
          },
        ]
      : []),
    {
      icon: <PanelLeftIcon />,
      label: "展开 / 收起侧栏",
      keys: "Ctrl B",
      onSelect: () => useLayout.getState().toggleSidebar(),
    },
    {
      icon: <PanelRightIcon />,
      label: "展开 / 收起检查面板",
      keys: "Alt R",
      onSelect: () => useLayout.getState().toggleRightbar(),
    },
    {
      icon: theme === "dark" ? <SunIcon /> : <MoonIcon />,
      label: theme === "dark" ? "切换到浅色主题" : "切换到深色主题",
      onSelect: () => usePrefs.getState().set({ theme: theme === "dark" ? "light" : "dark" }),
    },
  ];
  const shownActions = actions.filter((action) => match(action.label));
  const shownSettings = SETTINGS_ENTRIES.filter((entry) => match(`设置 ${entry.label}`));

  const { shownSessions, hiddenSessions, titleMatchedIds } = useMemo(() => {
    const matched = needle
      ? sessions.filter((session) =>
          `${session.title} ${session.cwd}`.toLowerCase().includes(needle),
        )
      : sessions;
    return {
      shownSessions: matched.slice(0, MAX_SESSION_ITEMS),
      hiddenSessions: Math.max(0, matched.length - MAX_SESSION_ITEMS),
      titleMatchedIds: new Set(matched.map((session) => session.id)),
    };
  }, [sessions, needle]);

  // 标题/目录已命中的会话不再进正文组，同一会话不出现两次。
  const shownContent = contentHits.filter((hit) => !titleMatchedIds.has(hit.session.id));

  return (
    <CommandDialog
      open={palette}
      onOpenChange={(open) => {
        useUi.getState().set({ palette: open });
        // 面板外的 query 状态不会随 Dialog 卸载，关闭时手动清空。
        if (!open) setQuery("");
      }}
      title="命令面板"
      description="搜索动作、设置或会话"
      showCloseButton={false}
      className="top-[18%] translate-y-0 gap-0 rounded-2xl sm:max-w-[560px]"
      commandProps={{ shouldFilter: false }}
    >
      <CommandInput placeholder="搜索动作、设置或会话…" value={query} onValueChange={setQuery} />
      <CommandList className="max-h-[min(420px,60vh)] px-1.5 pb-1.5">
        <CommandEmpty className="py-10 text-center text-sm text-text-tertiary">
          没有匹配项
        </CommandEmpty>

        {shownActions.length > 0 && (
          <CommandGroup heading="动作">
            {shownActions.map((action) => (
              <Item
                key={action.label}
                icon={action.icon}
                label={action.label}
                hint={action.hint}
                keys={action.keys}
                onSelect={action.onSelect}
              />
            ))}
          </CommandGroup>
        )}

        {shownSettings.length > 0 && (
          <CommandGroup heading="设置">
            {shownSettings.map((entry) => (
              <Item
                key={entry.section}
                value={`设置 ${entry.label}`}
                icon={entry.icon}
                label={entry.label}
                onSelect={() => openSettings(entry.section)}
              />
            ))}
          </CommandGroup>
        )}

        {shownSessions.length > 0 && (
          <CommandGroup heading="会话">
            {shownSessions.map((session) => (
              <Item
                key={session.id}
                value={`${session.title || "未命名会话"} ${session.cwd} ${session.id}`}
                icon={<MessageSquareIcon />}
                label={session.title || "未命名会话"}
                hint={`${workspaceName(session.cwd)} · ${relativeTime(session.updated_at_ms)}`}
                onSelect={() => void openSession(session.id)}
              />
            ))}
            {hiddenSessions > 0 && (
              <div className="px-2.5 pb-1 text-xs text-text-tertiary">
                还有 {hiddenSessions} 个匹配会话，继续输入可缩小范围
              </div>
            )}
          </CommandGroup>
        )}

        {needle.length >= SEARCH_MIN_CHARS && shownContent.length > 0 && (
          <CommandGroup heading="内容匹配">
            {shownContent.map((hit) => (
              <CommandItem
                key={hit.session.id}
                value={`内容匹配 ${hit.session.id}`}
                onSelect={() => {
                  useUi.getState().set({ palette: false });
                  void openSession(hit.session.id);
                }}
                className="h-auto gap-2.5 rounded-lg px-2.5 py-1.5 text-ui [&_svg]:size-4 [&_svg]:text-muted-foreground"
              >
                <MessageSquareIcon className="mt-0.5 self-start" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate">{hit.session.title || "未命名会话"}</span>
                  <span className="block truncate text-xs text-text-tertiary">
                    {hit.matches[0]?.snippet}
                  </span>
                </span>
              </CommandItem>
            ))}
          </CommandGroup>
        )}
      </CommandList>
    </CommandDialog>
  );
}
