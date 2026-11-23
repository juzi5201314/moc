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
import { type MsgKey, useT } from "@/lib/i18n";
import { cyclePlan } from "@/lib/shortcuts";
import { SEARCH_MIN_CHARS, useSessionSearch } from "@/lib/use-session-search";
import { useLayout } from "@/store/layout";
import { createQuickSession, openSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions, workspaceName } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { openSettings, type SettingsSection, useUi } from "@/store/ui";

const SETTINGS_ENTRIES: { section: SettingsSection; label: MsgKey; icon: ReactNode }[] = [
  { section: "providers", label: "sd.providers", icon: <CpuIcon /> },
  { section: "roles", label: "sd.roles", icon: <BoxesIcon /> },
  { section: "general", label: "pal.general", icon: <SettingsIcon /> },
  { section: "shortcuts", label: "sd.shortcuts", icon: <KeyboardIcon /> },
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
  const t = useT();
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
      label: t("hd.new"),
      hint: activeInfo ? workspaceName(activeInfo.cwd) : undefined,
      keys: "Alt N",
      onSelect: () => createQuickSession(activeInfo?.cwd),
    },
    ...(activeId
      ? [
          {
            icon: <CpuIcon />,
            label: t("pal.model"),
            keys: "Alt P",
            onSelect: () => useUi.getState().set({ modelPicker: activeId }),
          },
          {
            icon: <ListChecksIcon />,
            label: activeInfo?.plan_active ? t("pal.plan_off") : t("pal.plan_on"),
            keys: "Alt J",
            onSelect: cyclePlan,
          },
        ]
      : []),
    {
      icon: <PanelLeftIcon />,
      label: t("pal.toggle_sidebar"),
      keys: "Ctrl B",
      onSelect: () => useLayout.getState().toggleSidebar(),
    },
    {
      icon: <PanelRightIcon />,
      label: t("pal.toggle_inspector"),
      keys: "Alt R",
      onSelect: () => useLayout.getState().toggleRightbar(),
    },
    {
      icon: theme === "dark" ? <SunIcon /> : <MoonIcon />,
      label: theme === "dark" ? t("pal.theme_light") : t("pal.theme_dark"),
      onSelect: () => usePrefs.getState().set({ theme: theme === "dark" ? "light" : "dark" }),
    },
  ];
  const shownActions = actions.filter((action) => match(action.label));
  const shownSettings = SETTINGS_ENTRIES.filter((entry) =>
    match(`${t("sb.settings")} ${t(entry.label)}`),
  );

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
      title={t("pal.title")}
      description={t("pal.desc")}
      showCloseButton={false}
      className="top-[18%] translate-y-0 gap-0 rounded-2xl sm:max-w-[560px]"
      commandProps={{ shouldFilter: false }}
    >
      <CommandInput placeholder={t("pal.ph")} value={query} onValueChange={setQuery} />
      <CommandList className="max-h-[min(420px,60vh)] px-1.5 pb-1.5">
        <CommandEmpty className="py-10 text-center text-sm text-text-tertiary">
          {t("pal.empty")}
        </CommandEmpty>

        {shownActions.length > 0 && (
          <CommandGroup heading={t("pal.actions")}>
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
          <CommandGroup heading={t("sb.settings")}>
            {shownSettings.map((entry) => (
              <Item
                key={entry.section}
                value={`${t("sb.settings")} ${t(entry.label)}`}
                icon={entry.icon}
                label={t(entry.label)}
                onSelect={() => openSettings(entry.section)}
              />
            ))}
          </CommandGroup>
        )}

        {shownSessions.length > 0 && (
          <CommandGroup heading={t("pal.sessions")}>
            {shownSessions.map((session) => (
              <Item
                key={session.id}
                value={`${session.title || t("hd.untitled")} ${session.cwd} ${session.id}`}
                icon={<MessageSquareIcon />}
                label={session.title || t("hd.untitled")}
                hint={`${workspaceName(session.cwd)} · ${relativeTime(session.updated_at_ms)}`}
                onSelect={() => void openSession(session.id)}
              />
            ))}
            {hiddenSessions > 0 && (
              <div className="px-2.5 pb-1 text-xs text-text-tertiary">
                {t("pal.more_hint", { n: hiddenSessions })}
              </div>
            )}
          </CommandGroup>
        )}

        {needle.length >= SEARCH_MIN_CHARS && shownContent.length > 0 && (
          <CommandGroup heading={t("sb.content_hits")}>
            {shownContent.map((hit) => (
              <CommandItem
                key={hit.session.id}
                value={`${t("sb.content_hits")} ${hit.session.id}`}
                onSelect={() => {
                  useUi.getState().set({ palette: false });
                  void openSession(hit.session.id);
                }}
                className="h-auto gap-2.5 rounded-lg px-2.5 py-1.5 text-ui [&_svg]:size-4 [&_svg]:text-muted-foreground"
              >
                <MessageSquareIcon className="mt-0.5 self-start" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate">{hit.session.title || t("hd.untitled")}</span>
                  <span className="block truncate text-xs text-text-tertiary">
                    {hit.matches[0]?.snippet}
                  </span>
                </span>
              </CommandItem>
            ))}
          </CommandGroup>
        )}
      </CommandList>
      <div className="flex h-9 shrink-0 items-center gap-4 border-t border-border-subtle px-4 text-2xs text-text-tertiary">
        <span className="flex items-center gap-1.5">
          <Kbd>↑↓</Kbd>
          {t("pal.nav")}
        </span>
        <span className="flex items-center gap-1.5">
          <Kbd>↵</Kbd>
          {t("pal.select")}
        </span>
        <span className="flex items-center gap-1.5">
          <Kbd>Esc</Kbd>
          {t("pal.close")}
        </span>
      </div>
    </CommandDialog>
  );
}
