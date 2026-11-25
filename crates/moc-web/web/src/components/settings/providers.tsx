// 供应商与模型工作台：顶部全宽供应商切换条 + 全宽详情（Models / Connection / Params）
// + 模型钻取编辑 + 通配默认规则 + 预设新建 + 连通性测试 + 模型发现与批量操作。

import {
  ActivityIcon,
  CheckCircle2Icon,
  CheckIcon,
  CopyIcon,
  DownloadCloudIcon,
  ImageIcon,
  Loader2Icon,
  PencilIcon,
  PlusIcon,
  SearchIcon,
  SlidersHorizontalIcon,
  SparklesIcon,
  Trash2Icon,
  XCircleIcon,
  XIcon,
  ZapIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { t, useT } from "@/lib/i18n";
import { compactTokens } from "@/lib/models";
import {
  inferModelEntry,
  isValidBaseUrl,
  modelEntryFromCatalog,
  nextCopyId,
  PROVIDER_PRESETS,
  type ProviderPreset,
} from "@/lib/provider-presets";
import { getPath, setPath } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  CatalogModel,
  ChoiceOption,
  ConfigAction,
  ConfigReply,
  ProviderTestResult,
  SettingsDocument,
} from "@/protocol/types";
import { FieldList, FieldRow } from "./field";
import { ModelDefaultsView, ModelEditor } from "./model-editor";
import { ApiKeyInput, KeyValueEditor, RequestParamsEditor } from "./provider-widgets";
import { useSettings, writeField, writeSettings, writeSettingsWithUndo } from "./settings-store";

type Json = Record<string, unknown>;
type ProviderMode = "detail" | "add" | "defaults";
type DetailTab = "models" | "connection" | "params";

const API_FALLBACK: ChoiceOption[] = [
  { value: "openai_chat", label: "OpenAI Chat" },
  { value: "openai_responses", label: "OpenAI Responses" },
  { value: "anthropic_messages", label: "Anthropic Messages" },
];

const PRIMARY_CONN: Record<string, true> = {
  api: true,
  base_url: true,
  api_key: true,
  endpoint: true,
};

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function apiOptions(doc: SettingsDocument): ChoiceOption[] {
  const field = doc.fields.find((item) => item.path[0] === "providers" && item.path[2] === "api");
  return field?.kind.type === "choice" ? field.kind.options : API_FALLBACK;
}

async function discover(id: string, provider: unknown): Promise<CatalogModel[]> {
  const action: ConfigAction = { action: "models", id, provider };
  const reply = await rpc.call<ConfigReply>("config/manage", action);
  if (reply.kind !== "models") throw new Error(t("sp.bad_reply"));
  return reply.data;
}

async function testConnection(id: string, provider: unknown): Promise<ProviderTestResult> {
  const action: ConfigAction = { action: "test_provider", id, provider };
  const reply = await rpc.call<ConfigReply>("config/manage", action);
  if (reply.kind !== "provider_test") throw new Error(t("sp.bad_reply"));
  return reply.data;
}

function formatPriceBadge(pricing: unknown): string | null {
  if (!isObject(pricing)) return null;
  if (typeof pricing.input !== "number" || typeof pricing.output !== "number") return null;
  return `$${pricing.input}/$${pricing.output}`;
}

export function ProvidersSection({ doc }: { doc: SettingsDocument }) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  const providerFocus = useSettings((state) => state.providerFocus);

  const providers = useMemo(() => {
    const value = getPath(doc.effective, ["providers"]);
    return isObject(value) ? Object.keys(value).toSorted() : [];
  }, [doc.effective]);

  const defaultsCount = useMemo(() => {
    const value = getPath(doc.effective, ["defaults", "models"]);
    return isObject(value) ? Object.keys(value).length : 0;
  }, [doc.effective]);

  const [selected, setSelected] = useState<string | null>(null);
  const [mode, setMode] = useState<ProviderMode>("detail");
  const [editingModel, setEditingModel] = useState<string | null>(null);
  const [autoDiscover, setAutoDiscover] = useState<string | null>(null);

  useEffect(() => {
    if (!providerFocus) return;
    if (providerFocus.providerId === "__defaults__") {
      setMode("defaults");
      setEditingModel(null);
    } else if (providers.includes(providerFocus.providerId)) {
      setSelected(providerFocus.providerId);
      setMode("detail");
      setEditingModel(providerFocus.modelId ?? null);
    }
    useSettings.getState().clearProviderFocus();
  }, [providerFocus, providers]);

  const current = selected && providers.includes(selected) ? selected : (providers[0] ?? null);
  const options = apiOptions(doc);

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border-subtle pb-3">
        <div className="flex max-h-24 min-w-0 flex-1 flex-wrap items-center gap-1 overflow-y-auto">
          {providers.map((id) => {
            const provider = getPath(doc.effective, ["providers", id]);
            const enabled = !isObject(provider) || provider.enabled !== false;
            const inherited =
              scope === "project" && getPath(doc.document, ["providers", id]) === undefined;
            const count =
              isObject(provider) && isObject(provider.models)
                ? Object.keys(provider.models).length
                : 0;
            const active = mode === "detail" && id === current;
            return (
              <button
                key={id}
                type="button"
                onClick={() => {
                  setSelected(id);
                  setMode("detail");
                  setEditingModel(null);
                }}
                className={cn(
                  "flex h-8 shrink-0 items-center gap-1.5 rounded-lg px-2.5 text-ui transition-colors",
                  active
                    ? "bg-accent font-medium text-foreground"
                    : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                  !enabled && "opacity-60",
                )}
              >
                <span
                  className={cn(
                    "size-1.5 shrink-0 rounded-full",
                    enabled ? "bg-success" : "bg-text-tertiary",
                  )}
                />
                <span className="max-w-36 truncate">{id}</span>
                <span className="font-mono text-2xs text-text-tertiary tabular-nums">{count}</span>
                {inherited && <span className="text-2xs text-text-tertiary">· G</span>}
              </button>
            );
          })}
        </div>

        <div className="flex shrink-0 items-center gap-1">
          <button
            type="button"
            onClick={() => {
              setMode("defaults");
              setEditingModel(null);
            }}
            className={cn(
              "flex h-8 shrink-0 items-center gap-1.5 rounded-lg px-2.5 text-xs transition-colors",
              mode === "defaults"
                ? "bg-accent font-medium text-foreground"
                : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
            )}
          >
            <SlidersHorizontalIcon className="size-3.5" />
            <span>{t("sp.defaults_btn")}</span>
            {defaultsCount > 0 && (
              <span className="font-mono text-2xs text-text-tertiary tabular-nums">
                {defaultsCount}
              </span>
            )}
          </button>
          <Button
            type="button"
            size="xs"
            variant={mode === "add" ? "secondary" : "outline"}
            onClick={() => {
              setMode("add");
              setEditingModel(null);
            }}
          >
            <PlusIcon className="size-3.5" />
            {t("sp.add")}
          </Button>
        </div>
      </div>

      {mode === "defaults" ? (
        <ModelDefaultsView doc={doc} onBack={current ? () => setMode("detail") : undefined} />
      ) : mode === "add" || !current ? (
        <AddProvider
          options={options}
          existing={providers}
          onCancel={providers.length > 0 ? () => setMode("detail") : undefined}
          onCreated={(id, shouldFetch) => {
            setMode("detail");
            setSelected(id);
            setEditingModel(null);
            setAutoDiscover(shouldFetch ? id : null);
          }}
        />
      ) : (
        <ProviderDetail
          key={current}
          id={current}
          doc={doc}
          options={options}
          existingProviders={providers}
          editingModel={editingModel}
          onEditModel={setEditingModel}
          autoDiscover={autoDiscover === current}
          onConsumeAutoDiscover={() => setAutoDiscover(null)}
          onCloned={(newId) => {
            setSelected(newId);
            setEditingModel(null);
          }}
          onDeleted={() => {
            setSelected(null);
            setEditingModel(null);
          }}
        />
      )}
    </div>
  );
}

