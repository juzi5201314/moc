// 供应商与模型：左侧供应商列表，右侧连接参数 + 模型列表。
// 新建供应商后自动向 `/models` 拉取候选，勾选即加入；每个模型可展开编辑元数据。

import {
  CheckIcon,
  ChevronRightIcon,
  DownloadCloudIcon,
  ImageIcon,
  Loader2Icon,
  PlusIcon,
  SearchIcon,
  Trash2Icon,
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
import { t, useT } from "@/lib/i18n";
import { compactTokens } from "@/lib/models";
import { getPath, setPath } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  CatalogModel,
  ChoiceOption,
  ConfigAction,
  ConfigReply,
  SettingsDocument,
} from "@/protocol/types";
import { FieldList } from "./field";
import { useSettings, writeSettings } from "./settings-store";

type Json = Record<string, unknown>;

/** 还没有任何供应商时 catalog 里拿不到 `api` 的选项，用 core `ProviderApi` 的三种取值兜底。 */
const API_FALLBACK: ChoiceOption[] = [
  { value: "openai_chat", label: "OpenAI Chat" },
  { value: "openai_responses", label: "OpenAI Responses" },
  { value: "anthropic_messages", label: "Anthropic Messages" },
];

/** 连接参数里默认展开的字段；其余收进「高级选项」。 */
const PRIMARY: Record<string, true> = { api: true, base_url: true, api_key: true };

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

export function ProvidersSection({ doc }: { doc: SettingsDocument }) {
  const t = useT();
  const providers = useMemo(() => {
    const value = getPath(doc.effective, ["providers"]);
    return isObject(value) ? Object.keys(value).toSorted() : [];
  }, [doc.effective]);
  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [autoDiscover, setAutoDiscover] = useState<string | null>(null);
  const current = selected && providers.includes(selected) ? selected : (providers[0] ?? null);
  const options = apiOptions(doc);

  return (
    <div className="flex min-h-[480px] flex-col gap-5 md:flex-row">
      <aside className="flex gap-1 overflow-x-auto md:block md:w-48 md:shrink-0 md:space-y-0.5">
        {providers.map((id) => {
          const provider = getPath(doc.effective, ["providers", id]);
          const models =
            isObject(provider) && isObject(provider.models)
              ? Object.keys(provider.models).length
              : 0;
          const api = isObject(provider)
            ? options.find((option) => option.value === provider.api)?.label
            : undefined;
          return (
            <button
              key={id}
              type="button"
              onClick={() => {
                setSelected(id);
                setAdding(false);
              }}
              className={cn(
                "flex max-w-48 shrink-0 flex-col rounded-lg px-3 py-2 text-left transition-colors md:w-full",
                id === current && !adding ? "bg-accent" : "hover:bg-surface-hover",
              )}
            >
              <span className="truncate text-ui font-medium text-foreground">{id}</span>
              <span className="truncate text-xs text-text-tertiary">
                {api ?? t("sp.no_api")} · {t("sp.models_n", { n: models })}
              </span>
            </button>
          );
        })}
        <button
          type="button"
          onClick={() => setAdding(true)}
          className={cn(
            "flex h-9 shrink-0 items-center gap-2 rounded-lg px-3 text-ui text-muted-foreground transition-colors hover:bg-surface-hover hover:text-foreground md:w-full",
            adding && "bg-accent text-foreground",
          )}
        >
          <PlusIcon className="size-4" />
          {t("sp.add")}
        </button>
      </aside>

      <div className="min-w-0 flex-1">
        {adding || !current ? (
          <AddProvider
            options={options}
            existing={providers}
            onCancel={providers.length > 0 ? () => setAdding(false) : undefined}
            onCreated={(id) => {
              setAdding(false);
              setSelected(id);
              setAutoDiscover(id);
            }}
          />
        ) : (
          <ProviderDetail
            key={current}
            id={current}
            doc={doc}
            autoDiscover={autoDiscover === current}
            onDeleted={() => setSelected(null)}
          />
        )}
      </div>
    </div>
  );
}

