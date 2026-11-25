// 设置里需要定制界面的分区：通用（webui 前端偏好）、快捷键、模型角色、插件、会话存储维护。

import { Loader2Icon, MonitorIcon, MoonIcon, RefreshCwIcon, SunIcon } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { toast } from "sonner";
import { Kbd } from "@/components/common/icon-button";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useT } from "@/lib/i18n";
import { groupByProvider, modelName, parseSelector } from "@/lib/models";
import { SHORTCUTS, shortcutLabel } from "@/lib/shortcuts";
import { type DetailLevel, LEVEL_LABEL } from "@/lib/tool-view";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  ConfigAction,
  ConfigField,
  ConfigReply,
  MaintenanceReport,
  PluginInfo,
  PluginListResult,
  SettingsDocument,
} from "@/protocol/types";
import { selectModels, useConnection } from "@/store/connection";
import { refreshSideData } from "@/store/lifecycle";
import { type Theme, usePrefs } from "@/store/prefs";
import { allSessions, useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { FieldRow, fieldValues, SettingRow } from "./field";
import { useSettings, writeField } from "./settings-store";

const INHERIT = "__inherit__";

const DETAIL_OPTIONS = (["hidden", "brief", "full"] as const).map((value) => ({
  value,
  label: LEVEL_LABEL[value],
}));
const TOOL_DETAIL_OPTIONS = DETAIL_OPTIONS.filter((option) => option.value !== "full");

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "divide-y divide-border-subtle rounded-xl bg-card px-4 ring-1 ring-inset ring-border-subtle",
        className,
      )}
    >
      {children}
    </div>
  );
}

function Segmented<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string; icon?: ReactNode }[];
  onChange: (value: T) => void;
}) {
  return (
    <div className="flex w-fit max-w-full flex-wrap gap-0.5 md:ml-auto">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          onClick={() => {
            if (option.value !== value) onChange(option.value);
          }}
          className={cn(
            "inline-flex h-7 items-center gap-1.5 rounded-md px-2.5 text-xs transition-colors",
            value === option.value
              ? "bg-accent text-foreground"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {option.icon}
          {option.label}
        </button>
      ))}
    </div>
  );
}

// ---------- 通用 ----------