// ---------- 新建供应商（带服务商预设、前端校验、保存前测通、仅添加 / 添加并拉取） ----------

function AddProvider({
  options,
  existing,
  onCancel,
  onCreated,
}: {
  options: ChoiceOption[];
  existing: string[];
  onCancel?: () => void;
  onCreated: (id: string, shouldFetch: boolean) => void;
}) {
  const t = useT();
  const [presetKey, setPresetKey] = useState("custom");
  const [id, setId] = useState("");
  const [api, setApi] = useState(options[0]?.value ?? "openai_chat");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [azureVersion, setAzureVersion] = useState("");
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<ProviderTestResult | null>(null);

  const applyPreset = (preset: ProviderPreset) => {
    setPresetKey(preset.key);
    setTestResult(null);
    if (preset.key === "custom") {
      setAzureVersion("");
      return;
    }
    const suggestedId = existing.includes(preset.id) ? nextCopyId(preset.id, existing) : preset.id;
    setId(suggestedId);
    setApi(preset.api);
    setBaseUrl(preset.baseUrl);
    setApiKey(preset.apiKeyHint);
    setAzureVersion(preset.azureApiVersion ?? "");
  };

  const trimmed = id.trim();
  const trimmedUrl = baseUrl.trim();
  const idInvalid = trimmed && !/^[A-Za-z0-9_-]+$/.test(trimmed) ? t("sp.id_invalid") : null;
  const idDuplicate = existing.includes(trimmed) ? t("sp.id_dup") : null;
  const urlInvalid = trimmedUrl && !isValidBaseUrl(trimmedUrl) ? t("sp.url_scheme_err") : null;
  const azureInvalid =
    presetKey === "azure" && (!azureVersion.trim() || /\s/.test(azureVersion.trim()))
      ? t("sp.azure_version_invalid")
      : null;
  const ready =
    Boolean(trimmed && trimmedUrl) && !idInvalid && !idDuplicate && !urlInvalid && !azureInvalid;

  const buildPayload = () => {
    const payload: Record<string, unknown> = {
      api,
      base_url: trimmedUrl,
      api_key: apiKey,
      models: {},
    };
    if (presetKey === "azure" && azureVersion.trim()) {
      payload.endpoint = { Azure: { api_version: azureVersion.trim() } };
    }
    return payload;
  };

  const runTest = async () => {
    if (!trimmedUrl || urlInvalid) return;
    setTesting(true);
    setTestResult(null);
    try {
      const res = await testConnection(trimmed || "preview", buildPayload());
      setTestResult(res);
    } catch (error) {
      setTestResult({
        ok: false,
        latency_ms: 0,
        endpoint: trimmedUrl,
        message: errorText(error),
      });
    } finally {
      setTesting(false);
    }
  };

  const create = async (shouldFetch: boolean) => {
    if (!ready) return;
    setBusy(true);
    const payload = buildPayload();
    const ok = await writeSettings(
      (doc) => setPath(doc, ["providers", trimmed], payload),
      t("sp.added", { id: trimmed }),
    );
    setBusy(false);
    if (ok) onCreated(trimmed, shouldFetch);
  };

  return (
    <div className="space-y-5">
      <div>
        <h3 className="text-title font-semibold">{t("sp.add")}</h3>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">{t("sp.add_desc")}</p>
      </div>

      <div className="space-y-1.5">
        <span className="text-xs font-medium text-text-tertiary">{t("sp.presets")}</span>
        <div className="flex flex-wrap gap-1.5">
          {PROVIDER_PRESETS.map((preset) => (
            <button
              key={preset.key}
              type="button"
              onClick={() => applyPreset(preset)}
              className={cn(
                "h-7 rounded-lg px-2.5 text-xs transition-colors",
                presetKey === preset.key
                  ? "bg-accent font-medium text-foreground"
                  : "text-muted-foreground ring-1 ring-inset ring-border-subtle hover:bg-surface-hover hover:text-foreground",
              )}
            >
              {preset.label}
            </button>
          ))}
        </div>
      </div>

      <div className="space-y-3.5 rounded-xl bg-card p-4 ring-1 ring-inset ring-border-subtle">
        <Labeled
          label={t("sp.id")}
          hint={idInvalid ?? idDuplicate ?? t("sp.id_hint")}
          error={Boolean(idInvalid ?? idDuplicate)}
        >
          <Input
            value={id}
            onChange={(event) => setId(event.target.value)}
            placeholder={t("sp.id_ph")}
            spellCheck={false}
            className="h-8 rounded-lg text-ui shadow-none"
          />
        </Labeled>
        <Labeled label={t("sp.api")} hint={t("sp.api_desc")}>
          <Select value={api} onValueChange={setApi}>
            <SelectTrigger size="sm" className="h-8 w-full rounded-lg text-ui shadow-none">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {options.map((option) => (
                <SelectItem key={option.value} value={option.value}>
                  {option.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Labeled>
        <Labeled
          label={t("sp.base_url")}
          hint={urlInvalid ?? t("sp.base_url_desc")}
          error={Boolean(urlInvalid)}
        >
          <Input
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            placeholder="https://api.example.com/v1"
            className="h-8 rounded-lg font-mono text-xs shadow-none"
            spellCheck={false}
          />
        </Labeled>
        {presetKey === "azure" && (
          <Labeled
            label="api_version"
            hint={azureInvalid ?? t("sp.azure_version_hint")}
            error={Boolean(azureInvalid)}
          >
            <Input
              value={azureVersion}
              onChange={(event) => setAzureVersion(event.target.value)}
              placeholder="2025-04-01-preview"
              className="h-8 rounded-lg font-mono text-xs shadow-none"
              spellCheck={false}
            />
          </Labeled>
        )}
        <Labeled label={t("sp.api_key")} hint={t("sp.api_key_hint")}>
          <ApiKeyInput
            value={apiKey}
            onChange={setApiKey}
            placeholder="env:OPENAI_API_KEY"
            ariaLabel={t("sp.api_key")}
          />
        </Labeled>
      </div>

      {testResult && <TestResultBanner result={testResult} onClose={() => setTestResult(null)} />}

      <div className="flex flex-wrap items-center justify-between gap-2">
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={!trimmedUrl || Boolean(urlInvalid) || testing}
          onClick={() => void runTest()}
        >
          {testing ? <Loader2Icon className="animate-spin" /> : <ActivityIcon />}
          {t("sp.test_conn")}
        </Button>

        <div className="flex items-center gap-2">
          {onCancel && (
            <Button variant="ghost" size="sm" onClick={onCancel}>
              {t("common.cancel")}
            </Button>
          )}
          <Button
            variant="outline"
            size="sm"
            disabled={!ready || busy}
            onClick={() => void create(false)}
          >
            {t("sp.add_only")}
          </Button>
          <Button size="sm" disabled={!ready || busy} onClick={() => void create(true)}>
            {busy && <Loader2Icon className="animate-spin" />}
            {t("sp.add_fetch")}
          </Button>
        </div>
      </div>
    </div>
  );
}

function Labeled({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: string;
  error?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div className="grid grid-cols-1 items-start gap-3 md:grid-cols-[110px_minmax(0,1fr)]">
      <span className="pt-1.5 text-ui text-muted-foreground">{label}</span>
      <div className="min-w-0 space-y-1">
        {children}
        {hint && (
          <p className={cn("text-xs", error ? "text-destructive" : "text-text-tertiary")}>{hint}</p>
        )}
      </div>
    </div>
  );
}

function TestResultBanner({
  result,
  onClose,
}: {
  result: ProviderTestResult;
  onClose: () => void;
}) {
  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-xl px-3.5 py-2.5 text-xs",
        result.ok ? "bg-success/10 text-success" : "bg-destructive/10 text-destructive",
      )}
    >
      {result.ok ? (
        <CheckCircle2Icon className="mt-0.5 size-3.5 shrink-0" />
      ) : (
        <XCircleIcon className="mt-0.5 size-3.5 shrink-0" />
      )}
      <div className="min-w-0 flex-1 space-y-0.5">
        <p className="break-words">{result.message}</p>
        {result.endpoint && (
          <p className="truncate font-mono text-2xs opacity-75">{result.endpoint}</p>
        )}
      </div>
      <button
        type="button"
        onClick={onClose}
        className="shrink-0 rounded p-0.5 opacity-70 hover:opacity-100"
        aria-label="Close"
      >
        <XIcon className="size-3.5" />
      </button>
    </div>
  );
}

// ---------- 供应商详情（Models / Connection / Params 子页签） ----------

function ProviderDetail({
  id,
  doc,
  options,
  existingProviders,
  editingModel,
  onEditModel,
  autoDiscover,
  onConsumeAutoDiscover,
  onCloned,
  onDeleted,
}: {
  id: string;
  doc: SettingsDocument;
  options: ChoiceOption[];
  existingProviders: string[];
  editingModel: string | null;
  onEditModel: (modelId: string | null) => void;
  autoDiscover: boolean;
  onConsumeAutoDiscover: () => void;
  onCloned: (newId: string) => void;
  onDeleted: () => void;
}) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  const [tab, setTab] = useState<DetailTab>("models");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [cloning, setCloning] = useState(false);
  const [cloneName, setCloneName] = useState("");
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<ProviderTestResult | null>(null);
  const [modelQuery, setModelQuery] = useState("");
  const [selectedModels, setSelectedModels] = useState<Set<string>>(new Set());
  const [confirmBatchDelete, setConfirmBatchDelete] = useState(false);
  const [discovered, setDiscovered] = useState<{
    busy: boolean;
    models: CatalogModel[] | null;
    error: string | null;
  }>({
    busy: false,
    models: null,
    error: null,
  });

  const explicitObj = getPath(doc.document, ["providers", id]);
  const explicit = isObject(explicitObj);
  const inherited = scope === "project" && !explicit;
  const provider = getPath(doc.effective, ["providers", id]);
  const providerObj = isObject(provider) ? provider : {};
  const enabled = providerObj.enabled !== false;
  const models = useMemo(
    () => (isObject(providerObj.models) ? Object.keys(providerObj.models).toSorted() : []),
    [providerObj.models],
  );

  const apiLabel =
    options.find((opt) => opt.value === providerObj.api)?.label ??
    (typeof providerObj.api === "string" ? providerObj.api : t("sp.no_api"));
  const baseUrlStr = typeof providerObj.base_url === "string" ? providerObj.base_url : "";
  const apiKeyStr = typeof providerObj.api_key === "string" ? providerObj.api_key : "";

  const connection = useMemo(
    () =>
      doc.fields.filter(
        (field) =>
          field.path[0] === "providers" &&
          field.path[1] === id &&
          field.path[2] !== "models" &&
          !(field.path.length === 3 && field.path[2] === "enabled"),
      ),
    [doc.fields, id],
  );

  const primaryConn = connection.filter((field) => PRIMARY_CONN[field.path[2]]);
  const paramFields = connection.filter(
    (field) =>
      field.path.length > 3 && (field.path[2] === "extra_body" || field.path[2] === "headers"),
  );
  const rawParamFields = connection.filter(
    (field) =>
      field.path.length === 3 && (field.path[2] === "extra_body" || field.path[2] === "headers"),
  );
  const headersField = rawParamFields.find((field) => field.path[2] === "headers");
  const secondaryConn = connection.filter(
    (field) =>
      !PRIMARY_CONN[field.path[2]] &&
      !paramFields.includes(field) &&
      !rawParamFields.includes(field),
  );

  const configuredParamCount = useMemo(
    () =>
      paramFields.filter(
        (field) =>
          getPath(doc.document, field.path) !== undefined ||
          getPath(doc.inherited, field.path) !== undefined,
      ).length,
    [paramFields, doc],
  );

  const runDiscover = async () => {
    setTab("models");
    setDiscovered((state) => ({ ...state, busy: true, error: null }));
    const latest = useSettings.getState().docs[useSettings.getState().scope];
    try {
      const list = await discover(id, getPath(latest?.effective, ["providers", id]) ?? provider);
      setDiscovered({ busy: false, models: list, error: null });
    } catch (error) {
      setDiscovered({ busy: false, models: null, error: errorText(error) });
    }
  };

  const runTest = async () => {
    setTesting(true);
    setTestResult(null);
    const latest = useSettings.getState().docs[useSettings.getState().scope];
    try {
      const res = await testConnection(
        id,
        getPath(latest?.effective, ["providers", id]) ?? provider,
      );
      setTestResult(res);
    } catch (error) {
      setTestResult({
        ok: false,
        latency_ms: 0,
        endpoint: baseUrlStr,
        message: errorText(error),
      });
    } finally {
      setTesting(false);
    }
  };

  // biome-ignore lint/correctness/useExhaustiveDependencies: 仅在新建后首次挂载时自动拉取
  useEffect(() => {
    if (autoDiscover) {
      onConsumeAutoDiscover();
      void runDiscover();
    }
  }, [autoDiscover]);

  // 钻取至模型编辑子视图
  if (editingModel && models.includes(editingModel)) {
    const modelPath = ["providers", id, "models", editingModel];
    const modelInherited = scope === "project" && getPath(doc.document, modelPath) === undefined;
    return (
      <ModelEditor
        doc={doc}
        basePath={modelPath}
        title={editingModel}
        subtitle={`${id} · ${apiLabel}`}
        inherited={modelInherited}
        siblingIds={models}
        onBack={() => onEditModel(null)}
        onCloned={(newId) => onEditModel(newId)}
        onDeleted={() => onEditModel(null)}
      />
    );
  }

  const removeProvider = async () => {
    const previous = explicitObj;
    const ok = await writeSettingsWithUndo(
      (current) => setPath(current, ["providers", id], undefined),
      (current) => setPath(current, ["providers", id], previous),
      t("sp.deleted", { id }),
      t("sp.provider_restored", { id }),
    );
    if (ok) onDeleted();
  };

  const cloneProvider = async () => {
    const trimmed = cloneName.trim();
    if (!trimmed || !/^[A-Za-z0-9_-]+$/.test(trimmed) || existingProviders.includes(trimmed))
      return;
    const source = isObject(explicitObj)
      ? structuredClone(explicitObj)
      : structuredClone(providerObj);
    const ok = await writeSettings(
      (current) => setPath(current, ["providers", trimmed], source),
      t("sp.provider_cloned", { id: trimmed }),
    );
    if (ok) {
      setCloning(false);
      setCloneName("");
      onCloned(trimmed);
    }
  };

  const filteredModels = models.filter(
    (m) => !modelQuery.trim() || m.toLowerCase().includes(modelQuery.trim().toLowerCase()),
  );

  const toggleSelectModel = (modelId: string) => {
    setSelectedModels((prev) => {
      const next = new Set(prev);
      if (next.has(modelId)) next.delete(modelId);
      else next.add(modelId);
      return next;
    });
  };

  const batchPatch = async (patch: Record<string, unknown>, label: string) => {
    const ids = [...selectedModels].filter((m) => models.includes(m));
    if (ids.length === 0) return;
    await writeSettings((current) => {
      let next = current;
      for (const modelId of ids) {
        for (const [k, v] of Object.entries(patch)) {
          next = setPath(next, ["providers", id, "models", modelId, k], v);
        }
      }
      return next;
    }, label);
    setSelectedModels(new Set());
  };

  const batchDelete = async () => {
    const ids = [...selectedModels].filter(
      (m) => getPath(doc.document, ["providers", id, "models", m]) !== undefined,
    );
    if (ids.length === 0) return;
    const backup = ids.map(
      (m) => [m, getPath(doc.document, ["providers", id, "models", m])] as const,
    );
    const ok = await writeSettingsWithUndo(
      (current) => {
        let next = current;
        for (const m of ids) next = setPath(next, ["providers", id, "models", m], undefined);
        return next;
      },
      (current) => {
        let next = current;
        for (const [m, prev] of backup) next = setPath(next, ["providers", id, "models", m], prev);
        return next;
      },
      t("sp.batch_deleted", { n: ids.length }),
      t("sp.batch_restored", { n: ids.length }),
    );
    if (ok) {
      setSelectedModels(new Set());
      setConfirmBatchDelete(false);
    }
  };

  const explicitSelectedCount = [...selectedModels].filter(
    (m) => getPath(doc.document, ["providers", id, "models", m]) !== undefined,
  ).length;

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="truncate text-title font-semibold text-foreground">{id}</h3>
            <span className="text-xs text-text-tertiary">{apiLabel}</span>
            {inherited && <Badge variant="secondary">Global</Badge>}
            {!enabled && <Badge variant="secondary">{t("sp.disabled")}</Badge>}
          </div>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            {explicit ? t("sp.explicit_note") : t("sp.inherited_note")}
          </p>
        </div>

        <div className="flex shrink-0 flex-wrap items-center gap-2">
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <span>{t("sp.enabled")}</span>
            <Switch
              checked={enabled}
              onCheckedChange={(checked) =>
                void writeField(["providers", id, "enabled"], checked ? undefined : false)
              }
              aria-label={t("sp.enabled")}
            />
          </div>
          <Button
            type="button"
            size="xs"
            variant="outline"
            disabled={testing}
            onClick={() => void runTest()}
          >
            {testing ? <Loader2Icon className="animate-spin" /> : <ActivityIcon />}
            {t("sp.test_conn")}
          </Button>
          <IconButton
            label={t("sp.clone_provider")}
            size="sm"
            onClick={() => {
              setCloneName(nextCopyId(id, existingProviders));
              setCloning((prev) => !prev);
            }}
          >
            <CopyIcon />
          </IconButton>
          {explicit &&
            (confirmDelete ? (
              <div className="flex items-center gap-1">
                <Button size="xs" variant="ghost" onClick={() => setConfirmDelete(false)}>
                  {t("common.cancel")}
                </Button>
                <Button size="xs" variant="destructive" onClick={() => void removeProvider()}>
                  {t("sp.confirm_delete")}
                </Button>
              </div>
            ) : (
              <IconButton label={t("sp.delete")} size="sm" onClick={() => setConfirmDelete(true)}>
                <Trash2Icon />
              </IconButton>
            ))}
        </div>
      </div>

      {cloning && (
        <div className="flex flex-wrap items-center gap-2 rounded-xl bg-card p-3 ring-1 ring-inset ring-border-subtle">
          <span className="text-xs text-muted-foreground">{t("sp.clone_as")}</span>
          <Input
            value={cloneName}
            onChange={(event) => setCloneName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void cloneProvider();
              if (event.key === "Escape") setCloning(false);
            }}
            spellCheck={false}
            className="h-7 min-w-44 flex-1 rounded-lg font-mono text-xs shadow-none"
            autoFocus
          />
          <Button size="xs" variant="ghost" onClick={() => setCloning(false)}>
            {t("common.cancel")}
          </Button>
          <Button
            size="xs"
            disabled={
              !cloneName.trim() ||
              !/^[A-Za-z0-9_-]+$/.test(cloneName.trim()) ||
              existingProviders.includes(cloneName.trim())
            }
            onClick={() => void cloneProvider()}
          >
            {t("sp.clone")}
          </Button>
        </div>
      )}

      {testResult && <TestResultBanner result={testResult} onClose={() => setTestResult(null)} />}

      <div className="flex flex-wrap items-center gap-1 border-b border-border-subtle pb-2">
        {(
          [
            { id: "models", label: `${t("sp.tab_models")} (${models.length})` },
            { id: "connection", label: t("sp.connection") },
            {
              id: "params",
              label:
                configuredParamCount > 0
                  ? `${t("sp.tab_params")} (${configuredParamCount})`
                  : t("sp.tab_params"),
            },
          ] as { id: DetailTab; label: string }[]
        ).map((item) => (
          <button
            key={item.id}
            type="button"
            onClick={() => setTab(item.id)}
            className={cn(
              "h-7 rounded-md px-2.5 text-xs transition-colors",
              tab === item.id
                ? "bg-accent font-medium text-foreground"
                : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
            )}
          >
            {item.label}
          </button>
        ))}
      </div>

      {tab === "models" && (
        <div className="space-y-4">
          <div className="flex flex-wrap items-center justify-between gap-2 rounded-xl bg-card px-3.5 py-2.5 text-xs ring-1 ring-inset ring-border-subtle">
            <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 text-muted-foreground">
              <span className="truncate font-mono text-foreground">
                {baseUrlStr || t("sf.unset")}
              </span>
              <span className="text-text-tertiary">·</span>
              <span className="font-mono text-text-tertiary">
                {!apiKeyStr
                  ? t("sp.key_none")
                  : apiKeyStr.startsWith("env:") || apiKeyStr.startsWith("!")
                    ? apiKeyStr
                    : t("sp.key_set")}
              </span>
            </div>
            <button
              type="button"
              onClick={() => setTab("connection")}
              className="shrink-0 text-xs text-brand hover:underline"
            >
              {t("sp.edit_connection")}
            </button>
          </div>

          <div className="flex flex-wrap items-center justify-between gap-2">
            {models.length > 0 ? (
              <label className="flex h-8 min-w-44 flex-1 items-center gap-2 rounded-lg bg-card px-2.5 ring-1 ring-inset ring-border-subtle">
                <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
                <input
                  value={modelQuery}
                  onChange={(event) => setModelQuery(event.target.value)}
                  placeholder={t("sp.filter_models_ph", { n: models.length })}
                  className="min-w-0 flex-1 bg-transparent text-ui outline-none placeholder:text-text-tertiary"
                />
                {modelQuery && (
                  <button
                    type="button"
                    onClick={() => setModelQuery("")}
                    className="text-text-tertiary hover:text-foreground"
                    aria-label="Clear"
                  >
                    <XIcon className="size-3.5" />
                  </button>
                )}
              </label>
            ) : (
              <span className="text-xs font-medium text-text-tertiary">
                {t("sp.models_list", { n: 0 })}
              </span>
            )}
            <Button
              size="xs"
              variant="outline"
              disabled={discovered.busy}
              onClick={() => void runDiscover()}
            >
              {discovered.busy ? <Loader2Icon className="animate-spin" /> : <DownloadCloudIcon />}
              {t("sp.fetch")}
            </Button>
          </div>

          {selectedModels.size > 0 && (
            <div className="flex flex-wrap items-center justify-between gap-2 rounded-xl bg-accent/60 px-3 py-2 text-xs">
              <span className="font-medium text-foreground">
                {t("sp.batch_selected", { n: selectedModels.size })}
              </span>
              <div className="flex flex-wrap items-center gap-1.5">
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() => void batchPatch({ reasoning: true }, t("sp.batch_updated"))}
                >
                  + Reasoning
                </Button>
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() =>
                    void batchPatch({ input: ["text", "image"] }, t("sp.batch_updated"))
                  }
                >
                  + Image
                </Button>
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() => void batchPatch({ enabled: false }, t("sp.batch_updated"))}
                >
                  {t("sp.disable")}
                </Button>
                <Button
                  size="xs"
                  variant="outline"
                  onClick={() => void batchPatch({ enabled: undefined }, t("sp.batch_updated"))}
                >
                  {t("sp.enable")}
                </Button>
                {explicitSelectedCount > 0 &&
                  (confirmBatchDelete ? (
                    <>
                      <Button
                        size="xs"
                        variant="ghost"
                        onClick={() => setConfirmBatchDelete(false)}
                      >
                        {t("common.cancel")}
                      </Button>
                      <Button size="xs" variant="destructive" onClick={() => void batchDelete()}>
                        {t("sp.confirm_delete")} ({explicitSelectedCount})
                      </Button>
                    </>
                  ) : (
                    <Button
                      size="xs"
                      variant="ghost"
                      className="text-destructive hover:text-destructive"
                      onClick={() => setConfirmBatchDelete(true)}
                    >
                      <Trash2Icon className="size-3.5" />
                      {t("sp.remove_model")}
                    </Button>
                  ))}
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() => {
                    setSelectedModels(new Set());
                    setConfirmBatchDelete(false);
                  }}
                >
                  {t("common.cancel")}
                </Button>
              </div>
            </div>
          )}

          {discovered.error && (
            <div className="rounded-xl bg-destructive/10 px-3.5 py-2.5 text-xs text-destructive">
              {t("sp.fetch_failed", { error: discovered.error })}
            </div>
          )}

          {discovered.models && (
            <DiscoveryPanel
              providerId={id}
              candidates={discovered.models}
              existing={new Set(models)}
              onClose={() => setDiscovered({ busy: false, models: null, error: null })}
            />
          )}

          <div className="divide-y divide-border-subtle overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle">
            {models.length === 0 ? (
              <p className="px-4 py-6 text-center text-ui text-text-tertiary">
                {t("sp.no_models")}
              </p>
            ) : filteredModels.length === 0 ? (
              <p className="px-4 py-6 text-center text-ui text-text-tertiary">
                {t("sp.no_model_match")}
              </p>
            ) : (
              filteredModels.map((modelId) => (
                <ModelRow
                  key={modelId}
                  providerId={id}
                  modelId={modelId}
                  doc={doc}
                  checked={selectedModels.has(modelId)}
                  onToggleCheck={() => toggleSelectModel(modelId)}
                  onOpen={() => onEditModel(modelId)}
                  siblingIds={models}
                  onCloned={(newId) => onEditModel(newId)}
                />
              ))
            )}
            <ManualAdd providerId={id} existing={models} onAdded={(m) => onEditModel(m)} />
          </div>
        </div>
      )}

      {tab === "connection" && (
        <div className="space-y-5">
          <FieldList fields={primaryConn} doc={doc} strip={2} showGroups={false} />
          {secondaryConn.length > 0 && (
            <FieldList fields={secondaryConn} doc={doc} strip={2} includeAdvanced />
          )}
        </div>
      )}

      {tab === "params" && (
        <div className="space-y-6">
          {headersField && (
            <section className="space-y-2">
              <h4 className="text-xs font-medium text-text-tertiary">Headers</h4>
              <div className="rounded-xl bg-card p-4 ring-1 ring-inset ring-border-subtle">
                <KeyValueEditor
                  path={headersField.path}
                  explicit={getPath(doc.document, headersField.path)}
                  fallback={getPath(doc.inherited, headersField.path)}
                />
              </div>
            </section>
          )}
          <RequestParamsEditor
            fields={paramFields}
            rawFields={rawParamFields}
            doc={doc}
            renderField={(field) => <FieldRow field={field} doc={doc} />}
          />
        </div>
      )}
    </div>
  );
}

