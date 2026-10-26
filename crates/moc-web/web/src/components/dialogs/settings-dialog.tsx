// 设置：左侧导航「界面」（前端偏好）+ config/manage 返回的字段分页 + 「维护」。
// 编辑写进 document 的副本，保存时整份提交并带 revision；冲突或结构错误原样显示 core 的 message。

import { ChevronRight, Eye, EyeOff } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { choiceDisplay, formatField, getPath, parseField, setPath } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type {
  CatalogModel,
  ConfigField,
  ConfigReply,
  ConfigScope,
  MaintenanceReport,
  SettingsDocument,
} from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { type DetailTier, type RunningEnter, type Theme, usePrefs } from "@/store/prefs";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

const UI_TAB = "界面";
const MAINTENANCE_TAB = "维护";

const THEME_LABEL: Record<Theme, string> = { system: "跟随系统", light: "亮色", dark: "暗色" };
const TIER_LABEL: Record<DetailTier, string> = { hidden: "隐藏", brief: "简要", full: "完整" };
const ENTER_LABEL: Record<RunningEnter, string> = {
  steer: "插队补充（steer）",
  interrupt: "停止后发送",
};

function errorText(error: unknown): string {
  return error instanceof RpcError ? error.message : String(error);
}

async function manage(params: unknown): Promise<ConfigReply> {
  return rpc.call<ConfigReply>("config/manage", params);
}

type Json = Record<string, unknown>;

function useScopeCwd(): string {
  const active = useTabs((state) => state.active);
  const sessionCwd = useTranscript((state) =>
    active ? state.bySession[active]?.info.cwd : undefined,
  );
  const configCwd = useConnection((state) => state.config?.cwd ?? "");
  return sessionCwd ?? configCwd;
}

