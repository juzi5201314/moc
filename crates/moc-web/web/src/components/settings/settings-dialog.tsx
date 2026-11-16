// 设置：左侧分类导航 + 右侧分区内容；改动即保存，顶栏显示保存状态。
// webui 偏好（通用、快捷键）与 core 配置（其余分区，分全局 / 项目两个范围）分开。

import {
  BlocksIcon,
  CheckIcon,
  CommandIcon,
  CpuIcon,
  FileTextIcon,
  HardDriveIcon,
  LayersIcon,
  Loader2Icon,
  PlugIcon,
  SearchIcon,
  ServerIcon,
  Settings2Icon,
  ShieldIcon,
  SlidersHorizontalIcon,
  SparklesIcon,
  WrenchIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { type MsgKey, useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import type { ConfigField, ConfigScope, SettingsDocument } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { type SettingsSection, useUi } from "@/store/ui";
import { FieldList } from "./field";
import { ProvidersSection } from "./providers";
import {
  GeneralSection,
  MaintenanceCard,
  PluginsOverview,
  RolesSection,
  ShortcutsSection,
} from "./sections";
import { loadSettings, useSettings } from "./settings-store";

type SectionMeta = {
  id: SettingsSection;
  label: MsgKey;
  description: MsgKey;
  icon: ReactNode;
  /** 只读 core 配置的分区；`fields` 选出本分区展示的字段。 */
  fields?: (field: ConfigField) => boolean;
};

// `field.tab` 是本地化文本不能比较；按 path 前缀分派（与 TUI 同规则）。
const byPath =
  (...roots: string[]) =>
  (field: ConfigField) =>
    roots.includes(field.path[0]);

const NAV: { group: MsgKey; items: SectionMeta[] }[] = [
  {
    group: "sd.group_app",
    items: [
      {
        id: "general",
        label: "sd.general",
        description: "sd.general_desc",
        icon: <Settings2Icon />,
      },
      {
        id: "shortcuts",
        label: "sd.shortcuts",
        description: "sd.shortcuts_desc",
        icon: <CommandIcon />,
      },
    ],
  },
  {
    group: "sd.group_models",
    items: [
      {
        id: "providers",
        label: "sd.providers",
        description: "sd.providers_desc",
        icon: <ServerIcon />,
      },
      {
        id: "roles",
        label: "sd.roles",
        description: "sd.roles_desc",
        icon: <SparklesIcon />,
        fields: (field) =>
          field.path[0] === "models" && (field.path[1] === "roles" || field.path[1] === "fusion"),
      },
    ],
  },
  {
    group: "sd.group_agent",
    items: [
      {
        id: "instructions",
        label: "sd.instructions",
        description: "sd.instructions_desc",
        icon: <FileTextIcon />,
        fields: byPath("context_files", "system_prompt"),
      },
      {
        id: "skills",
        label: "sd.skills",
        description: "sd.skills_desc",
        icon: <LayersIcon />,
        fields: byPath("skills"),
      },
      {
        id: "tools",
        label: "sd.tools",
        description: "sd.tools_desc",
        icon: <WrenchIcon />,
        fields: byPath("tools", "web_fetch"),
      },
      {
        id: "sandbox",
        label: "sd.sandbox",
        description: "sd.sandbox_desc",
        icon: <ShieldIcon />,
        fields: byPath("sandbox"),
      },
      {
        id: "context",
        label: "sd.context",
        description: "sd.context_desc",
        icon: <CpuIcon />,
        fields: byPath("compaction", "limits"),
      },
    ],
  },
  {
    group: "sd.group_ext",
    items: [
      {
        id: "mcp",
        label: "sd.mcp",
        description: "sd.mcp_desc",
        icon: <PlugIcon />,
        fields: byPath("mcp"),
      },
      {
        id: "plugins",
        label: "sd.plugins",
        description: "sd.plugins_desc",
        icon: <BlocksIcon />,
        fields: (field) => field.path[0] === "plugins",
      },
      {
        id: "session",
        label: "sd.session",
        description: "sd.session_desc",
        icon: <HardDriveIcon />,
        fields: byPath("session"),
      },
      {
        id: "advanced",
        label: "sd.advanced",
        description: "sd.advanced_desc",
        icon: <SlidersHorizontalIcon />,
        fields: (field) =>
          field.advanced && field.path[0] !== "plugins" && field.path[0] !== "providers",
      },
    ],
  },
];

const SECTIONS = NAV.flatMap((group) => group.items);
const LOCAL_SECTIONS: Partial<Record<SettingsSection, true>> = { general: true, shortcuts: true };

function SaveStatus() {
  const t = useT();
  const saving = useSettings((state) => state.saving);
  const savedAt = useSettings((state) => state.savedAt);
  const [recent, setRecent] = useState(false);
  useEffect(() => {
    if (!savedAt) return;
    setRecent(true);
    const timer = window.setTimeout(() => setRecent(false), 1800);
    return () => window.clearTimeout(timer);
  }, [savedAt]);
  if (saving > 0) {
    return (
      <span className="flex items-center gap-1.5 text-xs text-text-tertiary">
        <Loader2Icon className="size-3.5 animate-spin" />
        {t("sd.saving")}
      </span>
    );
  }
  if (!recent) return null;
  return (
    <span className="flex items-center gap-1 text-xs text-success">
      <CheckIcon className="size-3.5" />
      {t("sd.saved")}
    </span>
  );
}

function ScopeSwitch({ doc }: { doc?: SettingsDocument }) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  return (
    <div className="flex items-center gap-2">
      <div className="flex rounded-lg bg-muted p-0.5">
        {(["global", "project"] as ConfigScope[]).map((item) => (
          <button
            key={item}
            type="button"
            onClick={() => useSettings.getState().setScope(item)}
            className={cn(
              "h-6 rounded-md px-2.5 text-xs transition-colors",
              scope === item
                ? "bg-background text-foreground shadow-xs"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {item === "global" ? t("sd.scope_global") : t("sd.scope_project")}
          </button>
        ))}
      </div>
      {doc && (
        <span
          className="max-w-[260px] truncate font-mono text-2xs text-text-tertiary"
          title={doc.path}
        >
          {doc.path}
        </span>
      )}
    </div>
  );
}

function SearchResults({ query, doc }: { query: string; doc: SettingsDocument }) {
  const t = useT();
  const needle = query.trim().toLowerCase();
  const groups = useMemo(() => {
    const result: { section: SectionMeta; fields: ConfigField[] }[] = [];
    for (const section of SECTIONS) {
      const matcher = section.fields;
      if (!matcher) continue;
      const fields = doc.fields.filter(
        (field) =>
          matcher(field) &&
          `${field.label} ${field.description} ${field.path.join(".")}`
            .toLowerCase()
            .includes(needle),
      );
      if (fields.length > 0) result.push({ section, fields });
    }
    return result;
  }, [doc.fields, needle]);

  if (groups.length === 0) {
    return (
      <p className="py-16 text-center text-ui text-text-tertiary">{t("sd.no_match", { query })}</p>
    );
  }
  return (
    <div className="space-y-7">
      {groups.map(({ section, fields }) => (
        <section key={section.id} className="space-y-2">
          <h3 className="text-ui font-semibold">{t(section.label)}</h3>
          <FieldList fields={fields} doc={doc} />
        </section>
      ))}
    </div>
  );
}

export function SettingsDialog() {
  const t = useT();
  const settings = useUi((state) => state.settings);
  const [section, setSection] = useState<SettingsSection>("general");
  const [query, setQuery] = useState("");
  const scope = useSettings((state) => state.scope);
  const doc = useSettings((state) => state.docs[state.scope]);
  const error = useSettings((state) => state.errors[state.scope]);
  const loading = useSettings((state) => state.loading);
  // 项目范围跟随当前会话的工作目录。
  const activeCwd = useTranscript(
    (state) => state.bySession[useTabs.getState().active ?? ""]?.info.cwd,
  );
  const coreCwd = useConnection((state) => state.config?.cwd);
  const cwd = activeCwd || coreCwd || "";

  useEffect(() => {
    if (!settings.open) return;
    setSection(settings.section ?? "general");
    setQuery("");
  }, [settings.open, settings.section]);

  useEffect(() => {
    if (settings.open && cwd) void loadSettings(cwd);
  }, [settings.open, cwd]);

  const meta = SECTIONS.find((item) => item.id === section) ?? SECTIONS[0];
  const local = Boolean(LOCAL_SECTIONS[section]) && !query;

  const body = (() => {
    if (section === "general" && !query) return <GeneralSection />;
    if (section === "shortcuts" && !query) return <ShortcutsSection />;
    if (error)
      return (
        <p className="rounded-lg bg-destructive/5 px-3 py-2 text-ui text-destructive">
          {t("sd.load_failed", { error })}
        </p>
      );
    if (!doc) {
      return (
        <div className="flex justify-center py-16 text-text-tertiary">
          {loading ? <Loader2Icon className="size-5 animate-spin" /> : t("sd.not_connected")}
        </div>
      );
    }
    if (query) return <SearchResults query={query} doc={doc} />;
    if (section === "providers") return <ProvidersSection doc={doc} />;
    if (section === "roles") return <RolesSection doc={doc} />;
    const fields = meta.fields ? doc.fields.filter(meta.fields) : [];
    return (
      <div className="space-y-6">
        {section === "plugins" && <PluginsOverview />}
        {fields.length > 0 ? (
          <FieldList fields={fields} doc={doc} />
        ) : (
          <p className="py-10 text-center text-ui text-text-tertiary">{t("sd.empty")}</p>
        )}
        {section === "session" && <MaintenanceCard />}
      </div>
    );
  })();

  return (
    <Dialog
      open={settings.open}
      onOpenChange={(open) => useUi.getState().set({ settings: { open } })}
    >
      <DialogContent className="flex h-[min(760px,88vh)] w-[min(1040px,94vw)] max-w-none gap-0 overflow-hidden p-0 sm:max-w-none">
        <nav className="flex w-[220px] shrink-0 flex-col border-r border-border-subtle bg-panel">
          <div className="p-3 pb-2">
            <label className="flex h-8 items-center gap-2 rounded-lg bg-background px-2.5 ring-1 ring-border focus-within:ring-border-strong">
              <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
              <input
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={t("sd.search_ph")}
                className="min-w-0 flex-1 bg-transparent text-ui outline-none placeholder:text-text-tertiary"
              />
            </label>
          </div>
          <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-2 pb-3">
            {NAV.map((group) => (
              <div key={group.group}>
                <div className="px-2.5 pb-1 text-2xs font-medium text-text-tertiary">
                  {t(group.group)}
                </div>
                {group.items.map((item) => (
                  <button
                    key={item.id}
                    type="button"
                    onClick={() => {
                      setSection(item.id);
                      setQuery("");
                    }}
                    className={cn(
                      "flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-ui transition-colors [&_svg]:size-4",
                      item.id === section && !query
                        ? "bg-accent font-medium text-foreground"
                        : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                    )}
                  >
                    <span className="text-text-tertiary">{item.icon}</span>
                    {t(item.label)}
                  </button>
                ))}
              </div>
            ))}
          </div>
        </nav>

        <div className="flex min-w-0 flex-1 flex-col">
          <header className="flex shrink-0 items-start justify-between gap-4 border-b border-border-subtle px-7 pt-5 pb-4 pr-14">
            <div className="min-w-0">
              <DialogTitle>{query ? t("sd.search_title") : t(meta.label)}</DialogTitle>
              <DialogDescription className="mt-1 text-xs leading-5">
                {query
                  ? t("sd.search_scope", {
                      scope: t(scope === "global" ? "sd.scope_global" : "sd.scope_project_short"),
                    })
                  : t(meta.description)}
              </DialogDescription>
            </div>
            <div className="flex shrink-0 items-center gap-3 pt-0.5">
              <SaveStatus />
              {!local && <ScopeSwitch doc={doc} />}
            </div>
          </header>
          <div className="min-h-0 flex-1 overflow-y-auto px-7 py-5">{body}</div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