// ---------- 模型列表行（精简能力徽标、启用开关、克隆、带二次确认+撤回的删除） ----------

function ModelRow({
  providerId,
  modelId,
  doc,
  checked,
  onToggleCheck,
  onOpen,
  siblingIds,
  onCloned,
}: {
  providerId: string;
  modelId: string;
  doc: SettingsDocument;
  checked: boolean;
  onToggleCheck: () => void;
  onOpen: () => void;
  siblingIds: string[];
  onCloned: (newId: string) => void;
}) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  const [confirmDelete, setConfirmDelete] = useState(false);

  const path = ["providers", providerId, "models", modelId];
  const model = getPath(doc.effective, path);
  const info = isObject(model) ? model : {};
  const explicitObj = getPath(doc.document, path);
  const explicit = explicitObj !== undefined;
  const inherited = scope === "project" && !explicit;
  const enabled = info.enabled !== false;
  const imageGen = info.image_gen === true;
  const context =
    !imageGen && typeof info.context_window === "number" ? info.context_window : undefined;
  const maxOut =
    !imageGen && typeof info.max_output_tokens === "number" ? info.max_output_tokens : undefined;
  const reasoning = !imageGen && info.reasoning === true;
  const image = !imageGen && Array.isArray(info.input) && info.input.includes("image");
  const fast = !imageGen && info.fast_mode === true;
  const priceText = !imageGen ? formatPriceBadge(info.pricing) : null;

  const remove = async () => {
    const previous = explicitObj;
    await writeSettingsWithUndo(
      (current) => setPath(current, path, undefined),
      (current) => setPath(current, path, previous),
      t("sp.model_deleted", { id: modelId }),
      t("sp.model_restored", { id: modelId }),
    );
    setConfirmDelete(false);
  };

  const clone = async () => {
    const targetId = nextCopyId(modelId, siblingIds);
    const source = isObject(explicitObj) ? structuredClone(explicitObj) : structuredClone(info);
    const ok = await writeSettings(
      (current) => setPath(current, ["providers", providerId, "models", targetId], source),
      t("sp.model_cloned", { id: targetId }),
    );
    if (ok) onCloned(targetId);
  };

  return (
    <div
      className={cn(
        "group/model flex min-h-11 items-center gap-2.5 px-3 py-1.5 transition-colors hover:bg-surface-hover",
        !enabled && "opacity-60",
      )}
    >
      <Checkbox checked={checked} onCheckedChange={onToggleCheck} aria-label={modelId} />
      <button
        type="button"
        onClick={onOpen}
        className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2 gap-y-0.5 text-left"
      >
        <span className="min-w-0 truncate font-mono text-xs font-medium text-foreground">
          {modelId}
        </span>
        {inherited && <Badge variant="secondary">Global</Badge>}
        {!enabled && <Badge variant="secondary">{t("sp.disabled")}</Badge>}
        <span className="ml-auto flex shrink-0 items-center gap-2 text-text-tertiary">
          {imageGen && <Badge variant="brand">Image</Badge>}
          {reasoning && <SparklesIcon className="size-3.5" aria-label="Reasoning" />}
          {image && <ImageIcon className="size-3.5" aria-label={t("mp.images")} />}
          {fast && <ZapIcon className="size-3.5" aria-label="Fast Mode" />}
          {priceText && (
            <span className="hidden font-mono text-2xs tabular-nums sm:inline">{priceText}</span>
          )}
          {context && (
            <span className="font-mono text-xs tabular-nums">
              {compactTokens(context)}
              {maxOut ? ` / ${compactTokens(maxOut)}` : ""}
            </span>
          )}
        </span>
      </button>

      {confirmDelete ? (
        <div className="flex shrink-0 items-center gap-1">
          <Button size="xs" variant="ghost" onClick={() => setConfirmDelete(false)}>
            {t("common.cancel")}
          </Button>
          <Button size="xs" variant="destructive" onClick={() => void remove()}>
            {t("sp.confirm_delete")}
          </Button>
        </div>
      ) : (
        <div className="flex shrink-0 items-center gap-1">
          <Switch
            checked={enabled}
            onCheckedChange={(val) =>
              void writeField([...path, "enabled"], val ? undefined : false)
            }
            aria-label={`${modelId} ${t("sp.enabled")}`}
          />
          <IconButton label={t("sp.edit_model")} size="xs" onClick={onOpen}>
            <PencilIcon />
          </IconButton>
          <IconButton label={t("sp.clone_model")} size="xs" onClick={() => void clone()}>
            <CopyIcon />
          </IconButton>
          {explicit && (
            <IconButton
              label={t("sp.remove_model")}
              size="xs"
              onClick={() => setConfirmDelete(true)}
            >
              <Trash2Icon />
            </IconButton>
          )}
        </div>
      )}
    </div>
  );
}

