// 设置：左侧分类导航 + 右侧分区内容；改动即保存，顶栏显示保存状态。
// webui 偏好（通用、快捷键）与 core 配置（其余分区，分全局 / 项目两个范围）分开。

import {
  BlocksIcon,
  CheckIcon,
  CommandIcon,
  CpuIcon,
  FileTextIcon,
  HardDriveIcon,
  ImageIcon,
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
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { ImagePlaygroundSettingsSection } from "@/components/image-playground/playground-settings";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { type MsgKey, useI18n, useT } from "@/lib/i18n";
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
  isRoleField,
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
        fields: byPath("general"),
      },
      {
        id: "shortcuts",
        label: "sd.shortcuts",
        description: "sd.shortcuts_desc",
        icon: <CommandIcon />,
      },
      {
        id: "image_playground",
        label: "sd.image_playground",
        description: "sd.image_playground_desc",
        icon: <ImageIcon />,
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
        fields: byPath("providers", "defaults"),
      },
      {
        id: "roles",
        label: "sd.roles",
        description: "sd.roles_desc",
        icon: <SparklesIcon />,
        fields: isRoleField,
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
        fields: byPath("context_files", "system_prompt", "git"),
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
const LOCAL_SECTIONS: Partial<Record<SettingsSection, true>> = {
  general: true,
  shortcuts: true,
  image_playground: true,
};

export function SaveStatus() {
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
  // 固定槽宽避免保存态显隐时把右侧范围切换按钮挤得左右跳动。
  return (
    <span className="inline-flex min-w-[60px] shrink-0 items-center justify-end gap-1 text-xs">
      {saving > 0 ? (
        <span className="flex items-center gap-1.5 text-text-tertiary">
          <Loader2Icon className="size-3.5 animate-spin" />
          {t("sd.saving")}
        </span>
      ) : recent ? (
        <span className="flex items-center gap-1 text-success">
          <CheckIcon className="size-3.5" />
          {t("sd.saved")}
        </span>
      ) : null}
    </span>
  );
}

export function ScopeSwitch({ doc }: { doc?: SettingsDocument }) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  return (
    <div className="flex min-w-0 items-center gap-2">
      <div className="flex shrink-0 gap-0.5">
        {(["global", "project"] as ConfigScope[]).map((item) => (
          <button
            key={item}
            type="button"
            onClick={() => useSettings.getState().setScope(item)}
            className={cn(
              "h-7 rounded-lg px-2.5 text-xs transition-colors",
              scope === item
                ? "bg-accent text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {item === "global" ? t("sd.scope_global") : t("sd.scope_project")}
          </button>
        ))}
      </div>
      {doc && (
        <span
          className="min-w-0 max-w-[260px] truncate font-mono text-2xs text-text-tertiary"
          title={doc.path}
        >
          {doc.path}
        </span>
      )}
    </div>
  );
}

function SearchResults({
  query,
  doc,
  onJumpProvider,
}: {
  query: string;
  doc: SettingsDocument;
  onJumpProvider: (providerId: string, modelId?: string) => void;
}) {
  const t = useT();
  const needle = query.trim().toLowerCase();

  const providerHits = useMemo(() => {
    const hits: { providerId: string; modelId?: string }[] = [];
    const effective =
      typeof doc.effective === "object" && doc.effective !== null
        ? (doc.effective as Record<string, unknown>)
        : null;
    const root = effective?.providers;
    if (typeof root === "object" && root !== null && !Array.isArray(root)) {
      for (const [pid, pval] of Object.entries(root as Record<string, unknown>).toSorted(
        ([a], [b]) => a.localeCompare(b),
      )) {
        if (pid.toLowerCase().includes(needle)) {
          hits.push({ providerId: pid });
        }
        const models =
          typeof pval === "object" && pval !== null && !Array.isArray(pval)
            ? (pval as Record<string, unknown>).models
            : null;
        if (typeof models === "object" && models !== null && !Array.isArray(models)) {
          for (const mid of Object.keys(models).toSorted()) {
            if (
              mid.toLowerCase().includes(needle) ||
              `${pid}/${mid}`.toLowerCase().includes(needle)
            ) {
              hits.push({ providerId: pid, modelId: mid });
            }
          }
        }
      }
    }
    return hits.slice(0, 24);
  }, [doc.effective, needle]);

  const groups = useMemo(() => {
    const result: { section: SectionMeta; fields: ConfigField[] }[] = [];
    for (const section of SECTIONS) {
      const matcher = section.fields;
      if (!matcher) continue;
      const fields = doc.fields
        .filter(
          (field) =>
            matcher(field) &&
            `${field.label} ${field.description} ${field.path.join(".")}`
              .toLowerCase()
              .includes(needle),
        )
        .slice(0, 40);
      if (fields.length > 0) result.push({ section, fields });
    }
    return result;
  }, [doc.fields, needle]);

  if (providerHits.length === 0 && groups.length === 0) {
    return (
      <p className="py-16 text-center text-ui text-text-tertiary">{t("sd.no_match", { query })}</p>
    );
  }
  return (
    <div className="space-y-7">
      {providerHits.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-ui font-semibold">Providers & Models</h3>
          <div className="divide-y divide-border-subtle overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle">
            {providerHits.map((hit) => (
              <button
                key={`${hit.providerId}/${hit.modelId ?? ""}`}
                type="button"
                onClick={() => onJumpProvider(hit.providerId, hit.modelId)}
                className="flex h-10 w-full items-center justify-between gap-3 px-3.5 text-left text-ui transition-colors hover:bg-surface-hover"
              >
                <span className="min-w-0 truncate font-mono text-xs text-foreground">
                  {hit.modelId ? `${hit.providerId} / ${hit.modelId}` : hit.providerId}
                </span>
                <span className="shrink-0 text-xs text-brand">{t("sp.open_in_providers")}</span>
              </button>
            ))}
          </div>
        </section>
      )}
      {groups.map(({ section, fields }) => (
        <section key={section.id} className="space-y-2">
          <h3 className="text-ui font-semibold">{t(section.label)}</h3>
          <FieldList fields={fields} doc={doc} includeAdvanced />
        </section>
      ))}
    </div>
  );
}

export function SettingsDialog() {
  const t = useT();
  const locale = useI18n((state) => state.locale);
  const settings = useUi((state) => state.settings);
  const [section, setSection] = useState<SettingsSection>("general");
  const [query, setQuery] = useState("");
  const returnFocus = useRef<HTMLElement | null>(null);
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
    void locale;
    if (settings.open && cwd) void loadSettings(cwd);
  }, [settings.open, cwd, locale]);

  const meta = SECTIONS.find((item) => item.id === section) ?? SECTIONS[0];
  const local = Boolean(LOCAL_SECTIONS[section]) && !query;

  const body = (() => {
    if (section === "general" && !query) return <GeneralSection />;
    if (section === "shortcuts" && !query) return <ShortcutsSection />;
    if (section === "image_playground" && !query) return <ImagePlaygroundSettingsSection />;
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
    if (query) {
      return (
        <SearchResults
          query={query}
          doc={doc}
          onJumpProvider={(providerId, modelId) => {
            useSettings.getState().focusProvider(providerId, modelId);
            setSection("providers");
            setQuery("");
          }}
        />
      );
    }
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
      <DialogContent
        className="flex h-[min(760px,88vh)] w-[min(1040px,94vw)] max-w-none flex-col gap-0 overflow-hidden p-0 sm:max-w-none md:flex-row"
        onOpenAutoFocus={() => {
          returnFocus.current =
            document.activeElement instanceof HTMLElement ? document.activeElement : null;
        }}
        onCloseAutoFocus={(event) => {
          const target = returnFocus.current;
          returnFocus.current = null;
          if (!target?.isConnected) return;
          event.preventDefault();
          target.focus();
        }}
        onPointerDownOutside={(event) => {
          if (event.target instanceof Element && event.target.closest("[data-sonner-toaster]")) {
            event.preventDefault();
          }
        }}
        onInteractOutside={(event) => {
          if (event.target instanceof Element && event.target.closest("[data-sonner-toaster]")) {
            event.preventDefault();
          }
        }}
      >
        <nav className="flex min-h-0 w-full shrink-0 flex-col border-b border-border-subtle bg-panel md:w-[220px] md:border-r md:border-b-0">
          <div className="p-3 pr-14 pb-2 md:pr-3">
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
          <div className="flex min-h-0 gap-2 overflow-x-auto px-2 pb-3 md:block md:flex-1 md:space-y-4 md:overflow-y-auto">
            {NAV.map((group) => (
              <div key={group.group} className="flex shrink-0 md:block">
                <div className="hidden px-2.5 pb-1 text-2xs font-medium text-text-tertiary md:block">
                  {t(group.group)}
                </div>
                {group.items.map((item) => (
                  <button
                    key={item.id}
                    type="button"
                    aria-current={item.id === section && !query ? "page" : undefined}
                    onClick={() => {
                      setSection(item.id);
                      setQuery("");
                    }}
                    className={cn(
                      "flex h-8 shrink-0 items-center gap-2.5 rounded-lg px-2.5 text-ui whitespace-nowrap transition-colors md:w-full [&_svg]:size-4",
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

        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <header className="flex shrink-0 flex-col items-start justify-between gap-2.5 border-b border-border-subtle px-4 pt-4 pb-3 lg:flex-row lg:gap-4 md:px-7 md:pt-5 md:pr-14 md:pb-4">
            <div className="min-w-0 flex-1">
              <DialogTitle>{query ? t("sd.search_title") : t(meta.label)}</DialogTitle>
              <DialogDescription className="mt-1 text-xs leading-5">
                {query
                  ? t("sd.search_scope", {
                      scope: t(scope === "global" ? "sd.scope_global" : "sd.scope_project_short"),
                    })
                  : t(meta.description)}
              </DialogDescription>
            </div>
            <div className="flex min-w-0 max-w-full items-center gap-3 pt-0.5 lg:shrink-0">
              <SaveStatus />
              {!local && <ScopeSwitch doc={doc} />}
            </div>
          </header>
          <div className="min-h-0 min-w-0 flex-1 overflow-y-auto px-4 py-4 [overflow-wrap:anywhere] md:px-7 md:py-5">
            {body}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
