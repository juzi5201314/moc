import {
  AlertCircleIcon,
  CheckIcon,
  CommandIcon,
  KeyIcon,
  Loader2Icon,
  MoonIcon,
  PaletteIcon,
  PuzzleIcon,
  RefreshCwIcon,
  SaveIcon,
  SparklesIcon,
  SunIcon,
} from "lucide-react";
import type { ReactNode } from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent } from "@/components/ui/dialog";
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
import { formatField, getPath, parseField, setPath } from "@/lib/settings-doc";
import { rpc } from "@/protocol/client";
import type {
  CatalogModel,
  ConfigAction,
  ConfigField,
  ConfigReply,
  ConfigScope,
  FieldKind,
  SettingsDocument,
} from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { usePrefs } from "@/store/prefs";
import { useUi } from "@/store/ui";

type SettingsTab = "general" | "models" | "providers" | "plugins" | "shortcuts";
type ScopeDocuments = Partial<Record<ConfigScope, SettingsDocument>>;
type EditState = {
  values: Record<string, string>;
  dirty: Record<string, boolean>;
  errors: Record<string, string>;
};
type ScopeEditStates = Record<ConfigScope, EditState>;
type DiscoveryState = { models: CatalogModel[]; busy: boolean; error: string | null };

const PATH_SEPARATOR = "\u001f";
const EMPTY_EDIT: EditState = { values: {}, dirty: {}, errors: {} };
const EMPTY_EDITS: ScopeEditStates = { global: EMPTY_EDIT, project: EMPTY_EDIT };
const SHORTCUTS = [
  { label: "新建会话", shortcut: "⌘N / Ctrl+N" },
  { label: "命令面板", shortcut: "⌘K / Ctrl+K" },
  { label: "工作区文件", shortcut: "Ctrl+Alt+P" },
  { label: "新建终端", shortcut: "Ctrl+~" },
  { label: "聚焦输入框", shortcut: "Escape / Ctrl+L" },
];