// ---------- 手动添加模型（自动按模型系列推断能力默认值） ----------

function ManualAdd({
  providerId,
  existing,
  onAdded,
}: {
  providerId: string;
  existing: string[];
  onAdded: (modelId: string) => void;
}) {
  const t = useT();
  const [value, setValue] = useState("");
  const id = value.trim();
  const add = () => {
    if (!id || existing.includes(id)) return;
    const entry = inferModelEntry(id);
    void writeSettings((doc) => setPath(doc, ["providers", providerId, "models", id], entry)).then(
      (ok) => {
        if (ok) {
          setValue("");
          onAdded(id);
        }
      },
    );
  };
  return (
    <div className="flex items-center gap-2 px-3 py-2">
      <PlusIcon className="size-3.5 shrink-0 text-text-tertiary" />
      <input
        value={value}
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={(event) => event.key === "Enter" && add()}
        placeholder={t("sp.manual_ph")}
        spellCheck={false}
        className="h-7 min-w-0 flex-1 bg-transparent font-mono text-xs outline-none placeholder:font-sans placeholder:text-text-tertiary"
      />
      {id && (
        <Button size="xs" variant="ghost" disabled={existing.includes(id)} onClick={add}>
          {existing.includes(id) ? t("sp.exists") : t("common.add")}
        </Button>
      )}
    </div>
  );
}

