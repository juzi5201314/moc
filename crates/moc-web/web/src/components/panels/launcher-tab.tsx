// 检查面板的「新标签页」：居中搜索框 + 分组入口，打开其它分页或工具弹窗。
// 列表很短，直接按标签与说明做子串过滤；回车打开第一个命中项。

import {
  ActivityIcon,
  FlagIcon,
  FolderIcon,
  GitCompareArrowsIcon,
  LayersIcon,
  ListTodoIcon,
  type LucideIcon,
  MessagesSquareIcon,
  SearchIcon,
  SettingsIcon,
  SquareTerminalIcon,
  StethoscopeIcon,
  WrenchIcon,
} from "lucide-react";
import { useMemo, useState } from "react";
import { type MsgKey, useT } from "@/lib/i18n";
import { useLayout } from "@/store/layout";
import { openSettings, useUi } from "@/store/ui";

type Entry = { id: string; label: MsgKey; hint: MsgKey; icon: LucideIcon; run: () => void };

function entries(sessionId: string): { group: MsgKey; items: Entry[] }[] {
  const open = useLayout.getState().openInspector;
  const ui = useUi.getState().set;
  return [
    {
      group: "nt.panels",
      items: [
        {
          id: "files",
          label: "insp.files",
          hint: "nt.files_hint",
          icon: FolderIcon,
          run: () => open("files"),
        },
        {
          id: "terminal",
          label: "insp.terminal",
          hint: "nt.terminal_hint",
          icon: SquareTerminalIcon,
          run: () => open("terminal"),
        },
        {
          id: "changes",
          label: "insp.changes",
          hint: "nt.changes_hint",
          icon: GitCompareArrowsIcon,
          run: () => open("changes"),
        },
        {
          id: "btw",
          label: "insp.btw",
          hint: "nt.btw_hint",
          icon: MessagesSquareIcon,
          run: () => open("btw"),
        },
      ],
    },
    {
      group: "nt.tools",
      items: [
        {
          id: "context",
          label: "nt.context",
          hint: "nt.context_hint",
          icon: LayersIcon,
          run: () => ui({ contextDialog: sessionId }),
        },
        {
          id: "jobs",
          label: "nt.jobs",
          hint: "nt.jobs_hint",
          icon: ListTodoIcon,
          run: () => ui({ jobsSession: sessionId }),
        },
        {
          id: "tools",
          label: "nt.tools_list",
          hint: "nt.tools_hint",
          icon: WrenchIcon,
          run: () => ui({ toolsSession: sessionId }),
        },
        {
          id: "features",
          label: "nt.features",
          hint: "nt.features_hint",
          icon: FlagIcon,
          run: () => ui({ features: true }),
        },
        {
          id: "doctor",
          label: "nt.doctor",
          hint: "nt.doctor_hint",
          icon: StethoscopeIcon,
          run: () => ui({ doctor: true }),
        },
        {
          id: "monitor",
          label: "nt.monitor",
          hint: "nt.monitor_hint",
          icon: ActivityIcon,
          run: () => ui({ monitor: true }),
        },
        {
          id: "settings",
          label: "nt.settings",
          hint: "nt.settings_hint",
          icon: SettingsIcon,
          run: () => openSettings(),
        },
      ],
    },
  ];
}

export function LauncherTab({ sessionId }: { sessionId: string }) {
  const t = useT();
  const [query, setQuery] = useState("");
  const groups = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return entries(sessionId)
      .map(({ group, items }) => ({
        group,
        items: needle
          ? items.filter((item) =>
              `${t(item.label)} ${t(item.hint)} ${item.id}`.toLowerCase().includes(needle),
            )
          : items,
      }))
      .filter(({ items }) => items.length > 0);
  }, [query, sessionId, t]);

  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex w-full max-w-md flex-col gap-6 px-5 pt-[12vh] pb-10">
        <label className="flex h-10 items-center gap-2.5 rounded-xl bg-card px-3.5 shadow-[var(--elevation-composer-inset)] transition-shadow focus-within:shadow-[0_0_0_1px_var(--ring)]">
          <SearchIcon className="size-4 shrink-0 text-text-tertiary" />
          <input
            // biome-ignore lint/a11y/noAutofocus: 新标签页的唯一主操作就是搜索
            autoFocus
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") groups[0]?.items[0]?.run();
            }}
            placeholder={t("nt.ph")}
            aria-label={t("nt.ph")}
            className="min-w-0 flex-1 bg-transparent text-ui text-foreground outline-none placeholder:text-text-tertiary"
          />
        </label>
        {groups.length === 0 ? (
          <p className="text-center text-ui text-text-tertiary">{t("nt.empty")}</p>
        ) : (
          groups.map(({ group, items }) => (
            <section key={group} className="flex flex-col">
              <h3 className="px-2 pb-1.5 text-xs font-medium text-text-tertiary">{t(group)}</h3>
              {items.map((item) => {
                const Icon = item.icon;
                return (
                  <button
                    key={item.id}
                    type="button"
                    onClick={item.run}
                    className="group/entry flex h-8 items-center gap-2.5 rounded-md px-2 text-left text-ui text-foreground transition-colors hover:bg-surface-hover focus-visible:bg-surface-hover focus-visible:outline-none"
                  >
                    <Icon className="size-4 shrink-0 text-muted-foreground" />
                    <span className="shrink-0">{t(item.label)}</span>
                    <span className="min-w-0 truncate text-xs text-text-tertiary">
                      {t(item.hint)}
                    </span>
                  </button>
                );
              })}
            </section>
          ))
        )}
      </div>
    </div>
  );
}