function AddProvider({
  options,
  existing,
  onCancel,
  onCreated,
}: {
  options: ChoiceOption[];
  existing: string[];
  onCancel?: () => void;
  onCreated: (id: string) => void;
}) {
  const t = useT();
  const [id, setId] = useState("");
  const [api, setApi] = useState(options[0]?.value ?? "openai_chat");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [busy, setBusy] = useState(false);
  const trimmed = id.trim();
  const invalid = trimmed && !/^[A-Za-z0-9_-]+$/.test(trimmed) ? t("sp.id_invalid") : null;
  const duplicate = existing.includes(trimmed) ? t("sp.id_dup") : null;
  const ready = Boolean(trimmed && baseUrl.trim()) && !invalid && !duplicate;

  const create = async () => {
    if (!ready) return;
    setBusy(true);
    const ok = await writeSettings(
      (doc) =>
        setPath(doc, ["providers", trimmed], { api, base_url: baseUrl.trim(), api_key: apiKey }),
      t("sp.added", { id: trimmed }),
    );
    setBusy(false);
    if (ok) onCreated(trimmed);
  };

  return (
    <div className="space-y-5">
      <div>
        <h3 className="text-title font-semibold">{t("sp.add")}</h3>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">{t("sp.add_desc")}</p>
      </div>
      <div className="space-y-3.5 rounded-xl bg-card p-4 ring-1 ring-inset ring-border-subtle">
        <Labeled
          label={t("sp.id")}
          hint={invalid ?? duplicate ?? t("sp.id_hint")}
          error={Boolean(invalid ?? duplicate)}
        >
          <Input
            value={id}
            onChange={(event) => setId(event.target.value)}
            placeholder={t("sp.id_ph")}
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
        <Labeled label={t("sp.base_url")} hint={t("sp.base_url_desc")}>
          <Input
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            placeholder="https://api.example.com/v1"
            className="h-8 rounded-lg font-mono text-xs shadow-none"
            spellCheck={false}
          />
        </Labeled>
        <Labeled label={t("sp.api_key")} hint={t("sp.api_key_hint")}>
          <Input
            type="password"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
            autoComplete="off"
            className="h-8 rounded-lg text-ui shadow-none"
          />
        </Labeled>
      </div>
      <div className="flex justify-end gap-2">
        {onCancel && (
          <Button variant="ghost" size="sm" onClick={onCancel}>
            {t("common.cancel")}
          </Button>
        )}
        <Button size="sm" disabled={!ready || busy} onClick={() => void create()}>
          {busy && <Loader2Icon className="animate-spin" />}
          {t("sp.add_fetch")}
        </Button>
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
    <div className="grid grid-cols-1 items-start gap-3 md:grid-cols-[96px_minmax(0,1fr)]">
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

function ProviderDetail({
  id,
  doc,
  autoDiscover,
  onDeleted,
}: {
  id: string;
  doc: SettingsDocument;
  autoDiscover: boolean;
  onDeleted: () => void;
}) {
  const t = useT();
  const [showMore, setShowMore] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [discovered, setDiscovered] = useState<{
    busy: boolean;
    models: CatalogModel[] | null;
    error: string | null;
  }>({
    busy: false,
    models: null,
    error: null,
  });
  const explicit = isObject(getPath(doc.document, ["providers", id]));
  const provider = getPath(doc.effective, ["providers", id]);
  const models =
    isObject(provider) && isObject(provider.models) ? Object.keys(provider.models) : [];

  const connection = doc.fields.filter(
    (field) => field.path[0] === "providers" && field.path[1] === id && field.path[2] !== "models",
  );
  const primary = connection.filter((field) => PRIMARY[field.path[2]]);
  const parameters = connection.filter(
    (field) => field.path.length > 3 && ["extra_body", "headers"].includes(field.path[2]),
  );
  const secondary = connection.filter(
    (field) => !PRIMARY[field.path[2]] && !parameters.includes(field),
  );

  const runDiscover = async () => {
    setDiscovered((state) => ({ ...state, busy: true, error: null }));
    // 用最新的生效配置（刚保存的地址 / 密钥）去拉取。
    const latest = useSettings.getState().docs[useSettings.getState().scope];
    try {
      const list = await discover(id, getPath(latest?.effective, ["providers", id]) ?? provider);
      setDiscovered({ busy: false, models: list, error: null });
    } catch (error) {
      setDiscovered({ busy: false, models: null, error: errorText(error) });
    }
  };

  // biome-ignore lint/correctness/useExhaustiveDependencies: 只在新建后的首次挂载时自动拉取
  useEffect(() => {
    if (autoDiscover) void runDiscover();
  }, [autoDiscover]);

  const remove = () =>
    void writeSettings(
      (current) => setPath(current, ["providers", id], undefined),
      t("sp.deleted", { id }),
    ).then((ok) => ok && onDeleted());

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h3 className="truncate text-title font-semibold">{id}</h3>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            {explicit ? t("sp.explicit_note") : t("sp.inherited_note")}
          </p>
        </div>
        {explicit &&
          (confirmDelete ? (
            <div className="flex shrink-0 items-center gap-1.5">
              <Button size="xs" variant="ghost" onClick={() => setConfirmDelete(false)}>
                {t("common.cancel")}
              </Button>
              <Button size="xs" variant="destructive" onClick={remove}>
                {t("sp.confirm_delete")}
              </Button>
            </div>
          ) : (
            <IconButton label={t("sp.delete")} size="sm" onClick={() => setConfirmDelete(true)}>
              <Trash2Icon />
            </IconButton>
          ))}
      </div>

      <section className="space-y-2">
        <h4 className="text-xs font-medium text-text-tertiary">{t("sp.connection")}</h4>
        <FieldList fields={primary} doc={doc} strip={2} showGroups={false} />
        {(secondary.length > 0 || parameters.length > 0) && (
          <>
            <button
              type="button"
              onClick={() => setShowMore((value) => !value)}
              className="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground"
            >
              <ChevronRightIcon
                className={cn("size-3.5 transition-transform", showMore && "rotate-90")}
              />
              {t("sp.advanced", { n: secondary.length + parameters.length })}
            </button>
            {showMore && (
              <div className="space-y-4">
                {secondary.length > 0 && (
                  <FieldList fields={secondary} doc={doc} strip={2} includeAdvanced />
                )}
                {parameters.length > 0 && (
                  <div className="space-y-2">
                    <h4 className="text-xs font-medium text-text-tertiary">
                      {t("sp.params_title")}
                    </h4>
                    <p className="text-xs leading-5 text-muted-foreground">{t("sp.params_desc")}</p>
                    <FieldList fields={parameters} doc={doc} strip={2} includeAdvanced />
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </section>

      <section className="space-y-2">
        <div className="flex items-center justify-between gap-2">
          <h4 className="text-xs font-medium text-text-tertiary">
            {t("sp.models_list", { n: models.length })}
          </h4>
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

        {discovered.error && (
          <div className="rounded-lg border border-destructive/25 bg-destructive/5 px-3 py-2 text-xs text-destructive">
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
          {models.length === 0 && (
            <p className="px-4 py-6 text-center text-ui text-text-tertiary">{t("sp.no_models")}</p>
          )}
          {models.map((modelId) => (
            <ModelRow key={modelId} providerId={id} modelId={modelId} doc={doc} />
          ))}
          <ManualAdd providerId={id} existing={models} />
        </div>
      </section>
    </div>
  );
}

function ModelRow({
  providerId,
  modelId,
  doc,
}: {
  providerId: string;
  modelId: string;
  doc: SettingsDocument;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const path = ["providers", providerId, "models", modelId];
  const model = getPath(doc.effective, path);
  const info = isObject(model) ? model : {};
  const explicit = getPath(doc.document, path) !== undefined;
  const fields = doc.fields.filter(
    (field) =>
      field.path.length > 4 &&
      field.path[0] === "providers" &&
      field.path[1] === providerId &&
      field.path[2] === "models" &&
      field.path[3] === modelId,
  );
  const imageGen = info.image_gen === true;
  const context =
    !imageGen && typeof info.context_window === "number" ? info.context_window : undefined;
  const image = !imageGen && Array.isArray(info.input) && info.input.includes("image");

  return (
    <div>
      <div className="group/model flex h-11 items-center gap-2 pr-2 pl-3 hover:bg-surface-hover">
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
          aria-expanded={open}
        >
          <ChevronRightIcon
            className={cn(
              "size-3.5 shrink-0 text-text-tertiary transition-transform",
              open && "rotate-90",
            )}
          />
          <span className="min-w-0 truncate font-mono text-xs text-foreground">{modelId}</span>
          <span className="ml-auto flex shrink-0 items-center gap-2 text-text-tertiary">
            {imageGen && <Badge variant="brand">Image</Badge>}
            {image && <ImageIcon className="size-3.5" aria-label={t("mp.images")} />}
            {context && (
              <span className="font-mono text-xs tabular-nums">{compactTokens(context)}</span>
            )}
          </span>
        </button>
        <div className="flex shrink-0 items-center opacity-0 transition-opacity group-hover/model:opacity-100 focus-within:opacity-100">
          {explicit && (
            <IconButton
              label={t("sp.remove_model")}
              size="xs"
              onClick={() => void writeSettings((current) => setPath(current, path, undefined))}
            >
              <Trash2Icon />
            </IconButton>
          )}
        </div>
      </div>
      {open && (
        <div className="border-t border-border-subtle bg-background/60 px-4 py-3">
          <FieldList fields={fields} doc={doc} strip={4} />
        </div>
      )}
    </div>
  );
}

function ManualAdd({ providerId, existing }: { providerId: string; existing: string[] }) {
  const t = useT();
  const [value, setValue] = useState("");
  const id = value.trim();
  const add = () => {
    if (!id || existing.includes(id)) return;
    void writeSettings((doc) => setPath(doc, ["providers", providerId, "models", id], {})).then(
      (ok) => ok && setValue(""),
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
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const visible = candidates.filter((model) =>
    `${model.id} ${model.name}`.toLowerCase().includes(query.trim().toLowerCase()),
  );
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
    const ids = [...picked];
    if (ids.length === 0) return;
    setBusy(true);
    const ok = await writeSettings(
      (doc) => {
        let next = doc;
        for (const id of ids) next = setPath(next, ["providers", providerId, "models", id], {});
        return next;
      },
      t("sp.models_added", { n: ids.length }),
    );
    setBusy(false);
    if (ok) onClose();
  };

  if (candidates.length === 0) {
    return (
      <div className="flex items-center justify-between rounded-lg bg-muted/60 px-3 py-2 text-xs text-muted-foreground">
        {t("sp.no_candidates")}
        <Button size="xs" variant="ghost" onClick={onClose}>
          {t("common.close")}
        </Button>
      </div>
    );
  }

  return (
    <div className="overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-brand/30">
      <div className="flex items-center gap-2 border-b border-border-subtle px-3 py-2">
        <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder={t("sp.search_ph", { n: candidates.length })}
          className="h-7 min-w-0 flex-1 bg-transparent text-ui outline-none placeholder:text-text-tertiary"
        />
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
        {visible.map((model) => {
          const added = existing.has(model.id);
          return (
            <li key={model.id}>
              {/* biome-ignore lint/a11y/noLabelWithoutControl: Radix Checkbox 渲染为 button，被 label 包裹即可点整行切换 */}
              <label
                className={cn(
                  "flex h-8 items-center gap-2.5 rounded-md px-2 text-ui",
                  added ? "opacity-50" : "cursor-pointer hover:bg-surface-hover",
                )}
              >
                {added ? (
                  <CheckIcon className="size-4 text-text-tertiary" />
                ) : (
                  <Checkbox
                    checked={picked.has(model.id)}
                    onCheckedChange={() => toggle(model.id)}
                  />
                )}
                <span className="min-w-0 truncate font-mono text-xs">{model.id}</span>
                {model.name && model.name !== model.id && (
                  <span className="min-w-0 truncate text-xs text-text-tertiary">{model.name}</span>
                )}
                {added && (
                  <span className="ml-auto shrink-0 text-2xs text-text-tertiary">
                    {t("sp.added_mark")}
                  </span>
                )}
              </label>
            </li>
          );
        })}
      </ul>
      <div className="flex items-center justify-end gap-2 border-t border-border-subtle px-3 py-2">
        <Button size="xs" variant="ghost" onClick={onClose}>
          {t("th.collapse")}
        </Button>
        <Button size="xs" disabled={picked.size === 0 || busy} onClick={() => void add()}>
          {busy && <Loader2Icon className="animate-spin" />}
          {picked.size > 0 ? t("sp.add_n", { n: picked.size }) : t("sp.add_models")}
        </Button>
      </div>
    </div>
  );
}