// ---------- 模型发现面板（元数据徽标、分类过滤、添加后不强制关闭） ----------

type DiscoveryFilter = "all" | "new" | "reasoning" | "vision";

function DiscoveryPanel({
  providerId,
  candidates,
  existing,
  onClose,
}: {
  providerId: string;
  candidates: CatalogModel[];
  existing: Set<string>;
  onClose: () => void;
}) {
  const t = useT();
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<DiscoveryFilter>("all");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);

  const byId = useMemo(() => new Map(candidates.map((item) => [item.id, item])), [candidates]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return candidates.filter((model) => {
      if (filter === "new" && existing.has(model.id)) return false;
      if (filter === "reasoning" && !model.reasoning) return false;
      if (filter === "vision" && !model.input?.includes("image")) return false;
      if (!needle) return true;
      return `${model.id} ${model.name}`.toLowerCase().includes(needle);
    });
  }, [candidates, existing, filter, query]);

  const addable = visible.filter((model) => !existing.has(model.id));
  const allPicked = addable.length > 0 && addable.every((model) => picked.has(model.id));

  const toggle = (id: string) =>
    setPicked((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const add = async () => {
    const ids = [...picked].filter((id) => !existing.has(id));
    if (ids.length === 0) return;
    setBusy(true);
    const ok = await writeSettings(
      (doc) => {
        let next = doc;
        for (const id of ids) {
          const cand = byId.get(id);
          const entry = cand ? modelEntryFromCatalog(cand) : inferModelEntry(id);
          next = setPath(next, ["providers", providerId, "models", id], entry);
        }
        return next;
      },
      t("sp.models_added", { n: ids.length }),
    );
    setBusy(false);
    // 保留发现面板打开，方便继续按关键词添加下一批模型。
    if (ok) setPicked(new Set());
  };

  if (candidates.length === 0) {
    return (
      <div className="flex items-center justify-between rounded-xl bg-muted/60 px-3.5 py-2.5 text-xs text-muted-foreground">
        {t("sp.no_candidates")}
        <Button size="xs" variant="ghost" onClick={onClose}>
          {t("common.close")}
        </Button>
      </div>
    );
  }

  return (
    <div className="overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-brand/30">
      <div className="flex flex-wrap items-center gap-2 border-b border-border-subtle px-3 py-2">
        <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder={t("sp.search_ph", { n: candidates.length })}
          className="h-7 min-w-32 flex-1 bg-transparent text-ui outline-none placeholder:text-text-tertiary"
        />
        <div className="flex items-center gap-0.5">
          {(
            [
              ["all", t("sp.filter_all")],
              ["new", t("sp.filter_new")],
              ["reasoning", "Reasoning"],
              ["vision", "Vision"],
            ] as [DiscoveryFilter, string][]
          ).map(([key, label]) => (
            <button
              key={key}
              type="button"
              onClick={() => setFilter(key)}
              className={cn(
                "h-6 rounded-md px-2 text-2xs transition-colors",
                filter === key
                  ? "bg-accent font-medium text-foreground"
                  : "text-text-tertiary hover:text-foreground",
              )}
            >
              {label}
            </button>
          ))}
        </div>
        <button
          type="button"
          onClick={() =>
            setPicked((current) => {
              const next = new Set(current);
              for (const model of addable) {
                if (allPicked) next.delete(model.id);
                else next.add(model.id);
              }
              return next;
            })
          }
          disabled={addable.length === 0}
          className="shrink-0 text-xs text-muted-foreground hover:text-foreground disabled:opacity-40"
        >
          {allPicked ? t("sp.unselect_all") : t("sp.select_all")}
        </button>
      </div>

      <ul className="max-h-64 overflow-y-auto p-1">
        {visible.length === 0 ? (
          <li className="px-3 py-6 text-center text-xs text-text-tertiary">
            {t("sp.no_model_match")}
          </li>
        ) : (
          visible.map((model) => {
            const added = existing.has(model.id);
            const priceText = formatPriceBadge(model.pricing);
            return (
              <li key={model.id}>
                {/* biome-ignore lint/a11y/noLabelWithoutControl: Radix Checkbox 渲染为 button，被 label 包裹即可点整行切换 */}
                <label
                  className={cn(
                    "flex h-8 items-center gap-2.5 rounded-md px-2 text-ui",
                    added ? "opacity-55" : "cursor-pointer hover:bg-surface-hover",
                  )}
                >
                  {added ? (
                    <CheckIcon className="size-4 shrink-0 text-text-tertiary" />
                  ) : (
                    <Checkbox
                      checked={picked.has(model.id)}
                      onCheckedChange={() => toggle(model.id)}
                    />
                  )}
                  <span className="min-w-0 truncate font-mono text-xs">{model.id}</span>
                  {model.name && model.name !== model.id && (
                    <span className="hidden min-w-0 truncate text-xs text-text-tertiary sm:inline">
                      {model.name}
                    </span>
                  )}
                  <span className="ml-auto flex shrink-0 items-center gap-2 text-2xs text-text-tertiary">
                    {model.image_gen && <Badge variant="brand">Image</Badge>}
                    {model.reasoning && <SparklesIcon className="size-3" />}
                    {model.input?.includes("image") && <ImageIcon className="size-3" />}
                    {priceText && <span className="font-mono tabular-nums">{priceText}</span>}
                    {model.context_window && (
                      <span className="font-mono tabular-nums">
                        {compactTokens(model.context_window)}
                      </span>
                    )}
                    {added && <span>{t("sp.added_mark")}</span>}
                  </span>
                </label>
              </li>
            );
          })
        )}
      </ul>

      <div className="flex items-center justify-between gap-2 border-t border-border-subtle px-3 py-2">
        <span className="text-2xs text-text-tertiary">
          {t("sp.discovery_count", { shown: visible.length, total: candidates.length })}
        </span>
        <div className="flex items-center gap-2">
          <Button size="xs" variant="ghost" onClick={onClose}>
            {t("common.done")}
          </Button>
          <Button size="xs" disabled={picked.size === 0 || busy} onClick={() => void add()}>
            {busy && <Loader2Icon className="animate-spin" />}
            {picked.size > 0 ? t("sp.add_n", { n: picked.size }) : t("sp.add_models")}
          </Button>
        </div>
      </div>
    </div>
  );
}