function fieldKey(path: string[]): string {
  return path.join(PATH_SEPARATOR);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function tabFromValue(value: string | undefined): SettingsTab {
  return value === "general" ||
    value === "models" ||
    value === "providers" ||
    value === "plugins" ||
    value === "shortcuts"
    ? value
    : "general";
}

function createEditState(data: SettingsDocument): EditState {
  const values: Record<string, string> = {};
  for (const field of data.fields) {
    const explicit = getPath(data.document, field.path);
    const effective = getPath(data.effective, field.path);
    values[fieldKey(field.path)] = formatField(
      explicit === undefined ? effective : explicit,
      field.kind,
    );
  }
  return { values, dirty: {}, errors: {} };
}

function displayValue(value: unknown, kind: FieldKind): string {
  if (kind.type === "secret" && value !== undefined && value !== null && value !== "")
    return "已设置（隐藏）";
  return formatField(value, kind) || "未设置";
}

function parseSetting(field: ConfigField, text: string) {
  const parsed = parseField(text, field.kind);
  if (!parsed.ok) return parsed;
  if (
    field.kind.type === "choice" &&
    parsed.value !== undefined &&
    !field.kind.options.some((item) => item.value === parsed.value)
  ) {
    return { ok: false as const, error: "请选择有效选项" };
  }
  return parsed;
}

function applyEdits(
  document: Record<string, unknown>,
  fields: ConfigField[],
  edits: EditState,
): { document: Record<string, unknown>; errors: Record<string, string> } {
  let next = document;
  const errors: Record<string, string> = {};
  for (const field of fields) {
    const key = fieldKey(field.path);
    if (!edits.dirty[key]) continue;
    const parsed = parseSetting(field, edits.values[key] ?? "");
    if (!parsed.ok) errors[key] = parsed.error;
    else next = setPath(next, field.path, parsed.value);
  }
  return { document: next, errors };
}

function providersIn(data: SettingsDocument | undefined): string[] {
  const providers = data ? getPath(data.effective, ["providers"]) : undefined;
  return isRecord(providers) ? Object.keys(providers).sort() : [];
}

function ConfigFieldRow({
  field,
  data,
  value,
  error,
  disabled,
  onChange,
}: {
  field: ConfigField;
  data: SettingsDocument;
  value: string;
  error?: string;
  disabled: boolean;
  onChange: (value: string) => void;
}) {
  const explicit = getPath(data.document, field.path);
  const inherited = getPath(data.inherited, field.path);
  const effective = getPath(data.effective, field.path);
  const defaults = getPath(data.defaults, field.path);
  const source =
    explicit !== undefined ? "当前范围覆盖" : inherited !== undefined ? "继承" : "默认值";
  const invalid = error ? "border-destructive" : "";

  let editor: ReactNode;
  if (field.kind.type === "bool") {
    editor = (
      <Switch
        checked={value === "true"}
        disabled={disabled}
        onCheckedChange={(checked) => onChange(String(checked))}
        aria-label={field.label}
      />
    );
  } else if (field.kind.type === "choice") {
    editor = (
      <Select
        value={value || "__inherit__"}
        onValueChange={(next) => onChange(next === "__inherit__" ? "" : next)}
        disabled={disabled}
      >
        <SelectTrigger size="sm" className={`w-full max-w-xs text-xs ${invalid}`}>
          <SelectValue placeholder="使用继承值" />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="__inherit__">使用继承值</SelectItem>
          {field.kind.options.map((option) => (
            <SelectItem key={option.value} value={option.value}>
              {option.label === option.value ? option.value : `${option.label}（${option.value}）`}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    );
  } else if (field.kind.type === "multiline" || field.kind.type === "object") {
    editor = (
      <Textarea
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
        className={`min-h-20 max-w-xl font-mono text-xs ${invalid}`}
        aria-invalid={Boolean(error)}
      />
    );
  } else {
    editor = (
      <Input
        type={field.kind.type === "secret" ? "password" : "text"}
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
        className={`h-8 max-w-xl text-xs ${invalid}`}
        aria-invalid={Boolean(error)}
      />
    );
  }

  return (
    <div className="space-y-2 border-b border-border/60 py-3 last:border-b-0">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0 space-y-0.5">
          <div className="flex flex-wrap items-center gap-1.5 font-medium text-foreground">
            <span>{field.label}</span>
            {field.advanced && (
              <span className="rounded border border-border px-1 py-0.5 text-[10px] text-muted-foreground">
                高级
              </span>
            )}
          </div>
          <div className="text-[11px] leading-relaxed text-muted-foreground">
            {field.description}
          </div>
          <div className="font-mono text-[10px] text-muted-foreground/60">
            {field.path.join(".")}
          </div>
        </div>
        <div className="shrink-0">{editor}</div>
      </div>
      <div className="grid gap-x-3 gap-y-1 text-[10px] text-muted-foreground sm:grid-cols-4">
        <span>
          <b className="font-medium text-foreground/70">当前范围：</b>
          {displayValue(explicit, field.kind)}
        </span>
        <span>
          <b className="font-medium text-foreground/70">生效值：</b>
          {displayValue(effective, field.kind)}
        </span>
        <span>
          <b className="font-medium text-foreground/70">继承值：</b>
          {displayValue(inherited, field.kind)}
        </span>
        <span>
          <b className="font-medium text-foreground/70">默认值：</b>
          {displayValue(defaults, field.kind)} <span className="text-primary/80">({source})</span>
        </span>
      </div>
      {error && <div className="text-[11px] text-destructive">{error}</div>}
    </div>
  );
}

export function SettingsDialog() {
  const { settings, set } = useUi();
  const { notify, theme, set: setPrefs } = usePrefs();
  const server = useConnection((state) => state.server);
  const config = useConnection((state) => state.config);
  const models = config?.models ?? [];
  const cwd = config?.cwd ?? "";

  const [activeTab, setActiveTab] = useState<SettingsTab>("general");
  const [scope, setScope] = useState<ConfigScope>("global");
  const [documents, setDocuments] = useState<ScopeDocuments>({});
  const [editStates, setEditStates] = useState<ScopeEditStates>(EMPTY_EDITS);
  const [loadingScopes, setLoadingScopes] = useState<Record<ConfigScope, boolean>>({
    global: false,
    project: false,
  });
  const [saving, setSaving] = useState(false);
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [settingsMessage, setSettingsMessage] = useState<string | null>(null);
  const [providerId, setProviderId] = useState("");
  const [providerDraft, setProviderDraft] = useState("{}");
  const [discovery, setDiscovery] = useState<DiscoveryState | null>(null);
  const [selectedModelIds, setSelectedModelIds] = useState<string[]>([]);
  const wasOpen = useRef(false);
  const loadedCwd = useRef<string | null>(null);

  const currentDocument = documents[scope];
  const currentEdits = editStates[scope];
  const loading = loadingScopes[scope];
  const currentFields = useMemo(
    () =>
      currentDocument?.fields.filter(
        (field) => field.tab === (activeTab === "providers" ? "供应商" : "模型"),
      ) ?? [],
    [activeTab, currentDocument],
  );
  const currentProviderFields = useMemo(
    () =>
      currentDocument?.fields.filter(
        (field) =>
          field.path[0] === "providers" && field.path[1] === providerId && field.path.length <= 3,
      ) ?? [],
    [currentDocument, providerId],
  );
  const knownProviders = useMemo(() => providersIn(currentDocument), [currentDocument]);
  const existingModelIds = useMemo(() => {
    const value = getPath(currentDocument?.effective, ["providers", providerId, "models"]);
    return new Set(isRecord(value) ? Object.keys(value) : []);
  }, [currentDocument, providerId]);
  const dirtyCount = Object.values(currentEdits.dirty).filter(Boolean).length;

  const loadScope = useCallback(
    async (target: ConfigScope, reset: boolean) => {
      if (!cwd) {
        setSettingsError("尚未取得核心工作目录，暂时无法读取配置");
        return;
      }
      setLoadingScopes((state) => ({ ...state, [target]: true }));
      setSettingsError(null);
      setSettingsMessage(null);
      try {
        const action: ConfigAction = { action: "read", scope: target, cwd };
        const reply = await rpc.call<ConfigReply>("config/manage", action);
        if (reply.kind !== "settings") throw new Error("配置读取返回了非 settings 响应");
        setDocuments((state) => ({ ...state, [target]: reply.data }));
        if (reset) setEditStates((state) => ({ ...state, [target]: createEditState(reply.data) }));
      } catch (error) {
        setSettingsError(
          `读取${target === "global" ? "全局" : "项目"}配置失败：${errorText(error)}`,
        );
      } finally {
        setLoadingScopes((state) => ({ ...state, [target]: false }));
      }
    },
    [cwd],
  );

  useEffect(() => {
    if (settings.open) setActiveTab(tabFromValue(settings.tab));
  }, [settings.open, settings.tab]);

  useEffect(() => {
    if (!settings.open) {
      wasOpen.current = false;
      return;
    }
    if (wasOpen.current && loadedCwd.current === cwd) return;
    wasOpen.current = true;
    loadedCwd.current = cwd;
    setDocuments({});
    setEditStates({ global: EMPTY_EDIT, project: EMPTY_EDIT });
    setSettingsError(null);
    setSettingsMessage(null);
    setDiscovery(null);
    void loadScope("global", true);
    void loadScope("project", true);
  }, [cwd, loadScope, settings.open]);

  useEffect(() => {
    if (!providerId && knownProviders[0]) setProviderId(knownProviders[0]);
  }, [knownProviders, providerId]);

  useEffect(() => {
    if (!currentDocument || !providerId) return;
    const provider = getPath(currentDocument.effective, ["providers", providerId]);
    setProviderDraft(JSON.stringify(provider ?? currentDocument.provider_template ?? {}, null, 2));
  }, [currentDocument, providerId]);

  const updateField = (field: ConfigField, value: string) => {
    const key = fieldKey(field.path);
    setEditStates((state) => {
      const current = state[scope];
      const errors = { ...current.errors };
      delete errors[key];
      return {
        ...state,
        [scope]: {
          values: { ...current.values, [key]: value },
          dirty: { ...current.dirty, [key]: true },
          errors,
        },
      };
    });
    setSettingsMessage(null);
  };

  const saveDocument = useCallback(
    async (document: Record<string, unknown>, message: string): Promise<boolean> => {
      const data = documents[scope];
      if (!data || !cwd) {
        toast.error(!data ? "配置尚未读取完成" : "尚未取得核心工作目录");
        return false;
      }
      setSaving(true);
      setSettingsError(null);
      setSettingsMessage(null);
      try {
        const action: ConfigAction = {
          action: "save",
          scope,
          cwd,
          document,
          revision: data.revision,
        };
        const reply = await rpc.call<ConfigReply>("config/manage", action);
        if (reply.kind !== "settings") throw new Error("配置保存返回了非 settings 响应");
        setDocuments((state) => ({ ...state, [scope]: reply.data }));
        setEditStates((state) => ({ ...state, [scope]: createEditState(reply.data) }));
        setSettingsMessage(message);
        toast.success(message);
        return true;
      } catch (error) {
        const message = `保存配置失败：${errorText(error)}`;
        setSettingsError(message);
        toast.error(message);
        return false;
      } finally {
        setSaving(false);
      }
    },
    [cwd, documents, scope],
  );

  const handleSave = async () => {
    if (!currentDocument || dirtyCount === 0) return;
    const prepared = applyEdits(currentDocument.document, currentDocument.fields, currentEdits);
    if (Object.keys(prepared.errors).length) {
      setEditStates((state) => ({
        ...state,
        [scope]: { ...state[scope], errors: prepared.errors },
      }));
      toast.error("请先修正配置字段");
      return;
    }
    await saveDocument(prepared.document, "配置已保存并重载");
  };

  const handleReload = () => {
    setDiscovery(null);
    void loadScope(scope, true);
  };

  const providerForDiscovery = (): {
    provider: Record<string, unknown>;
    errors: Record<string, string>;
  } | null => {
    if (!currentDocument || !providerId.trim()) return null;
    if (currentProviderFields.length) {
      const effective = getPath(currentDocument.effective, ["providers", providerId]);
      let provider: Record<string, unknown> = isRecord(effective) ? effective : {};
      const errors: Record<string, string> = {};
      for (const field of currentProviderFields) {
        const key = fieldKey(field.path);
        if (!currentEdits.dirty[key]) continue;
        const parsed = parseSetting(field, currentEdits.values[key] ?? "");
        if (!parsed.ok) errors[key] = parsed.error;
        else provider = setPath(provider, field.path.slice(2), parsed.value);
      }
      return { provider, errors };
    }
    try {
      const parsed: unknown = JSON.parse(providerDraft);
      if (!isRecord(parsed)) throw new Error("供应商配置必须是 JSON 对象");
      return { provider: parsed, errors: {} };
    } catch (error) {
      toast.error(`供应商配置格式有误：${errorText(error)}`);
      return null;
    }
  };

  const handleDiscover = async () => {
    const prepared = providerForDiscovery();
    if (!prepared) {
      if (!providerId.trim()) toast.error("请先选择或填写供应商 ID");
      return;
    }
    if (Object.keys(prepared.errors).length) {
      setEditStates((state) => ({
        ...state,
        [scope]: { ...state[scope], errors: { ...state[scope].errors, ...prepared.errors } },
      }));
      toast.error("请先修正供应商字段");
      return;
    }
    setDiscovery({ models: [], busy: true, error: null });
    setSelectedModelIds([]);
    try {
      const action: ConfigAction = {
        action: "models",
        id: providerId,
        provider: prepared.provider,
      };
      const reply = await rpc.call<ConfigReply>("config/manage", action);
      if (reply.kind !== "models") throw new Error("模型发现返回了非 models 响应");
      setDiscovery({ models: reply.data, busy: false, error: null });
      setSelectedModelIds(
        reply.data.filter((model) => !existingModelIds.has(model.id)).map((model) => model.id),
      );
      toast.success(`已发现 ${reply.data.length} 个模型`);
    } catch (error) {
      const message = errorText(error);
      setDiscovery({ models: [], busy: false, error: message });
      toast.error(`模型发现失败：${message}`);
    }
  };

  const handleAddModels = async () => {
    if (!currentDocument || !discovery || !selectedModelIds.length) return;
    let next = currentDocument.document;
    if (currentProviderFields.length) {
      const prepared = applyEdits(next, currentProviderFields, currentEdits);
      if (Object.keys(prepared.errors).length) {
        setEditStates((state) => ({
          ...state,
          [scope]: { ...state[scope], errors: { ...state[scope].errors, ...prepared.errors } },
        }));
        toast.error("请先修正供应商字段");
        return;
      }
      next = prepared.document;
    } else {
      try {
        const provider: unknown = JSON.parse(providerDraft);
        if (!isRecord(provider)) throw new Error("供应商配置必须是 JSON 对象");
        next = setPath(next, ["providers", providerId], provider);
      } catch (error) {
        toast.error(`供应商配置格式有误：${errorText(error)}`);
        return;
      }
    }
    for (const id of selectedModelIds) {
      if (!existingModelIds.has(id))
        next = setPath(next, ["providers", providerId, "models", id], {});
    }
    const defaultModel = getPath(currentDocument.effective, ["models", "roles", "default"]);
    if (typeof defaultModel !== "string" || !defaultModel) {
      next = setPath(next, ["models", "roles", "default"], `${providerId}/${selectedModelIds[0]}`);
    }
    if (await saveDocument(next, "已添加模型并保存配置")) setDiscovery(null);
  };

  const openTab = (tab: SettingsTab) => {
    setActiveTab(tab);
    set({ settings: { open: true, tab } });
  };

  const fieldsForView =
    activeTab === "providers"
      ? currentFields.filter(
          (field) =>
            !(
              field.path[0] === "providers" &&
              field.path[1] === providerId &&
              field.path.length <= 3
            ),
        )
      : currentFields;

  const renderFields = () => {
    if (loading && !currentDocument)
      return (
        <div className="flex items-center justify-center gap-2 py-12 text-xs text-muted-foreground">
          <Loader2Icon className="size-4 animate-spin" />
          正在读取配置文档...
        </div>
      );
    if (!currentDocument)
      return <div className="py-12 text-center text-xs text-muted-foreground">暂无配置文档</div>;
    if (!fieldsForView.length)
      return (
        <div className="py-12 text-center text-xs text-muted-foreground">
          当前范围暂无此类可编辑字段
        </div>
      );
    return fieldsForView.map((field) => {
      const key = fieldKey(field.path);
      return (
        <ConfigFieldRow
          key={key}
          field={field}
          data={currentDocument}
          value={currentEdits.values[key] ?? ""}
          error={currentEdits.errors[key]}
          disabled={saving}
          onChange={(value) => updateField(field, value)}
        />
      );
    });
  };

  const renderDiscovery = () => {
    if (!currentDocument) return null;
    const hasSchema = isRecord(currentDocument.provider_template);
    const selected = new Set(selectedModelIds);
    return (
      <div className="space-y-3 rounded-lg border border-border bg-muted/10 p-3">
        <div>
          <div className="font-medium text-foreground">从供应商发现模型</div>
          <div className="mt-0.5 text-[11px] text-muted-foreground">
            按 provider_template 编辑供应商，再调用 config/manage models；结果需勾选后保存。
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Input
            value={providerId}
            onChange={(event) => {
              setProviderId(event.target.value);
              setDiscovery(null);
            }}
            placeholder="供应商 ID，例如 openai"
            className="h-8 w-52 text-xs"
            aria-label="供应商 ID"
          />
          {knownProviders.length > 0 && (
            <Select
              value={knownProviders.includes(providerId) ? providerId : "__none__"}
              onValueChange={(value) => {
                if (value !== "__none__") setProviderId(value);
                setDiscovery(null);
              }}
            >
              <SelectTrigger size="sm" className="w-44 text-xs">
                <SelectValue placeholder="选择已有供应商" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="__none__">新供应商 ID</SelectItem>
                {knownProviders.map((id) => (
                  <SelectItem key={id} value={id}>
                    {id}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
          <Button
            size="xs"
            variant="outline"
            onClick={() => void handleDiscover()}
            disabled={discovery?.busy || !providerId.trim()}
          >
            {discovery?.busy ? (
              <Loader2Icon className="size-3 animate-spin" />
            ) : (
              <SparklesIcon className="size-3" />
            )}
            发现模型
          </Button>
        </div>
        {currentProviderFields.length ? (
          <div className="rounded border border-border/60 bg-background px-2.5">
            {currentProviderFields.map((field) => {
              const key = fieldKey(field.path);
              return (
                <ConfigFieldRow
                  key={key}
                  field={field}
                  data={currentDocument}
                  value={currentEdits.values[key] ?? ""}
                  error={currentEdits.errors[key]}
                  disabled={saving}
                  onChange={(value) => updateField(field, value)}
                />
              );
            })}
          </div>
        ) : (
          <div className="space-y-1.5">
            <div className="text-[11px] text-muted-foreground">
              当前供应商没有字段目录；
              {hasSchema ? "按 provider_template 编辑 JSON。" : "请输入供应商 JSON。"}
            </div>
            <Textarea
              value={providerDraft}
              onChange={(event) => setProviderDraft(event.target.value)}
              className="min-h-32 font-mono text-xs"
              aria-label="供应商配置 JSON"
            />
          </div>
        )}
        {discovery?.error && (
          <div className="flex items-start gap-1.5 text-[11px] text-destructive">
            <AlertCircleIcon className="mt-0.5 size-3.5 shrink-0" />
            <span>{discovery.error}</span>
          </div>
        )}
        {discovery && !discovery.busy && discovery.models.length > 0 && (
          <div className="space-y-2 border-t border-border/60 pt-2">
            <div className="flex items-center justify-between text-[11px] text-muted-foreground">
              <span>候选模型（已存在的模型不可重复添加）</span>
              <span>{selectedModelIds.length} 个待添加</span>
            </div>
            <div className="grid max-h-40 gap-1 overflow-y-auto sm:grid-cols-2">
              {discovery.models.map((model) => {
                const exists = existingModelIds.has(model.id);
                const inputId = `discovered-model-${model.id.replace(/[^a-zA-Z0-9_-]/g, "-")}`;
                return (
                  <label
                    key={model.id}
                    htmlFor={inputId}
                    className={`flex items-center gap-2 rounded border px-2 py-1.5 text-xs ${exists ? "cursor-not-allowed opacity-50" : "cursor-pointer hover:bg-muted/50"}`}
                  >
                    <Checkbox
                      id={inputId}
                      checked={exists || selected.has(model.id)}
                      disabled={exists || saving}
                      onCheckedChange={(checked) => {
                        if (exists) return;
                        setSelectedModelIds((ids) =>
                          checked === true
                            ? [...new Set([...ids, model.id])]
                            : ids.filter((id) => id !== model.id),
                        );
                      }}
                    />
                    <span className="min-w-0 flex-1 truncate font-mono">{model.id}</span>
                    <span className="truncate text-[10px] text-muted-foreground">{model.name}</span>
                    {exists && <CheckIcon className="size-3 text-muted-foreground" />}
                  </label>
                );
              })}
            </div>
            <Button
              size="xs"
              onClick={() => void handleAddModels()}
              disabled={saving || selectedModelIds.length === 0}
            >
              <SaveIcon className="size-3" />
              添加所选模型并保存
            </Button>
          </div>
        )}
        {discovery && !discovery.busy && !discovery.models.length && !discovery.error && (
          <div className="text-[11px] text-muted-foreground">没有返回可添加的模型。</div>
        )}
      </div>
    );
  };

  const configHeader = (
    <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border/60 pb-3">
      <div className="flex items-center gap-2">
        <Select
          value={scope}
          onValueChange={(value) => {
            setScope(value as ConfigScope);
            setSettingsError(null);
            setSettingsMessage(null);
          }}
        >
          <SelectTrigger size="sm" className="w-28 text-xs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="global">全局配置</SelectItem>
            <SelectItem value="project">项目配置</SelectItem>
          </SelectContent>
        </Select>
        <span className="max-w-[360px] truncate font-mono text-[10px] text-muted-foreground">
          {currentDocument?.path ?? (cwd || "等待连接")}
        </span>
      </div>
      <div className="flex items-center gap-1.5">
        <Button variant="outline" size="xs" onClick={handleReload} disabled={loading || saving}>
          <RefreshCwIcon className={`size-3 ${loading ? "animate-spin" : ""}`} />
          重载
        </Button>
        <Button
          size="xs"
          onClick={() => void handleSave()}
          disabled={!currentDocument || saving || dirtyCount === 0}
        >
          {saving ? (
            <Loader2Icon className="size-3 animate-spin" />
          ) : (
            <SaveIcon className="size-3" />
          )}
          保存{dirtyCount > 0 ? ` (${dirtyCount})` : ""}
        </Button>
      </div>
    </div>
  );

  return (
    <Dialog open={settings.open} onOpenChange={(open) => set({ settings: { open } })}>
      <DialogContent className="max-w-5xl gap-0 overflow-hidden p-0 text-xs">
        <div className="flex h-[min(720px,90vh)]">
          <div className="w-44 shrink-0 space-y-1 border-r border-border bg-muted/20 p-2">
            <div className="px-2 py-1.5 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
              系统设置
            </div>
            <NavButton
              active={activeTab === "general"}
              icon={<PaletteIcon />}
              onClick={() => openTab("general")}
            >
              常规与外观
            </NavButton>
            <NavButton
              active={activeTab === "models"}
              icon={<SparklesIcon />}
              onClick={() => openTab("models")}
            >
              模型配置
            </NavButton>
            <NavButton
              active={activeTab === "providers"}
              icon={<KeyIcon />}
              onClick={() => openTab("providers")}
            >
              账号与服务商
            </NavButton>
            <NavButton
              active={activeTab === "plugins"}
              icon={<PuzzleIcon />}
              onClick={() => openTab("plugins")}
            >
              插件管理
            </NavButton>
            <NavButton
              active={activeTab === "shortcuts"}
              icon={<CommandIcon />}
              onClick={() => openTab("shortcuts")}
            >
              快捷键指南
            </NavButton>
          </div>

          <div className="min-w-0 flex-1 overflow-y-auto p-5">
            {(activeTab === "models" || activeTab === "providers") && (
              <div className="mb-4 space-y-3">
                {configHeader}
                {(settingsError || settingsMessage) && (
                  <div
                    className={`text-[11px] ${settingsError ? "text-destructive" : "text-emerald-600"}`}
                  >
                    {settingsError ?? settingsMessage}
                  </div>
                )}
              </div>
            )}

            {activeTab === "general" && (
              <div className="space-y-5">
                <div>
                  <h3 className="text-sm font-semibold">常规与外观</h3>
                  <p className="text-xs text-muted-foreground">
                    管理应用主题界面、系统通知与交互偏好
                  </p>
                </div>
                <div className="space-y-4 border-t pt-4">
                  <div className="flex items-center justify-between">
                    <div>
                      <div className="font-medium">外观主题</div>
                      <div className="text-[11px] text-muted-foreground">
                        切换深色或浅色主题模式
                      </div>
                    </div>
                    <div className="flex items-center gap-1 rounded-lg border bg-muted/30 p-0.5">
                      <button
                        type="button"
                        onClick={() => setPrefs({ theme: "light" })}
                        className={`flex items-center gap-1.5 rounded-md px-2.5 py-1 text-xs transition-colors ${theme === "light" ? "bg-background font-medium text-foreground shadow-2xs" : "text-muted-foreground"}`}
                      >
                        <SunIcon className="size-3" />
                        浅色
                      </button>
                      <button
                        type="button"
                        onClick={() => setPrefs({ theme: "dark" })}
                        className={`flex items-center gap-1.5 rounded-md px-2.5 py-1 text-xs transition-colors ${theme === "dark" ? "bg-background font-medium text-foreground shadow-2xs" : "text-muted-foreground"}`}
                      >
                        <MoonIcon className="size-3" />
                        深色
                      </button>
                    </div>
                  </div>
                  <div className="flex items-center justify-between border-t pt-3">
                    <div>
                      <div className="font-medium">桌面通知</div>
                      <div className="text-[11px] text-muted-foreground">
                        当后台有需要审批的权限或工具完成时提醒
                      </div>
                    </div>
                    <Switch
                      id="notify"
                      checked={notify}
                      onCheckedChange={(value) => setPrefs({ notify: value })}
                    />
                  </div>
                  <div className="space-y-1.5 border-t pt-3 text-muted-foreground">
                    <div className="font-medium text-foreground">核心服务端环境</div>
                    <div className="flex justify-between text-[11px]">
                      <span>协议版本:</span>
                      <span className="font-mono text-foreground">
                        {server?.protocol_version ?? "1.4"}
                      </span>
                    </div>
                    <div className="flex justify-between text-[11px]">
                      <span>核心程序版本:</span>
                      <span className="font-mono text-foreground">
                        {server?.server.version ?? "moc-core v0.1.0"}
                      </span>
                    </div>
                    <div className="flex justify-between text-[11px]">
                      <span>配置工作目录:</span>
                      <span className="max-w-[60%] truncate font-mono text-foreground">
                        {cwd || "未连接"}
                      </span>
                    </div>
                  </div>
                </div>
              </div>
            )}

            {activeTab === "models" && (
              <div className="space-y-4">
                <div>
                  <h3 className="text-sm font-semibold">模型配置</h3>
                  <p className="text-xs text-muted-foreground">
                    编辑当前范围的模型角色、模型参数与上下文能力
                  </p>
                </div>
                <div className="divide-y divide-border overflow-hidden rounded-lg border">
                  {models.map((model) => (
                    <div
                      key={model.id}
                      className="flex items-center justify-between bg-card p-3 hover:bg-muted/20"
                    >
                      <div className="min-w-0">
                        <div className="truncate font-mono font-medium">{model.id}</div>
                        <div className="text-[11px] text-muted-foreground">
                          {model.display_name || "未命名模型"}
                        </div>
                      </div>
                      <div className="shrink-0 text-right text-[10px] text-muted-foreground">
                        {model.context_window
                          ? `${Math.round(model.context_window / 1000)}k context`
                          : "context 未声明"}
                        {model.inputs?.length ? ` · ${model.inputs.join(", ")}` : ""}
                      </div>
                    </div>
                  ))}
                  {!models.length && (
                    <div className="p-4 text-center text-muted-foreground">
                      暂无 config/get 模型
                    </div>
                  )}
                </div>
                <div className="rounded-lg border border-border bg-card px-3">
                  <div className="border-b border-border/60 py-3 font-medium">模型字段</div>
                  {renderFields()}
                </div>
              </div>
            )}

            {activeTab === "providers" && (
              <div className="space-y-4">
                <div>
                  <h3 className="text-sm font-semibold">账号与服务商</h3>
                  <p className="text-xs text-muted-foreground">
                    编辑供应商连接配置，并按 provider_template 发现模型
                  </p>
                </div>
                {renderDiscovery()}
                <div className="rounded-lg border border-border bg-card px-3">
                  <div className="border-b border-border/60 py-3 font-medium">供应商字段</div>
                  {renderFields()}
                </div>
              </div>
            )}

            {activeTab === "plugins" && (
              <div className="space-y-4">
                <div>
                  <h3 className="text-sm font-semibold">插件管理</h3>
                  <p className="text-xs text-muted-foreground">
                    插件配置字段由核心配置文档提供；保存后会重载插件宿主
                  </p>
                </div>
                <div className="rounded-lg border bg-card p-3.5">
                  <div className="flex items-center justify-between">
                    <span className="font-medium">插件配置</span>
                    <span className="text-[10px] text-muted-foreground">
                      请在项目/全局配置页编辑
                    </span>
                  </div>
                  <p className="mt-2 text-[11px] text-muted-foreground">
                    插件字段会随 core 返回的 SettingsDocument 出现在对应配置分组中。
                  </p>
                </div>
              </div>
            )}

            {activeTab === "shortcuts" && (
              <div className="space-y-4">
                <div>
                  <h3 className="text-sm font-semibold">全局快捷键指南</h3>
                  <p className="text-xs text-muted-foreground">提高操作效率的高频键盘快捷键</p>
                </div>
                <div className="divide-y divide-border overflow-hidden rounded-lg border">
                  {SHORTCUTS.map((item) => (
                    <div
                      key={item.label}
                      className="flex items-center justify-between bg-card p-2.5"
                    >
                      <span className="font-medium text-foreground">{item.label}</span>
                      <kbd className="rounded border bg-muted/60 px-2 py-0.5 font-mono text-[10px] text-muted-foreground">
                        {item.shortcut}
                      </kbd>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function NavButton({
  active,
  icon,
  onClick,
  children,
}: {
  active: boolean;
  icon: ReactNode;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`flex h-8 w-full items-center gap-2 rounded-md px-2 text-xs font-medium transition-colors ${active ? "bg-background text-foreground shadow-2xs" : "text-muted-foreground hover:bg-muted/60 hover:text-foreground"}`}
    >
      <span className="size-3.5 [&>svg]:size-3.5">{icon}</span>
      <span>{children}</span>
    </button>
  );
}