export function SettingsDialog() {
  const state = useUi((s) => s.settings);
  const set = useUi((s) => s.set);
  const cwd = useScopeCwd();
  const [scope, setScope] = useState<ConfigScope>("global");
  const [settings, setSettings] = useState<SettingsDocument | null>(null);
  const [document, setDocument] = useState<Json>({});
  const [dirty, setDirty] = useState(false);
  const [invalid, setInvalid] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [tab, setTab] = useState(UI_TAB);

  const load = useCallback(async () => {
    setError(null);
    try {
      const reply = await manage({ action: "read", scope, cwd });
      if (reply.kind !== "settings") throw new Error(`意外的回复：${reply.kind}`);
      setSettings(reply.data);
      setDocument(reply.data.document ?? {});
      setDirty(false);
      setInvalid({});
    } catch (err) {
      setError(errorText(err));
    }
  }, [scope, cwd]);

  useEffect(() => {
    if (state.open) void load();
  }, [state.open, load]);

  useEffect(() => {
    if (state.open) setTab(state.tab ?? UI_TAB);
  }, [state.open, state.tab]);

  const tabs = useMemo(() => {
    const seen: string[] = [];
    for (const field of settings?.fields ?? []) {
      if (!seen.includes(field.tab)) seen.push(field.tab);
    }
    return seen;
  }, [settings]);

  const close = () => set({ settings: { open: false } });

  const updateField = (field: ConfigField, value: unknown) => {
    setDocument((current) => setPath(current, field.path, value));
    setDirty(true);
  };

  const markInvalid = (key: string, message: string | null) =>
    setInvalid((current) => {
      const next = { ...current };
      if (message) next[key] = message;
      else delete next[key];
      return next;
    });

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    setError(null);
    try {
      await manage({ action: "save", scope, cwd, document, revision: settings.revision });
      toast.success("设置已保存");
      await load();
      await useConnection
        .getState()
        .refreshConfig()
        .catch(() => undefined);
    } catch (err) {
      setError(errorText(err));
      toast.error(`保存失败：${errorText(err)}`, {
        action: { label: "重新载入", onClick: () => void load() },
      });
    } finally {
      setSaving(false);
    }
  };

  const configTab = tab !== UI_TAB && tab !== MAINTENANCE_TAB;

  return (
    <Dialog open={state.open} onOpenChange={(open) => !open && close()}>
      <DialogContent className="flex h-[80vh] flex-col gap-3 sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>设置</DialogTitle>
          <DialogDescription className="truncate">
            {settings ? settings.path : "界面偏好与 moc 配置"}
          </DialogDescription>
        </DialogHeader>
        <div className="flex min-h-0 flex-1 gap-4">
          <nav aria-label="设置分页" className="w-40 shrink-0 space-y-0.5 overflow-y-auto">
            {[UI_TAB, ...tabs, MAINTENANCE_TAB].map((item) => (
              <button
                key={item}
                type="button"
                aria-current={tab === item ? "page" : undefined}
                onClick={() => setTab(item)}
                className={cn(
                  "w-full truncate rounded-md px-2 py-1.5 text-left text-sm",
                  tab === item
                    ? "bg-accent text-foreground"
                    : "text-muted-foreground hover:bg-accent/60",
                )}
              >
                {item}
              </button>
            ))}
          </nav>
          <div className="flex min-w-0 flex-1 flex-col">
            {configTab ? (
              <div className="mb-3 flex items-center gap-2">
                <Select value={scope} onValueChange={(value) => setScope(value as ConfigScope)}>
                  <SelectTrigger size="sm" className="w-32" aria-label="配置范围">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="global">全局</SelectItem>
                    <SelectItem value="project">项目</SelectItem>
                  </SelectContent>
                </Select>
                {scope === "project" ? (
                  <span className="truncate text-muted-foreground text-xs" title={cwd}>
                    {cwd}
                  </span>
                ) : null}
              </div>
            ) : null}
            {error && configTab ? (
              <p
                role="alert"
                className="mb-3 rounded-md border border-destructive/40 px-3 py-2 text-destructive text-sm"
              >
                {error}
              </p>
            ) : null}
            <div className="min-h-0 flex-1 overflow-y-auto pr-1">
              {tab === UI_TAB ? (
                <UiPrefs />
              ) : tab === MAINTENANCE_TAB ? (
                <Maintenance />
              ) : settings ? (
                <FieldTab
                  tab={tab}
                  settings={settings}
                  document={document}
                  invalid={invalid}
                  onChange={updateField}
                  onInvalid={markInvalid}
                  onDocument={(next) => {
                    setDocument(next);
                    setDirty(true);
                  }}
                />
              ) : (
                <p className="text-muted-foreground text-sm">正在读取配置…</p>
              )}
            </div>
            {configTab ? (
              <div className="flex justify-end gap-2 border-t pt-3">
                <Button variant="outline" onClick={() => void load()} disabled={saving}>
                  重新载入
                </Button>
                <Button
                  onClick={() => void save()}
                  disabled={!dirty || saving || Object.keys(invalid).length > 0}
                >
                  {saving ? "保存中…" : "保存"}
                </Button>
              </div>
            ) : null}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function Row({
  label,
  htmlFor,
  children,
}: {
  label: string;
  htmlFor?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-4 py-2">
      <label htmlFor={htmlFor} className="text-sm">
        {label}
      </label>
      {children}
    </div>
  );
}

function ChoiceSelect<T extends string>({
  id,
  value,
  labels,
  onChange,
}: {
  id: string;
  value: T;
  labels: Record<T, string>;
  onChange: (value: T) => void;
}) {
  return (
    <Select value={value} onValueChange={(next) => onChange(next as T)}>
      <SelectTrigger id={id} size="sm" className="w-44">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {(Object.keys(labels) as T[]).map((key) => (
          <SelectItem key={key} value={key}>
            {labels[key]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function UiPrefs() {
  const prefs = usePrefs();
  return (
    <div className="divide-y">
      <Row label="主题" htmlFor="pref-theme">
        <ChoiceSelect
          id="pref-theme"
          value={prefs.theme}
          labels={THEME_LABEL}
          onChange={(theme) => prefs.set({ theme })}
        />
      </Row>
      <Row label="思考显示" htmlFor="pref-thinking">
        <ChoiceSelect
          id="pref-thinking"
          value={prefs.thinkingTier}
          labels={TIER_LABEL}
          onChange={(thinkingTier) => prefs.set({ thinkingTier })}
        />
      </Row>
      <Row label="工具显示" htmlFor="pref-tool">
        <ChoiceSelect
          id="pref-tool"
          value={prefs.toolTier}
          labels={TIER_LABEL}
          onChange={(toolTier) => prefs.set({ toolTier })}
        />
      </Row>
      <Row label="运行中按 Enter" htmlFor="pref-enter">
        <ChoiceSelect
          id="pref-enter"
          value={prefs.runningEnter}
          labels={ENTER_LABEL}
          onChange={(runningEnter) => prefs.set({ runningEnter })}
        />
      </Row>
      <Row label="后台时发送浏览器通知" htmlFor="pref-notify">
        <Switch
          id="pref-notify"
          checked={prefs.notify}
          onCheckedChange={async (checked) => {
            if (checked && "Notification" in window && Notification.permission !== "granted") {
              const result = await Notification.requestPermission();
              if (result !== "granted") {
                toast.error("浏览器未授予通知权限");
                return;
              }
            }
            prefs.set({ notify: checked });
          }}
        />
      </Row>
    </div>
  );
}

function FieldTab({
  tab,
  settings,
  document,
  invalid,
  onChange,
  onInvalid,
  onDocument,
}: {
  tab: string;
  settings: SettingsDocument;
  document: Json;
  invalid: Record<string, string>;
  onChange: (field: ConfigField, value: unknown) => void;
  onInvalid: (key: string, message: string | null) => void;
  onDocument: (next: Json) => void;
}) {
  const fields = settings.fields.filter((field) => field.tab === tab);
  const basic = fields.filter((field) => !field.advanced);
  const advanced = fields.filter((field) => field.advanced);
  const providers = useMemo(() => {
    if (!fields.some((field) => field.path[0] === "providers")) return [];
    const ids = new Set<string>();
    for (const field of fields)
      if (field.path[0] === "providers" && field.path[1]) ids.add(field.path[1]);
    return [...ids];
  }, [fields]);

  const render = (field: ConfigField) => (
    <FieldEditor
      key={field.path.join("\u0000")}
      field={field}
      settings={settings}
      value={getPath(document, field.path)}
      error={invalid[field.path.join(".")]}
      onChange={(value) => onChange(field, value)}
      onInvalid={(message) => onInvalid(field.path.join("."), message)}
    />
  );

  return (
    <div className="space-y-1">
      {basic.map(render)}
      {advanced.length > 0 ? (
        <Collapsible>
          <CollapsibleTrigger className="group flex items-center gap-1 py-2 text-muted-foreground text-sm hover:text-foreground">
            <ChevronRight
              className="size-4 transition-transform group-data-[state=open]:rotate-90"
              aria-hidden
            />
            高级（{advanced.length}）
          </CollapsibleTrigger>
          <CollapsibleContent className="space-y-1">{advanced.map(render)}</CollapsibleContent>
        </Collapsible>
      ) : null}
      {providers.map((provider) => (
        <ProviderModels
          key={provider}
          provider={provider}
          settings={settings}
          document={document}
          onDocument={onDocument}
        />
      ))}
      {fields.length === 0 ? <p className="text-muted-foreground text-sm">此页没有字段</p> : null}
    </div>
  );
}

function placeholderFor(settings: SettingsDocument, field: ConfigField): string {
  for (const source of [settings.effective, settings.inherited, settings.defaults]) {
    const value = getPath(source, field.path);
    if (value !== undefined && value !== null) return formatField(value, field.kind);
  }
  return "";
}

function FieldEditor({
  field,
  settings,
  value,
  error,
  onChange,
  onInvalid,
}: {
  field: ConfigField;
  settings: SettingsDocument;
  value: unknown;
  error?: string;
  onChange: (value: unknown) => void;
  onInvalid: (message: string | null) => void;
}) {
  const id = `field-${field.path.join("-")}`;
  const placeholder = placeholderFor(settings, field);
  const [text, setText] = useState(() => formatField(value, field.kind));
  const [reveal, setReveal] = useState(false);

  useEffect(() => {
    setText(formatField(value, field.kind));
  }, [value, field.kind]);

  const commit = (next: string) => {
    setText(next);
    const parsed = parseField(next, field.kind);
    if (parsed.ok) {
      onInvalid(null);
      onChange(parsed.value);
    } else {
      onInvalid(parsed.error);
    }
  };

  let control: React.ReactNode;
  switch (field.kind.type) {
    case "bool": {
      const effective = value ?? getPath(settings.effective, field.path);
      control = (
        <Switch
          id={id}
          checked={effective === true}
          onCheckedChange={(checked) => onChange(checked)}
        />
      );
      break;
    }
    case "choice": {
      const options = field.kind.options;
      control = (
        <Select
          value={typeof value === "string" ? value : ""}
          onValueChange={(next) => onChange(next || undefined)}
        >
          <SelectTrigger id={id} size="sm" className="w-64">
            <SelectValue placeholder={placeholder || "未设置"} />
          </SelectTrigger>
          <SelectContent>
            {options.map((option) => (
              <SelectItem key={option.value} value={option.value}>
                {choiceDisplay(option)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      );
      break;
    }
    case "multiline":
    case "object":
      control = (
        <Textarea
          id={id}
          value={text}
          placeholder={placeholder}
          aria-invalid={Boolean(error)}
          onChange={(event) => commit(event.target.value)}
          className="min-h-24 font-mono text-xs"
        />
      );
      break;
    case "secret":
      control = (
        <div className="flex gap-1">
          <Input
            id={id}
            type={reveal ? "text" : "password"}
            value={text}
            placeholder={placeholder ? "••••••" : ""}
            autoComplete="off"
            onChange={(event) => commit(event.target.value)}
          />
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={reveal ? "隐藏" : "显示"}
            onClick={() => setReveal((current) => !current)}
          >
            {reveal ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
          </Button>
        </div>
      );
      break;
    default:
      control = (
        <Input
          id={id}
          inputMode={
            field.kind.type === "integer" || field.kind.type === "number" ? "decimal" : undefined
          }
          value={text}
          placeholder={placeholder}
          aria-invalid={Boolean(error)}
          onChange={(event) => commit(event.target.value)}
        />
      );
  }

  const wide = field.kind.type === "multiline" || field.kind.type === "object";
  return (
    <div className={cn("py-2", wide ? "space-y-1.5" : "flex items-start justify-between gap-4")}>
      <div className="min-w-0 flex-1">
        <label htmlFor={id} className="text-sm">
          {field.label}
        </label>
        {field.description ? (
          <p className="text-muted-foreground text-xs">{field.description}</p>
        ) : null}
        {error ? (
          <p role="alert" className="text-destructive text-xs">
            {error}
          </p>
        ) : null}
      </div>
      <div className={cn(wide ? "w-full" : "w-72 shrink-0")}>{control}</div>
    </div>
  );
}

function ProviderModels({
  provider,
  settings,
  document,
  onDocument,
}: {
  provider: string;
  settings: SettingsDocument;
  document: Json;
  onDocument: (next: Json) => void;
}) {
  const [models, setModels] = useState<CatalogModel[] | null>(null);
  const [checked, setChecked] = useState<Record<string, true>>({});
  const [loading, setLoading] = useState(false);

  const fetchModels = async () => {
    setLoading(true);
    try {
      const value = getPath(settings.effective, ["providers", provider]) ?? {};
      const reply = await manage({ action: "models", id: provider, provider: value });
      if (reply.kind !== "models") throw new Error(`意外的回复：${reply.kind}`);
      setModels(reply.data);
      setChecked({});
    } catch (error) {
      toast.error(`获取模型失败：${errorText(error)}`);
    } finally {
      setLoading(false);
    }
  };

  const apply = () => {
    let next = document;
    const template =
      settings.model_template && typeof settings.model_template === "object"
        ? settings.model_template
        : {};
    for (const id of Object.keys(checked)) {
      const path = ["providers", provider, "models", id];
      if (getPath(next, path) === undefined) next = setPath(next, path, structuredClone(template));
    }
    onDocument(next);
    setModels(null);
    toast.success(`已加入 ${Object.keys(checked).length} 个模型，保存后生效`);
  };

  return (
    <div className="mt-2 rounded-lg border p-3">
      <div className="flex items-center justify-between">
        <span className="text-sm">供应商 {provider}</span>
        <Button size="sm" variant="outline" disabled={loading} onClick={() => void fetchModels()}>
          {loading ? "获取中…" : "获取模型列表"}
        </Button>
      </div>
      {models ? (
        <div className="mt-2 space-y-1">
          <ul className="max-h-48 space-y-1 overflow-y-auto">
            {models.map((model) => {
              const id = `model-${provider}-${model.id}`;
              const exists =
                getPath(document, ["providers", provider, "models", model.id]) !== undefined;
              return (
                <li key={model.id} className="flex items-center gap-2 text-sm">
                  <Checkbox
                    id={id}
                    disabled={exists}
                    checked={exists || Boolean(checked[model.id])}
                    onCheckedChange={(value) =>
                      setChecked((current) => {
                        const next = { ...current };
                        if (value === true) next[model.id] = true;
                        else delete next[model.id];
                        return next;
                      })
                    }
                  />
                  <label htmlFor={id} className="truncate">
                    {model.name === model.id ? model.id : `${model.name}（${model.id}）`}
                  </label>
                </li>
              );
            })}
          </ul>
          <div className="flex justify-end gap-2">
            <Button size="sm" variant="ghost" onClick={() => setModels(null)}>
              取消
            </Button>
            <Button size="sm" disabled={Object.keys(checked).length === 0} onClick={apply}>
              加入所选
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function Maintenance() {
  const [report, setReport] = useState<MaintenanceReport | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (apply: boolean) => {
    setBusy(true);
    try {
      const reply = await manage({ action: "maintenance", apply });
      if (reply.kind !== "maintenance") throw new Error(`意外的回复：${reply.kind}`);
      setReport(reply.data);
      if (apply) toast.success(`已清理 ${reply.data.removed.length} 项`);
    } catch (error) {
      toast.error(`维护失败：${errorText(error)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-3">
      <p className="text-muted-foreground text-sm">
        清理不再需要的会话缓存与临时文件。先预览，确认后再删除。
      </p>
      <div className="flex gap-2">
        <Button variant="outline" disabled={busy} onClick={() => void run(false)}>
          预览
        </Button>
        <Button
          variant="destructive"
          disabled={busy || !report || report.applied || report.removed.length === 0}
          onClick={() => void run(true)}
        >
          确认清理
        </Button>
      </div>
      {report ? (
        <div className="space-y-1 text-sm">
          <p>
            {report.applied ? "已删除" : "可删除"} {report.removed.length} 项，共{" "}
            {formatBytes(report.bytes)}； 受保护 {report.protected} 项
          </p>
          <ul className="max-h-60 overflow-y-auto rounded-md border p-2 font-mono text-xs">
            {report.removed.map((path) => (
              <li key={path} className="truncate" title={path}>
                {path}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
