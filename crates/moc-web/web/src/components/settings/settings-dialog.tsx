// 设置：左侧分类导航 + 右侧分区内容；改动即保存，顶栏显示保存状态。
// 浏览器本地偏好（通用、快捷键）与 core 配置（其余分区，分全局 / 项目两个范围）分开。

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
  label: string;
  description: string;
  icon: ReactNode;
  /** 只读 core 配置的分区；`fields` 选出本分区展示的字段。 */
  fields?: (field: ConfigField) => boolean;
};

const byTab = (tab: string) => (field: ConfigField) => field.tab === tab;

const NAV: { group: string; items: SectionMeta[] }[] = [
  {
    group: "应用",
    items: [
      {
        id: "general",
        label: "通用",
        description: "外观、交互与通知；只保存在当前浏览器。",
        icon: <Settings2Icon />,
      },
      {
        id: "shortcuts",
        label: "快捷键",
        description: "全局快捷键与输入框按键。",
        icon: <CommandIcon />,
      },
    ],
  },
  {
    group: "模型",
    items: [
      {
        id: "providers",
        label: "供应商与模型",
        description: "接入模型服务，拉取模型列表，按模型编辑上下文长度、思考与输入模态。",
        icon: <ServerIcon />,
      },
      {
        id: "roles",
        label: "模型角色",
        description: "主对话、探索子代理与计划分别使用的模型。",
        icon: <SparklesIcon />,
        fields: (field) =>
          field.path[0] === "models" && (field.path[1] === "roles" || field.path[1] === "fusion"),
      },
    ],
  },
  {
    group: "Agent",
    items: [
      {
        id: "instructions",
        label: "指令",
        description: "AGENTS.md 等指令文件与系统提示。",
        icon: <FileTextIcon />,
        fields: byTab("指令"),
      },
      {
        id: "skills",
        label: "技能",
        description: "技能目录的发现与预算。",
        icon: <LayersIcon />,
        fields: byTab("技能"),
      },
      {
        id: "tools",
        label: "工具",
        description: "内置工具的超时、输出与资源上限。",
        icon: <WrenchIcon />,
        fields: byTab("工具"),
      },
      {
        id: "sandbox",
        label: "沙盒",
        description: "命令执行的默认隔离级别与路径授权。",
        icon: <ShieldIcon />,
        fields: byTab("沙盒"),
      },
      {
        id: "context",
        label: "上下文",
        description: "压缩策略与请求限制。",
        icon: <CpuIcon />,
        fields: byTab("上下文"),
      },
    ],
  },
  {
    group: "扩展与系统",
    items: [
      {
        id: "mcp",
        label: "MCP",
        description: "MCP 服务器的加载与调用限制。",
        icon: <PlugIcon />,
        fields: byTab("MCP"),
      },
      {
        id: "plugins",
        label: "插件",
        description: "当前工作区加载的插件，以及插件宿主的限制。",
        icon: <BlocksIcon />,
        fields: (field) => field.path[0] === "plugins",
      },
      {
        id: "session",
        label: "会话存储",
        description: "会话日志的位置、压缩与保留。",
        icon: <HardDriveIcon />,
        fields: byTab("会话"),
      },
      {
        id: "advanced",
        label: "高级",
        description: "不常调整的选项。",
        icon: <SlidersHorizontalIcon />,
        fields: (field) => field.tab === "高级" && field.path[0] !== "plugins",
      },
    ],
  },
];

const SECTIONS = NAV.flatMap((group) => group.items);
const LOCAL_SECTIONS: Partial<Record<SettingsSection, true>> = { general: true, shortcuts: true };

function SaveStatus() {
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
        保存中
      </span>
    );
  }
  if (!recent) return null;
  return (
    <span className="flex items-center gap-1 text-xs text-success">
      <CheckIcon className="size-3.5" />
      已保存
    </span>
  );
}

function ScopeSwitch({ doc }: { doc?: SettingsDocument }) {
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
            {item === "global" ? "全局" : "当前项目"}
          </button>
        ))}
      </div>
      {doc && (
        <span
          className="max-w-[260px] truncate font-mono text-[11px] text-text-tertiary"
          title={doc.path}
        >
          {doc.path}
        </span>
      )}
    </div>
  );
}

function SearchResults({ query, doc }: { query: string; doc: SettingsDocument }) {
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
      <p className="py-16 text-center text-[13px] text-text-tertiary">
        没有匹配「{query}」的设置项
      </p>
    );
  }
  return (
    <div className="space-y-7">
      {groups.map(({ section, fields }) => (
        <section key={section.id} className="space-y-2">
          <h3 className="text-[13px] font-semibold">{section.label}</h3>
          <FieldList fields={fields} doc={doc} />
        </section>
      ))}
    </div>
  );
}

export function SettingsDialog() {
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
        <p className="rounded-lg bg-destructive/5 px-3 py-2 text-[13px] text-destructive">
          读取配置失败：{error}
        </p>
      );
    if (!doc) {
      return (
        <div className="flex justify-center py-16 text-text-tertiary">
          {loading ? <Loader2Icon className="size-5 animate-spin" /> : "尚未连接"}
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
          <p className="py-10 text-center text-[13px] text-text-tertiary">
            这个分区没有可编辑的项。
          </p>
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
                placeholder="搜索设置"
                className="min-w-0 flex-1 bg-transparent text-[13px] outline-none placeholder:text-text-tertiary"
              />
            </label>
          </div>
          <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-2 pb-3">
            {NAV.map((group) => (
              <div key={group.group}>
                <div className="px-2.5 pb-1 text-[11px] font-medium text-text-tertiary">
                  {group.group}
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
                      "flex h-8 w-full items-center gap-2.5 rounded-lg px-2.5 text-[13px] transition-colors [&_svg]:size-4",
                      item.id === section && !query
                        ? "bg-accent font-medium text-foreground"
                        : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                    )}
                  >
                    <span className="text-text-tertiary">{item.icon}</span>
                    {item.label}
                  </button>
                ))}
              </div>
            ))}
          </div>
        </nav>

        <div className="flex min-w-0 flex-1 flex-col">
          <header className="flex shrink-0 items-start justify-between gap-4 border-b border-border-subtle px-7 pt-5 pb-4 pr-14">
            <div className="min-w-0">
              <DialogTitle className="text-[17px] font-semibold">
                {query ? "搜索结果" : meta.label}
              </DialogTitle>
              <DialogDescription className="mt-1 text-[12.5px] text-muted-foreground">
                {query ? `在${scope === "global" ? "全局" : "项目"}配置中搜索` : meta.description}
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
