// 模型详情编辑子视图（General / Compat / Params）与通配模型默认规则视图（[defaults.models]）。

import {
  ArrowLeftIcon,
  CopyIcon,
  ImageIcon,
  PencilIcon,
  PlusIcon,
  SparklesIcon,
  Trash2Icon,
} from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { useT } from "@/lib/i18n";
import { compactTokens } from "@/lib/models";
import { nextCopyId } from "@/lib/provider-presets";
import { getPath, setPath } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import type { ConfigField, SettingsDocument } from "@/protocol/types";
import { FieldList, FieldRow } from "./field";
import { KeyValueEditor, RequestParamsEditor } from "./provider-widgets";
import { useSettings, writeField, writeSettings, writeSettingsWithUndo } from "./settings-store";

type Json = Record<string, unknown>;
type ModelTab = "general" | "compat" | "params";

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** 独立全宽的模型编辑器：用于供应商下的具体模型，也复用于 `[defaults.models."pattern"]`。 */
export function ModelEditor({
  doc,
  basePath,
  title,
  subtitle,
  inherited,
  siblingIds,
  onBack,
  onCloned,
  onRename,
  onDeleted,
}: {
  doc: SettingsDocument;
  /** `["providers", providerId, "models", modelId]` 或 `["defaults", "models", pattern]` */
  basePath: string[];
  title: string;
  subtitle?: string;
  inherited: boolean;
  siblingIds: string[];
  onBack: () => void;
  onCloned?: (newId: string) => void;
  onRename?: (nextId: string) => Promise<boolean>;
  onDeleted: () => void;
}) {
  const t = useT();
  const [tab, setTab] = useState<ModelTab>("general");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [cloning, setCloning] = useState(false);
  const [cloneName, setCloneName] = useState("");
  const [renaming, setRenaming] = useState(false);
  const [renameDraft, setRenameDraft] = useState("");
  const [renameBusy, setRenameBusy] = useState(false);

  const model = getPath(doc.effective, basePath);
  const info = isObject(model) ? model : {};
  const explicitObj = getPath(doc.document, basePath);
  const explicit = explicitObj !== undefined;
  const enabled = info.enabled !== false;
  const imageGen = info.image_gen === true;
  const isProviderModel = basePath[0] === "providers";
  const prefixLen = basePath.length;

  const allFields = useMemo(
    () =>
      doc.fields.filter(
        (field) =>
          field.path.length > prefixLen && basePath.every((seg, idx) => field.path[idx] === seg),
      ),
    [doc.fields, basePath, prefixLen],
  );

  const { generalFields, compatFields, paramFields, rawParamFields, headersField } = useMemo(() => {
    const general: ConfigField[] = [];
    const compat: ConfigField[] = [];
    const params: ConfigField[] = [];
    const rawParams: ConfigField[] = [];
    let headers: ConfigField | undefined;
    for (const field of allFields) {
      const sub = field.path.slice(prefixLen);
      // `enabled` 已放在模型顶栏开关中，不再在表单里重复一行。
      if (sub.length === 1 && sub[0] === "enabled") continue;
      if (sub[0] === "compat") {
        if (sub.length === 2 && sub[1] === "headers") {
          headers = field;
          rawParams.push(field);
        } else if (sub.length === 2 && sub[1] === "extra_body") {
          rawParams.push(field);
        } else if (sub.length > 2 && (sub[1] === "extra_body" || sub[1] === "headers")) {
          params.push(field);
        } else {
          compat.push(field);
        }
      } else if (field.advanced) {
        compat.push(field);
      } else {
        general.push(field);
      }
    }
    return {
      generalFields: general,
      compatFields: compat,
      paramFields: params,
      rawParamFields: rawParams,
      headersField: headers,
    };
  }, [allFields, prefixLen]);

  const configuredParamCount = useMemo(
    () =>
      paramFields.filter(
        (field) =>
          getPath(doc.document, field.path) !== undefined ||
          getPath(doc.inherited, field.path) !== undefined,
      ).length,
    [paramFields, doc],
  );

  const handleDelete = async () => {
    const previous = explicitObj;
    const ok = await writeSettingsWithUndo(
      (current) => setPath(current, basePath, undefined),
      (current) => setPath(current, basePath, previous),
      t("sp.model_deleted", { id: title }),
      t("sp.model_restored", { id: title }),
    );
    if (ok) onDeleted();
  };

  const renameTrimmed = renameDraft.trim();
  const renameDuplicate = renameTrimmed !== title && siblingIds.includes(renameTrimmed);

  const handleRename = async () => {
    if (!onRename || renameBusy || !renameTrimmed || renameDuplicate) return;
    if (renameTrimmed === title) {
      setRenaming(false);
      return;
    }
    setRenameBusy(true);
    const ok = await onRename(renameTrimmed);
    setRenameBusy(false);
    if (ok) {
      setRenaming(false);
      setRenameDraft("");
    }
  };

  const handleClone = async () => {
    const trimmed = cloneName.trim();
    if (!trimmed || siblingIds.includes(trimmed)) return;
    const source = isObject(explicitObj)
      ? structuredClone(explicitObj)
      : isObject(model)
        ? structuredClone(model)
        : {};
    const targetPath = [...basePath.slice(0, -1), trimmed];
    const ok = await writeSettings(
      (current) => setPath(current, targetPath, source),
      t("sp.model_cloned", { id: trimmed }),
    );
    if (ok) {
      setCloning(false);
      setCloneName("");
      onCloned?.(trimmed);
    }
  };

  const tabs: { id: ModelTab; label: string }[] = [
    { id: "general", label: t("sp.tab_general") },
    ...(!imageGen && compatFields.length > 0
      ? [{ id: "compat" as const, label: `${t("sp.tab_compat")} (${compatFields.length})` }]
      : []),
    ...(paramFields.length > 0 || rawParamFields.length > 0
      ? [
          {
            id: "params" as const,
            label:
              configuredParamCount > 0
                ? `${t("sp.tab_params")} (${configuredParamCount})`
                : t("sp.tab_params"),
          },
        ]
      : []),
  ];

  const activeTab: ModelTab = tabs.some((item) => item.id === tab) ? tab : "general";

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <Button type="button" size="xs" variant="ghost" onClick={onBack} className="-ml-1.5">
            <ArrowLeftIcon className="size-3.5" />
            {t("common.back")}
          </Button>
          <span className="text-border-strong">/</span>
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h3 className="truncate font-mono text-sm font-semibold text-foreground">{title}</h3>
              {inherited && <Badge variant="secondary">Global</Badge>}
              {!enabled && <Badge variant="secondary">{t("sp.disabled")}</Badge>}
              {imageGen && <Badge variant="brand">Image</Badge>}
            </div>
            {subtitle && <p className="truncate text-xs text-text-tertiary">{subtitle}</p>}
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-2">
          {isProviderModel && (
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <span>{t("sp.enabled")}</span>
              <Switch
                checked={enabled}
                onCheckedChange={(checked) =>
                  void writeField([...basePath, "enabled"], checked ? undefined : false)
                }
                aria-label={t("sp.enabled")}
              />
            </div>
          )}
          {onRename && (
            <IconButton
              label={t("sp.defaults_rename")}
              size="sm"
              active={renaming}
              onClick={() => {
                setCloning(false);
                setConfirmDelete(false);
                setRenameDraft(title);
                setRenaming((prev) => !prev);
              }}
            >
              <PencilIcon />
            </IconButton>
          )}
          <IconButton
            label={t("sp.clone_model")}
            size="sm"
            active={cloning}
            onClick={() => {
              setRenaming(false);
              setConfirmDelete(false);
              setCloneName(nextCopyId(title, siblingIds));
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
                <Button size="xs" variant="destructive" onClick={() => void handleDelete()}>
                  {t("sp.confirm_delete")}
                </Button>
              </div>
            ) : (
              <IconButton
                label={t("sp.remove_model")}
                size="sm"
                onClick={() => {
                  setRenaming(false);
                  setCloning(false);
                  setConfirmDelete(true);
                }}
              >
                <Trash2Icon />
              </IconButton>
            ))}
        </div>
      </div>

      {renaming && onRename && (
        <div className="flex flex-wrap items-center gap-2 rounded-xl bg-card p-3 ring-1 ring-inset ring-border-subtle">
          <span className="text-xs text-muted-foreground">{t("sp.defaults_rename_as")}</span>
          <Input
            value={renameDraft}
            onChange={(event) => setRenameDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.nativeEvent.isComposing) return;
              if (event.key === "Enter") void handleRename();
              if (event.key === "Escape") setRenaming(false);
            }}
            aria-label={t("sp.defaults_rename")}
            spellCheck={false}
            className="h-7 min-w-44 flex-1 rounded-lg font-mono text-xs shadow-none"
            autoFocus
          />
          <Button size="xs" variant="ghost" onClick={() => setRenaming(false)}>
            {t("common.cancel")}
          </Button>
          <Button
            size="xs"
            disabled={renameBusy || !renameTrimmed || renameDuplicate}
            onClick={() => void handleRename()}
          >
            {renameDuplicate ? t("sp.exists") : t("common.save")}
          </Button>
        </div>
      )}

      {cloning && (
        <div className="flex flex-wrap items-center gap-2 rounded-xl bg-card p-3 ring-1 ring-inset ring-border-subtle">
          <span className="text-xs text-muted-foreground">{t("sp.clone_as")}</span>
          <Input
            value={cloneName}
            onChange={(event) => setCloneName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void handleClone();
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
            disabled={!cloneName.trim() || siblingIds.includes(cloneName.trim())}
            onClick={() => void handleClone()}
          >
            {t("sp.clone")}
          </Button>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-1 border-b border-border-subtle pb-2">
        {tabs.map((item) => (
          <button
            key={item.id}
            type="button"
            onClick={() => setTab(item.id)}
            className={cn(
              "h-7 rounded-md px-2.5 text-xs transition-colors",
              activeTab === item.id
                ? "bg-accent font-medium text-foreground"
                : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
            )}
          >
            {item.label}
          </button>
        ))}
      </div>

      {activeTab === "general" && (
        <FieldList fields={generalFields} doc={doc} strip={prefixLen} showGroups={false} />
      )}

      {activeTab === "compat" && (
        <FieldList fields={compatFields} doc={doc} strip={prefixLen} includeAdvanced />
      )}

      {activeTab === "params" && (
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

// ---------- `[defaults.models."<pattern>"]` 通配模型默认规则管理 ----------

export function ModelDefaultsView({ doc, onBack }: { doc: SettingsDocument; onBack?: () => void }) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  const [editing, setEditing] = useState<string | null>(null);
  const [pattern, setPattern] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [renameBusy, setRenameBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const pendingRenameRef = useRef<{ from: string; to: string } | null>(null);

  const patterns = useMemo(() => {
    const map = getPath(doc.effective, ["defaults", "models"]);
    return isObject(map) ? Object.keys(map).toSorted() : [];
  }, [doc.effective]);

  const canRenamePattern = (pat: string) =>
    getPath(doc.document, ["defaults", "models", pat]) !== undefined &&
    !(scope === "project" && getPath(doc.inherited, ["defaults", "models", pat]) !== undefined);

  const renameRule = async (oldPat: string, nextPat: string): Promise<boolean> => {
    const trimmed = nextPat.trim();
    if (!trimmed) return false;
    if (trimmed === oldPat) return true;
    if (patterns.includes(trimmed)) return false;
    pendingRenameRef.current = { from: oldPat, to: trimmed };
    const ok = await writeSettings(
      (current) => {
        const prev = getPath(current, ["defaults", "models", oldPat]);
        if (prev === undefined) return current;
        const withoutOld = setPath(current, ["defaults", "models", oldPat], undefined);
        return setPath(withoutOld, ["defaults", "models", trimmed], structuredClone(prev));
      },
      t("sp.defaults_renamed", { from: oldPat, to: trimmed }),
    );
    pendingRenameRef.current = null;
    if (ok) {
      setEditing((current) => (current === oldPat ? trimmed : current));
    }
    return ok;
  };

  const resolvedEditing =
    editing && patterns.includes(editing)
      ? editing
      : editing &&
          pendingRenameRef.current?.from === editing &&
          patterns.includes(pendingRenameRef.current.to)
        ? pendingRenameRef.current.to
        : null;

  if (resolvedEditing) {
    const inherited = getPath(doc.document, ["defaults", "models", resolvedEditing]) === undefined;
    const canRename = canRenamePattern(resolvedEditing);
    return (
      <ModelEditor
        doc={doc}
        basePath={["defaults", "models", resolvedEditing]}
        title={resolvedEditing}
        subtitle={t("sp.defaults_rule_sub")}
        inherited={inherited}
        siblingIds={patterns}
        onBack={() => setEditing(null)}
        onCloned={(newId) => setEditing(newId)}
        onRename={canRename ? (next) => renameRule(resolvedEditing, next) : undefined}
        onDeleted={() => setEditing(null)}
      />
    );
  }

  const addRule = async () => {
    const trimmed = pattern.trim();
    if (!trimmed || patterns.includes(trimmed)) return;
    const ok = await writeSettings(
      (current) =>
        setPath(current, ["defaults", "models", trimmed], {
          context_window: 200_000,
          max_output_tokens: 32_000,
          reasoning: false,
        }),
      t("sp.defaults_added", { pattern: trimmed }),
    );
    if (ok) {
      setPattern("");
      setEditing(trimmed);
    }
  };

  const commitRowRename = async (pat: string) => {
    const trimmed = renameDraft.trim();
    if (renameBusy || !trimmed || (trimmed !== pat && patterns.includes(trimmed))) return;
    if (trimmed === pat) {
      setRenaming(null);
      return;
    }
    setRenameBusy(true);
    const ok = await renameRule(pat, trimmed);
    setRenameBusy(false);
    if (ok) {
      setRenaming(null);
      setRenameDraft("");
    }
  };

  const removeRule = async (pat: string) => {
    const previous = getPath(doc.document, ["defaults", "models", pat]);
    await writeSettingsWithUndo(
      (current) => setPath(current, ["defaults", "models", pat], undefined),
      (current) => setPath(current, ["defaults", "models", pat], previous),
      t("sp.defaults_deleted", { pattern: pat }),
      t("sp.defaults_restored", { pattern: pat }),
    );
    setConfirmDelete(null);
  };

  return (
    <div className="space-y-5">
      <div className="flex items-start justify-between gap-3">
        <div>
          <div className="flex items-center gap-2">
            {onBack && (
              <Button type="button" size="xs" variant="ghost" onClick={onBack} className="-ml-1.5">
                <ArrowLeftIcon className="size-3.5" />
                {t("common.back")}
              </Button>
            )}
            <h3 className="text-title font-semibold">{t("sp.defaults_title")}</h3>
          </div>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">{t("sp.defaults_desc")}</p>
        </div>
      </div>

      <div className="divide-y divide-border-subtle overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle">
        {patterns.length === 0 && (
          <p className="px-4 py-6 text-center text-ui text-text-tertiary">
            {t("sp.defaults_empty")}
          </p>
        )}
        {patterns.map((pat) => {
          const val = getPath(doc.effective, ["defaults", "models", pat]);
          const info = isObject(val) ? val : {};
          const explicit = getPath(doc.document, ["defaults", "models", pat]) !== undefined;
          const canRename = canRenamePattern(pat);
          if (renaming === pat && canRename) {
            const trimmed = renameDraft.trim();
            const duplicate = trimmed !== pat && patterns.includes(trimmed);
            return (
              <div key={pat} className="flex items-center gap-2 px-3 py-2">
                <Input
                  value={renameDraft}
                  onChange={(event) => setRenameDraft(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.nativeEvent.isComposing) return;
                    if (event.key === "Enter") void commitRowRename(pat);
                    if (event.key === "Escape") setRenaming(null);
                  }}
                  aria-label={t("sp.defaults_rename")}
                  spellCheck={false}
                  className="h-7 min-w-0 flex-1 rounded-lg font-mono text-xs shadow-none"
                  autoFocus
                />
                <Button size="xs" variant="ghost" onClick={() => setRenaming(null)}>
                  {t("common.cancel")}
                </Button>
                <Button
                  size="xs"
                  disabled={renameBusy || !trimmed || duplicate}
                  onClick={() => void commitRowRename(pat)}
                >
                  {duplicate ? t("sp.exists") : t("common.save")}
                </Button>
              </div>
            );
          }
          const ctx = typeof info.context_window === "number" ? info.context_window : undefined;
          const out =
            typeof info.max_output_tokens === "number" ? info.max_output_tokens : undefined;
          const reasoning = info.reasoning === true;
          const vision = Array.isArray(info.input) && info.input.includes("image");
          return (
            <div
              key={pat}
              className="flex items-center gap-2 px-3 py-2.5 transition-colors hover:bg-surface-hover"
            >
              <button
                type="button"
                onClick={() => setEditing(pat)}
                className="flex min-w-0 flex-1 items-center gap-2 text-left"
              >
                <span className="truncate font-mono text-xs font-medium text-foreground">
                  {pat}
                </span>
                {!explicit && <Badge variant="secondary">Global</Badge>}
                <span className="ml-auto flex shrink-0 items-center gap-2 text-xs text-text-tertiary">
                  {reasoning && <SparklesIcon className="size-3.5" />}
                  {vision && <ImageIcon className="size-3.5" />}
                  {ctx && (
                    <span className="font-mono tabular-nums">
                      {compactTokens(ctx)}
                      {out ? ` / ${compactTokens(out)}` : ""}
                    </span>
                  )}
                </span>
              </button>
              {explicit && confirmDelete === pat ? (
                <div className="flex shrink-0 items-center gap-1">
                  <Button size="xs" variant="ghost" onClick={() => setConfirmDelete(null)}>
                    {t("common.cancel")}
                  </Button>
                  <Button size="xs" variant="destructive" onClick={() => void removeRule(pat)}>
                    {t("sp.confirm_delete")}
                  </Button>
                </div>
              ) : (
                (canRename || explicit) && (
                  <div className="flex shrink-0 items-center gap-1">
                    {canRename && (
                      <IconButton
                        label={t("sp.defaults_rename")}
                        size="xs"
                        onClick={() => {
                          setConfirmDelete(null);
                          setRenaming(pat);
                          setRenameDraft(pat);
                        }}
                      >
                        <PencilIcon />
                      </IconButton>
                    )}
                    {explicit && (
                      <IconButton
                        label={t("sp.remove_model")}
                        size="xs"
                        onClick={() => {
                          setRenaming(null);
                          setConfirmDelete(pat);
                        }}
                      >
                        <Trash2Icon />
                      </IconButton>
                    )}
                  </div>
                )
              )}
            </div>
          );
        })}
        <div className="flex items-center gap-2 px-3 py-2">
          <PlusIcon className="size-3.5 shrink-0 text-text-tertiary" />
          <input
            value={pattern}
            onChange={(event) => setPattern(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void addRule();
            }}
            placeholder={t("sp.defaults_ph")}
            spellCheck={false}
            className="h-7 min-w-0 flex-1 bg-transparent font-mono text-xs outline-none placeholder:font-sans placeholder:text-text-tertiary"
          />
          {pattern.trim() && (
            <Button
              size="xs"
              variant="ghost"
              disabled={patterns.includes(pattern.trim())}
              onClick={() => void addRule()}
            >
              {patterns.includes(pattern.trim()) ? t("sp.exists") : t("common.add")}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}