export function GeneralSection() {
  const t = useT();
  const prefs = usePrefs();
  const server = useConnection((state) => state.server);
  const cwd = useConnection((state) => state.config?.cwd);
  const globalDoc = useSettings((state) => state.docs.global);
  const langField = globalDoc?.fields.find(
    (field) => field.path.length === 1 && field.path[0] === "language",
  );
  const langValues = globalDoc ? fieldValues(globalDoc, ["language"]) : null;
  const langCurrent = String(langValues?.explicit ?? langValues?.fallback ?? "auto") || "auto";

  const toggleNotify = async (on: boolean) => {
    if (!on) {
      prefs.set({ notify: false });
      return;
    }
    if (typeof Notification === "undefined") {
      toast.error(t("ss.notify_unsupported"));
      return;
    }
    const permission =
      Notification.permission === "default"
        ? await Notification.requestPermission()
        : Notification.permission;
    if (permission !== "granted") {
      toast.error(t("ss.notify_denied"));
      return;
    }
    prefs.set({ notify: true });
  };

  return (
    <div className="space-y-6">
      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">{t("ss.appearance")}</h3>
        <Card>
          <SettingRow label={t("ss.theme")} description={t("ss.theme_desc")}>
            <Segmented<Theme>
              value={prefs.theme}
              onChange={(theme) => prefs.set({ theme })}
              options={[
                {
                  value: "system",
                  label: t("ss.theme_system"),
                  icon: <MonitorIcon className="size-3.5" />,
                },
                {
                  value: "light",
                  label: t("ss.theme_light"),
                  icon: <SunIcon className="size-3.5" />,
                },
                {
                  value: "dark",
                  label: t("ss.theme_dark"),
                  icon: <MoonIcon className="size-3.5" />,
                },
              ]}
            />
          </SettingRow>
          {globalDoc && langField && (
            <SettingRow
              label={langField.label || t("ss.language")}
              description={langField.description || t("ss.language_desc")}
            >
              <Segmented
                value={langCurrent}
                onChange={(value) => void writeField(["language"], value, "global")}
                options={[
                  { value: "auto", label: t("common.auto") },
                  { value: "zh", label: "中文" },
                  { value: "en", label: "English" },
                ]}
              />
            </SettingRow>
          )}
          <SettingRow label={t("ss.thinking")} description={t("ss.thinking_desc")}>
            <Segmented<DetailLevel>
              value={prefs.thinkingDetail}
              onChange={(thinkingDetail) => prefs.set({ thinkingDetail })}
              options={DETAIL_OPTIONS.map((option) => ({ ...option, label: t(option.label) }))}
            />
          </SettingRow>
          <SettingRow label={t("ss.tool_display")} description={t("ss.tool_display_desc")}>
            <Segmented<DetailLevel>
              value={prefs.toolDetail}
              onChange={(toolDetail) => prefs.set({ toolDetail })}
              options={TOOL_DETAIL_OPTIONS.map((option) => ({ ...option, label: t(option.label) }))}
            />
          </SettingRow>
          <SettingRow label={t("ss.widgets")} description={t("ss.widgets_desc")}>
            <div className="flex justify-end">
              <Switch
                aria-label={t("ss.widgets")}
                checked={prefs.widgets}
                onCheckedChange={(widgets) => prefs.set({ widgets })}
              />
            </div>
          </SettingRow>
        </Card>
      </section>

      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">{t("ss.interaction")}</h3>
        <Card>
          <SettingRow label={t("ss.running_enter")} description={t("ss.running_enter_desc")}>
            <Segmented
              value={prefs.runningEnter}
              onChange={(runningEnter) => prefs.set({ runningEnter })}
              options={[
                { value: "steer", label: t("q.steer") },
                { value: "interrupt", label: t("ss.interrupt") },
              ]}
            />
          </SettingRow>
          <SettingRow label={t("ss.queue_on_enter")} description={t("ss.queue_on_enter_desc")}>
            <div className="flex justify-end">
              <Switch
                checked={prefs.queueOnEnter}
                onCheckedChange={(queueOnEnter) => prefs.set({ queueOnEnter })}
              />
            </div>
          </SettingRow>
          <SettingRow label={t("ss.notify")} description={t("ss.notify_desc")}>
            <div className="flex justify-end">
              <Switch
                checked={prefs.notify}
                onCheckedChange={(value) => void toggleNotify(value)}
              />
            </div>
          </SettingRow>
        </Card>
      </section>

      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">{t("ss.about")}</h3>
        <Card className="py-1">
          <InfoRow
            label={t("ss.server")}
            value={
              server ? `${server.server.name} ${server.server.version}` : t("sd.not_connected")
            }
          />
          <InfoRow label={t("ss.protocol")} value={server?.protocol_version ?? "—"} />
          <InfoRow label={t("ss.cwd")} value={cwd ?? "—"} mono />
        </Card>
      </section>
    </div>
  );
}

function InfoRow({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-center justify-between gap-6 py-2.5 text-ui">
      <span className="shrink-0 text-muted-foreground">{label}</span>
      <span
        className={cn("min-w-0 truncate text-foreground", mono && "font-mono text-xs")}
        title={value}
      >
        {value}
      </span>
    </div>
  );
}

// ---------- 快捷键 ----------

export function ShortcutsSection() {
  const t = useT();
  const queueOnEnter = usePrefs((state) => state.queueOnEnter);
  return (
    <Card className="py-1">
      {SHORTCUTS.map((item) => (
        <div key={item.keys} className="flex items-center justify-between py-2.5 text-ui">
          <span className="text-foreground">{t(shortcutLabel(item.label, queueOnEnter))}</span>
          <span className="flex gap-1">
            {item.keys.split(" ").map((key) => (
              <Kbd key={key} className="h-5 min-w-5 px-1.5 text-2xs">
                {key}
              </Kbd>
            ))}
          </span>
        </div>
      ))}
    </Card>
  );
}

// ---------- 模型角色 ----------

/** 模型 selector 编辑器：模型下拉 + 思考强度下拉，写成 `provider/model[:level]`。 */
function ModelSelectorRow({ field, doc }: { field: ConfigField; doc: SettingsDocument }) {
  const t = useT();
  const models = useConnection(selectModels);
  const { explicit, fallback } = fieldValues(doc, field.path);
  const selector = typeof explicit === "string" ? explicit : "";
  const parsed = parseSelector(selector, models);
  const fallbackText = typeof fallback === "string" && fallback ? fallback : "";
  const levels = parsed.model?.thinking_levels ?? [];
  const inherited = selector === "inherit";
  const explorer = field.path[1] === "roles" && field.path[2] === "explore";
  const unknown = selector && !parsed.model && !inherited && !(explorer && selector === "none");

  let fallbackLabel = t("common.default");
  if (fallbackText === "inherit") fallbackLabel = t("ss.inherit_parent");
  else if (explorer && fallbackText === "none") fallbackLabel = t("ss.inherit_none_explorer");
  else if (fallbackText) fallbackLabel = t("ss.default_model", { model: modelName(fallbackText) });

  const write = (id: string, level?: string) =>
    void writeField(field.path, id ? (level ? `${id}:${level}` : id) : undefined);

  return (
    <SettingRow
      label={field.label}
      description={field.description}
      title={field.path.join(".")}
      overridden={explicit !== undefined}
      onReset={() => write("")}
    >
      <div className="flex gap-1.5">
        <Select
          value={selector ? parsed.id : INHERIT}
          onValueChange={(value) => write(value === INHERIT ? "" : value)}
        >
          <SelectTrigger size="sm" className="h-8 min-w-0 flex-1 rounded-lg text-ui shadow-none">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT}>{fallbackLabel}</SelectItem>
            {explorer && (
              <>
                <SelectItem value="inherit">{t("ss.use_parent")}</SelectItem>
                <SelectItem value="none">{t("ss.disable_explorer")}</SelectItem>
              </>
            )}
            {unknown && (
              <SelectItem value={parsed.id}>{t("ss.not_found", { id: parsed.id })}</SelectItem>
            )}
            {groupByProvider(models).map(([provider, list]) => (
              <SelectGroup key={provider}>
                <SelectLabel className="text-2xs">{provider}</SelectLabel>
                {list.map((model) => (
                  <SelectItem key={model.id} value={model.id}>
                    {modelName(model.id)}
                  </SelectItem>
                ))}
              </SelectGroup>
            ))}
          </SelectContent>
        </Select>
        {levels.length > 0 && (
          <Select
            value={parsed.level ?? INHERIT}
            onValueChange={(value) => write(parsed.id, value === INHERIT ? undefined : value)}
          >
            <SelectTrigger
              size="sm"
              className="h-8 w-[84px] shrink-0 rounded-lg text-ui shadow-none"
              aria-label={t("mp.level")}
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={INHERIT}>{t("common.default")}</SelectItem>
              {levels.map((level) => (
                <SelectItem key={level} value={level}>
                  {level}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
      </div>
    </SettingRow>
  );
}

export function RolesSection({ doc }: { doc: SettingsDocument }) {
  const roles = doc.fields.filter(isRoleField);
  return (
    <Card>
      {roles.map((field) =>
        field.path[1] === "recent" ? (
          <FieldRow key={field.path.join(".")} field={field} doc={doc} />
        ) : (
          <ModelSelectorRow key={field.path.join(".")} field={field} doc={doc} />
        ),
      )}
    </Card>
  );
}

/** 「模型」分页的字段：角色与 Fusion 的 selector，以及选择器置顶的最近使用条数。 */
export function isRoleField(field: ConfigField): boolean {
  return (
    field.path[0] === "models" &&
    (field.path[1] === "roles" || field.path[1] === "fusion" || field.path[1] === "recent")
  );
}

// ---------- 插件 ----------

export function PluginsOverview() {
  const t = useT();
  const active = useTabs((state) => state.active);
  // plugin/list 按会话 cwd 发现插件，core 只认已存在的会话：草稿时退回最近一次会话。
  const latest = useSessions((state) => allSessions(state)[0]?.id);
  const sessionId = active && active !== "draft-session" ? active : (latest ?? "");
  // 直接打开 /plugins 时握手可能还没完成，等连上再读，避免「连接已关闭」。
  const online = useConnection((state) => state.status === "online");
  const [plugins, setPlugins] = useState<PluginInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async (reload: boolean) => {
    if (!sessionId) {
      setPlugins(null);
      return;
    }
    if (!online) return;
    setBusy(true);
    setError(null);
    try {
      if (reload) {
        await rpc.call("plugin/reload", { session_id: sessionId });
        // 视图里缓存的 plugin/list（斜杠命令、故障提示）也要刷新。
        if (sessionId) void refreshSideData(sessionId);
      }
      const result = await rpc.call<PluginListResult>("plugin/list", { session_id: sessionId });
      setPlugins(result.plugins);
      if (reload) toast.success(t("slash.plugins_reloaded"));
    } catch (loadError) {
      setError(errorText(loadError));
    } finally {
      setBusy(false);
    }
  };

  // biome-ignore lint/correctness/useExhaustiveDependencies: 进入分区或切换会话时读取一次
  useEffect(() => {
    void load(false);
  }, [sessionId, online]);

  return (
    <section className="space-y-2">
      <div className="flex items-center justify-between">
        <h3 className="text-xs font-medium text-text-tertiary">
          {t("ss.plugins_loaded", { n: plugins?.length ?? 0 })}
        </h3>
        <Button
          size="xs"
          variant="outline"
          disabled={busy || !sessionId}
          onClick={() => void load(true)}
        >
          {busy ? <Loader2Icon className="animate-spin" /> : <RefreshCwIcon />}
          {t("ss.reload")}
        </Button>
      </div>
      {error && <p className="text-xs text-destructive">{error}</p>}
      <Card>
        {!sessionId && (
          <p className="py-6 text-center text-ui text-text-tertiary">
            {t("ss.plugins_no_session")}
          </p>
        )}
        {plugins?.length === 0 && (
          <p className="py-6 text-center text-ui text-text-tertiary">{t("ss.no_plugins")}</p>
        )}
        {plugins?.map((plugin) => (
          <div key={`${plugin.source}:${plugin.name}`} className="space-y-1.5 py-3">
            <div className="flex items-center gap-2">
              <span
                className={cn(
                  "size-1.5 shrink-0 rounded-full",
                  plugin.error ? "bg-destructive" : "bg-success",
                )}
              />
              <span className="text-sm font-medium">{plugin.name}</span>
              {plugin.version && (
                <span className="font-mono text-2xs text-text-tertiary">{plugin.version}</span>
              )}
              <span className="ml-auto shrink-0 text-2xs text-text-tertiary">{plugin.source}</span>
            </div>
            {plugin.description && (
              <p className="pl-3.5 text-xs leading-5 text-muted-foreground">{plugin.description}</p>
            )}
            {plugin.error && (
              <p className="pl-3.5 text-xs leading-5 break-words text-destructive">
                {plugin.error}
              </p>
            )}
            {((plugin.tools?.length ?? 0) > 0 || (plugin.commands?.length ?? 0) > 0) && (
              <div className="flex flex-wrap gap-1 pl-3.5">
                {(plugin.tools ?? []).map((tool) => (
                  <span
                    key={tool.name}
                    title={tool.description}
                    className="rounded bg-muted px-1.5 py-px font-mono text-2xs text-muted-foreground"
                  >
                    {tool.name}
                  </span>
                ))}
                {(plugin.commands ?? []).map((command) => (
                  <Badge
                    key={command.name}
                    variant="brand"
                    title={command.description}
                    className="font-mono font-normal"
                  >
                    /{command.name}
                  </Badge>
                ))}
              </div>
            )}
          </div>
        ))}
      </Card>
    </section>
  );
}

// ---------- 会话存储维护 ----------

export function MaintenanceCard() {
  const t = useT();
  const [report, setReport] = useState<MaintenanceReport | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (apply: boolean) => {
    setBusy(true);
    try {
      const action: ConfigAction = { action: "maintenance", apply };
      const reply = await rpc.call<ConfigReply>("config/manage", action);
      if (reply.kind !== "maintenance") throw new Error(t("ss.maint_bad_reply"));
      setReport(reply.data);
      if (apply)
        toast.success(
          t("ss.maint_done", {
            n: reply.data.removed.length,
            bytes: formatBytes(reply.data.bytes),
          }),
        );
    } catch (error) {
      toast.error(t("ss.maint_failed", { error: errorText(error) }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card>
      <SettingRow
        label={t("ss.maint_label")}
        description={
          report && !report.applied
            ? t("ss.maint_preview", {
                n: report.removed.length,
                bytes: formatBytes(report.bytes),
                protected: report.protected,
              })
            : t("ss.maint_desc")
        }
      >
        <div className="flex justify-end gap-1.5">
          <Button size="xs" variant="outline" disabled={busy} onClick={() => void run(false)}>
            {busy && !report && <Loader2Icon className="animate-spin" />}
            {t("ss.maint_check")}
          </Button>
          <Button
            size="xs"
            variant="destructive"
            disabled={busy || !report || report.applied || report.removed.length === 0}
            onClick={() => void run(true)}
          >
            {t("ss.maint_run")}
          </Button>
        </div>
      </SettingRow>
    </Card>
  );
}
