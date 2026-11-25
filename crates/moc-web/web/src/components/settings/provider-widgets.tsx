// 供应商与模型专用结构化控件：API Key 显隐与引用识别、Token 快捷输入、
// 定价 4 格数字表单、思考等级芯片、端点选择器、键值表、按需添加 API 参数面板。

import {
  ArrowLeftIcon,
  ArrowRightIcon,
  Code2Icon,
  EyeIcon,
  EyeOffIcon,
  PlusIcon,
  SearchIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useT } from "@/lib/i18n";
import { compactTokens } from "@/lib/models";
import {
  BUILTIN_THINKING_LEVELS,
  EFFORT_MAP_KEYS,
  isValidCustomThinkingLevel,
  secretRefKind,
} from "@/lib/provider-presets";
import { formatField, getPath, parseField } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import type { ConfigField, SettingsDocument } from "@/protocol/types";
import { writeField } from "./settings-store";

type Json = Record<string, unknown>;

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** 比较平面键值表（如定价、headers）是否语义一致；空对象与未设置视为等价。 */
function sameFlatRecord(a: unknown, b: Record<string, string | number> | undefined): boolean {
  if (b === undefined) {
    return a === undefined || (isObject(a) && Object.keys(a).length === 0);
  }
  if (!isObject(a)) return false;
  const aKeys = Object.keys(a);
  const bKeys = Object.keys(b);
  if (aKeys.length !== bKeys.length) return false;
  return bKeys.every((key) => a[key] === b[key]);
}

// ---------- API Key 输入框（带显隐按钮与 env:/! 引用明文识别） ----------

export function ApiKeyInput({
  value,
  onChange,
  onFocus,
  onBlur,
  onKeyDown,
  placeholder,
  ariaLabel,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  onFocus?: () => void;
  onBlur?: () => void;
  onKeyDown?: React.KeyboardEventHandler<HTMLInputElement>;
  placeholder?: string;
  ariaLabel?: string;
  className?: string;
}) {
  const t = useT();
  const [revealed, setRevealed] = useState(false);
  const refKind = secretRefKind(value);
  const showPlain = revealed || refKind !== null;

  return (
    <div className="relative flex items-center">
      <Input
        type={showPlain ? "text" : "password"}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onFocus={onFocus}
        onBlur={onBlur}
        onKeyDown={onKeyDown}
        placeholder={placeholder}
        aria-label={ariaLabel}
        autoComplete="off"
        spellCheck={false}
        className={cn(
          "h-8 rounded-lg text-ui shadow-none",
          refKind ? "pr-14 font-mono text-xs" : "pr-8",
          className,
        )}
      />
      <div className="absolute right-1.5 flex items-center gap-1">
        {refKind && (
          <Badge variant="brand" className="pointer-events-none px-1.5 py-0 font-mono uppercase">
            {refKind}
          </Badge>
        )}
        {!refKind && (
          <button
            type="button"
            tabIndex={-1}
            onMouseDown={(event) => event.preventDefault()}
            onClick={() => setRevealed((prev) => !prev)}
            className="flex size-6 items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-surface-hover hover:text-foreground"
            title={revealed ? t("sp.hide_key") : t("sp.show_key")}
            aria-label={revealed ? t("sp.hide_key") : t("sp.show_key")}
          >
            {revealed ? <EyeOffIcon className="size-3.5" /> : <EyeIcon className="size-3.5" />}
          </button>
        )}
      </div>
    </div>
  );
}

// ---------- Token 数量输入（支持 k/m 缩写、紧凑回显与常用快捷芯片） ----------

const CONTEXT_PRESETS = [
  { label: "128K", value: 128_000 },
  { label: "200K", value: 200_000 },
  { label: "400K", value: 400_000 },
  { label: "1M", value: 1_000_000 },
];

const OUTPUT_PRESETS = [
  { label: "8K", value: 8_192 },
  { label: "16K", value: 16_384 },
  { label: "32K", value: 32_000 },
  { label: "64K", value: 64_000 },
  { label: "128K", value: 128_000 },
];

export function TokenCountEditor({
  field,
  explicit,
  fallback,
  onError,
}: {
  field: ConfigField;
  explicit: unknown;
  fallback: unknown;
  onError: (error: string | null) => void;
}) {
  const t = useT();
  const explicitText = formatField(explicit, field.kind);
  const [draft, setDraft] = useState(explicitText);
  const [focused, setFocused] = useState(false);

  useEffect(() => {
    if (!focused) setDraft(explicitText);
  }, [explicitText, focused]);

  const key = field.path.at(-1);
  const presets =
    key === "context_window" ? CONTEXT_PRESETS : key === "max_output_tokens" ? OUTPUT_PRESETS : [];
  const effectiveNum =
    typeof explicit === "number" ? explicit : typeof fallback === "number" ? fallback : undefined;
  const parsedPreview = useMemo(() => {
    if (!draft.trim()) return effectiveNum;
    const res = parseField(draft, field.kind);
    return res.ok && typeof res.value === "number" ? res.value : undefined;
  }, [draft, effectiveNum, field.kind]);

  const commit = (text = draft) => {
    if (text === explicitText) return;
    const parsed = parseField(text, field.kind);
    if (!parsed.ok) {
      onError(parsed.error);
      return;
    }
    onError(null);
    void writeField(field.path, parsed.value);
  };

  const fallbackText = typeof fallback === "number" ? String(fallback) : "";
  const placeholder = fallbackText ? t("sf.default_of", { value: fallbackText }) : t("sf.unset");

  return (
    <div className="space-y-1.5">
      <div className="relative flex items-center">
        <Input
          value={draft}
          placeholder={placeholder}
          aria-label={field.label}
          spellCheck={false}
          onFocus={() => setFocused(true)}
          onBlur={() => {
            setFocused(false);
            commit();
          }}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") event.currentTarget.blur();
            if (event.key === "Escape") {
              setDraft(explicitText);
              onError(null);
            }
          }}
          className="h-8 rounded-lg pr-14 font-mono text-ui tabular-nums shadow-none"
        />
        {parsedPreview !== undefined && parsedPreview > 0 && (
          <span className="pointer-events-none absolute right-2.5 font-mono text-2xs text-text-tertiary tabular-nums">
            {compactTokens(parsedPreview)}
          </span>
        )}
      </div>
      {presets.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {presets.map((preset) => {
            const active = effectiveNum === preset.value;
            return (
              <button
                key={preset.label}
                type="button"
                onClick={() => {
                  setDraft(String(preset.value));
                  onError(null);
                  if (explicit !== preset.value) void writeField(field.path, preset.value);
                }}
                className={cn(
                  "rounded-md px-1.5 py-0.5 font-mono text-2xs transition-colors",
                  active
                    ? "bg-accent font-medium text-foreground"
                    : "text-text-tertiary hover:bg-surface-hover hover:text-foreground",
                )}
              >
                {preset.label}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

// ---------- Pricing 结构化 4 格输入 ----------

type PricingDraft = {
  input: string;
  output: string;
  cache_read: string;
  cache_write: string;
};

function pricingToDraft(value: unknown): PricingDraft {
  if (!isObject(value)) return { input: "", output: "", cache_read: "", cache_write: "" };
  const fmt = (k: string) => (typeof value[k] === "number" ? String(value[k]) : "");
  return {
    input: fmt("input"),
    output: fmt("output"),
    cache_read: fmt("cache_read"),
    cache_write: fmt("cache_write"),
  };
}

export function PricingEditor({
  path,
  explicit,
  fallback,
  onError,
}: {
  path: string[];
  explicit: unknown;
  fallback: unknown;
  onError: (error: string | null) => void;
}) {
  const t = useT();
  const baseDraft = useMemo(() => pricingToDraft(explicit), [explicit]);
  const fallbackDraft = useMemo(() => pricingToDraft(fallback), [fallback]);
  const [draft, setDraft] = useState<PricingDraft>(baseDraft);
  const [editing, setEditing] = useState(false);

  useEffect(() => {
    if (!editing) setDraft(baseDraft);
  }, [baseDraft, editing]);

  const commit = (next: PricingDraft) => {
    const entries = Object.entries(next).filter(([, v]) => v.trim() !== "");
    if (entries.length === 0) {
      onError(null);
      if (explicit !== undefined) void writeField(path, undefined);
      return;
    }
    const parsed: Record<string, number> = {};
    for (const [k, raw] of entries) {
      const num = Number(raw.trim());
      if (!Number.isFinite(num) || num < 0) {
        onError(t("set.err_nonneg"));
        return;
      }
      parsed[k] = num;
    }
    if (parsed.input === undefined || parsed.output === undefined) {
      onError(t("sp.pricing_required"));
      return;
    }
    onError(null);
    if (!sameFlatRecord(explicit, parsed)) void writeField(path, parsed);
  };

  const fields: { key: keyof PricingDraft; label: string }[] = [
    { key: "input", label: t("sp.price_input") },
    { key: "output", label: t("sp.price_output") },
    { key: "cache_read", label: t("sp.price_cache_read") },
    { key: "cache_write", label: t("sp.price_cache_write") },
  ];

  return (
    <div className="grid grid-cols-2 gap-2.5 sm:grid-cols-4">
      {fields.map((item) => (
        <div key={item.key} className="space-y-1">
          <span className="block text-2xs text-text-tertiary">{item.label}</span>
          <div className="relative flex items-center">
            <Input
              aria-label={`pricing ${item.key}`}
              value={draft[item.key]}
              placeholder={fallbackDraft[item.key] || "—"}
              inputMode="decimal"
              spellCheck={false}
              onFocus={() => setEditing(true)}
              onBlur={() => {
                setEditing(false);
                commit(draft);
              }}
              onChange={(event) =>
                setDraft((prev) => ({ ...prev, [item.key]: event.target.value }))
              }
              onKeyDown={(event) => {
                if (event.key === "Enter") event.currentTarget.blur();
                if (event.key === "Escape") {
                  setDraft(baseDraft);
                  onError(null);
                }
              }}
              className="h-8 rounded-lg pr-11 font-mono text-xs tabular-nums shadow-none"
            />
            <span className="pointer-events-none absolute right-2 text-2xs text-text-tertiary">
              $/M
            </span>
          </div>
        </div>
      ))}
    </div>
  );
}

// ---------- Thinking Levels 可勾选芯片 + 排序 + 自定义等级 ----------

export function ThinkingLevelsEditor({
  path,
  explicit,
  fallback,
  onError,
}: {
  path: string[];
  explicit: unknown;
  fallback: unknown;
  onError: (error: string | null) => void;
}) {
  const t = useT();
  const [custom, setCustom] = useState("");
  const current: string[] = useMemo(() => {
    const raw = explicit ?? fallback;
    if (Array.isArray(raw)) return raw.filter((v): v is string => typeof v === "string");
    return [...BUILTIN_THINKING_LEVELS];
  }, [explicit, fallback]);
  const isDefault = explicit === undefined;

  const save = (next: string[]) => {
    onError(null);
    const sameAsBuiltin =
      next.length === BUILTIN_THINKING_LEVELS.length &&
      next.every((v, idx) => v === BUILTIN_THINKING_LEVELS[idx]);
    if (next.length === 0 || sameAsBuiltin) {
      void writeField(path, undefined);
    } else {
      void writeField(path, next);
    }
  };

  const toggle = (level: string) => {
    if (current.includes(level)) {
      const next = current.filter((item) => item !== level);
      if (next.length === 0) {
        void writeField(path, undefined);
      } else {
        save(next);
      }
    } else {
      save([...current, level]);
    }
  };

  const move = (index: number, delta: number) => {
    const target = index + delta;
    if (target < 0 || target >= current.length) return;
    const next = [...current];
    const [item] = next.splice(index, 1);
    next.splice(target, 0, item);
    save(next);
  };

  const addCustom = () => {
    const trimmed = custom.trim();
    if (!trimmed) return;
    if (!isValidCustomThinkingLevel(trimmed)) {
      onError(t("sp.thinking_level_invalid"));
      return;
    }
    if (current.includes(trimmed)) {
      setCustom("");
      return;
    }
    onError(null);
    setCustom("");
    save([...current, trimmed]);
  };

  const inactiveBuiltins = BUILTIN_THINKING_LEVELS.filter((lvl) => !current.includes(lvl));

  return (
    <div className="space-y-2.5">
      <div className="flex flex-wrap items-center gap-1.5">
        {current.map((level, index) => (
          <span
            key={level}
            className="inline-flex h-7 items-center gap-1 rounded-lg bg-accent px-2 font-mono text-xs text-foreground"
          >
            <span>{level}</span>
            {index === 0 && (
              <span className="text-2xs font-sans text-brand">{t("sp.level_default")}</span>
            )}
            {current.length > 1 && (
              <span className="ml-0.5 inline-flex items-center">
                <button
                  type="button"
                  disabled={index === 0}
                  onClick={() => move(index, -1)}
                  className="rounded p-0.5 text-text-tertiary hover:text-foreground disabled:opacity-25"
                  aria-label={t("sp.move_left")}
                >
                  <ArrowLeftIcon className="size-3" />
                </button>
                <button
                  type="button"
                  disabled={index === current.length - 1}
                  onClick={() => move(index, 1)}
                  className="rounded p-0.5 text-text-tertiary hover:text-foreground disabled:opacity-25"
                  aria-label={t("sp.move_right")}
                >
                  <ArrowRightIcon className="size-3" />
                </button>
              </span>
            )}
            <button
              type="button"
              onClick={() => toggle(level)}
              className="rounded p-0.5 text-text-tertiary hover:text-foreground"
              aria-label={t("sp.remove_model")}
            >
              <XIcon className="size-3" />
            </button>
          </span>
        ))}
        {isDefault && (
          <span className="text-2xs text-text-tertiary">{t("sp.thinking_default_hint")}</span>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        {inactiveBuiltins.map((level) => (
          <button
            key={level}
            type="button"
            onClick={() => toggle(level)}
            className="inline-flex h-7 items-center gap-1 rounded-lg px-2 font-mono text-xs text-muted-foreground ring-1 ring-inset ring-border-subtle transition-colors hover:bg-surface-hover hover:text-foreground"
          >
            <PlusIcon className="size-3" />
            {level}
          </button>
        ))}
        <div className="inline-flex items-center gap-1">
          <Input
            value={custom}
            onChange={(event) => setCustom(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                addCustom();
              }
            }}
            placeholder={t("sp.custom_level_ph")}
            spellCheck={false}
            className="h-7 w-32 rounded-lg font-mono text-xs shadow-none"
          />
          {custom.trim() && (
            <Button type="button" size="xs" variant="ghost" onClick={addCustom}>
              {t("common.add")}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

// ---------- Endpoint 选择器（Default / Azure OpenAI） ----------

export function EndpointEditor({
  path,
  explicit,
  fallback,
  onError,
}: {
  path: string[];
  explicit: unknown;
  fallback: unknown;
  onError: (error: string | null) => void;
}) {
  const t = useT();
  const effective = explicit ?? fallback;
  const isAzure = isObject(effective) && isObject(effective.Azure);
  const currentVersion =
    isAzure && isObject(effective.Azure) && typeof effective.Azure.api_version === "string"
      ? effective.Azure.api_version
      : "";
  const [version, setVersion] = useState(currentVersion || "2025-04-01-preview");

  useEffect(() => {
    if (currentVersion) setVersion(currentVersion);
  }, [currentVersion]);

  const selectMode = (mode: string) => {
    onError(null);
    if (mode === "default") {
      // 上层继承值是 Azure 时需显式写入 "Default" 才能覆盖；否则删键回落默认即可。
      const fallbackIsAzure = isObject(fallback) && isObject(fallback.Azure);
      void writeField(path, fallbackIsAzure ? "Default" : undefined);
    } else {
      const v = version.trim() || "2025-04-01-preview";
      setVersion(v);
      void writeField(path, { Azure: { api_version: v } });
    }
  };

  const commitVersion = () => {
    const trimmed = version.trim();
    if (!trimmed || /\s/.test(trimmed)) {
      onError(t("sp.azure_version_invalid"));
      return;
    }
    onError(null);
    if (trimmed !== currentVersion) {
      void writeField(path, { Azure: { api_version: trimmed } });
    }
  };

  return (
    <div className="space-y-2">
      <Select value={isAzure ? "azure" : "default"} onValueChange={selectMode}>
        <SelectTrigger size="sm" className="h-8 w-full rounded-lg text-ui shadow-none">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="default">{t("common.default")}</SelectItem>
          <SelectItem value="azure">Azure OpenAI</SelectItem>
        </SelectContent>
      </Select>
      {isAzure && (
        <div className="flex items-center gap-2">
          <span className="shrink-0 font-mono text-2xs text-text-tertiary">api_version</span>
          <Input
            value={version}
            onChange={(event) => setVersion(event.target.value)}
            onBlur={commitVersion}
            onKeyDown={(event) => {
              if (event.key === "Enter") event.currentTarget.blur();
            }}
            placeholder="2025-04-01-preview"
            spellCheck={false}
            className="h-8 rounded-lg font-mono text-xs shadow-none"
          />
        </div>
      )}
    </div>
  );
}

// ---------- Key-Value 键值表编辑器（headers / effort_map） ----------

type KvRow = { id: string; key: string; value: string; fixed?: boolean };

let kvSeq = 0;
function nextKvId(prefix: string): string {
  kvSeq += 1;
  return `${prefix}-${kvSeq}`;
}

function objectToRows(value: unknown, fixedKeys?: readonly string[]): KvRow[] {
  const obj = isObject(value) ? value : {};
  const rows: KvRow[] = [];
  if (fixedKeys) {
    for (const key of fixedKeys) {
      rows.push({
        id: `fixed:${key}`,
        key,
        value: typeof obj[key] === "string" ? obj[key] : obj[key] != null ? String(obj[key]) : "",
        fixed: true,
      });
    }
  }
  for (const [key, val] of Object.entries(obj)) {
    if (fixedKeys?.includes(key)) continue;
    rows.push({
      id: `entry:${key}`,
      key,
      value: typeof val === "string" ? val : val != null ? String(val) : "",
    });
  }
  return rows;
}

export function KeyValueEditor({
  path,
  explicit,
  fallback,
  fixedKeys,
  keyPlaceholder,
  valuePlaceholder,
  allowCustom = true,
}: {
  path: string[];
  explicit: unknown;
  fallback: unknown;
  fixedKeys?: readonly string[];
  keyPlaceholder?: string;
  valuePlaceholder?: string;
  allowCustom?: boolean;
}) {
  const t = useT();
  const initial = useMemo(() => objectToRows(explicit, fixedKeys), [explicit, fixedKeys]);
  const fallbackObj = isObject(fallback) ? fallback : {};
  const [rows, setRows] = useState<KvRow[]>(initial);
  const [newKey, setNewKey] = useState("");
  const [newVal, setNewVal] = useState("");

  useEffect(() => {
    setRows(initial);
  }, [initial]);

  const commit = (nextRows: KvRow[]) => {
    const result: Record<string, string> = {};
    for (const row of nextRows) {
      const k = row.key.trim();
      const v = row.value.trim();
      if (k && v) result[k] = v;
    }
    const nextValue = Object.keys(result).length > 0 ? result : undefined;
    if (sameFlatRecord(explicit, nextValue)) return;
    void writeField(path, nextValue);
  };

  const addRow = () => {
    const k = newKey.trim();
    const v = newVal.trim();
    if (!k) return;
    const next = [...rows.filter((r) => r.key !== k), { id: nextKvId("custom"), key: k, value: v }];
    setRows(next);
    setNewKey("");
    setNewVal("");
    if (v) commit(next);
  };

  return (
    <div className="space-y-2">
      {rows.length === 0 && !allowCustom && (
        <p className="text-xs text-text-tertiary">{t("sf.unset")}</p>
      )}
      {rows.map((row) => {
        const rawInherited = fallbackObj[row.key];
        const inheritedVal = typeof rawInherited === "string" ? rawInherited : undefined;
        return (
          <div key={row.id} className="flex items-center gap-2">
            {row.fixed ? (
              <span className="w-24 shrink-0 truncate font-mono text-xs text-muted-foreground">
                {row.key}
              </span>
            ) : (
              <Input
                value={row.key}
                onChange={(event) =>
                  setRows((prev) =>
                    prev.map((item) =>
                      item.id === row.id ? { ...item, key: event.target.value } : item,
                    ),
                  )
                }
                onBlur={() => commit(rows)}
                placeholder={keyPlaceholder ?? "Header-Name"}
                spellCheck={false}
                className="h-8 w-44 shrink-0 rounded-lg font-mono text-xs shadow-none"
              />
            )}
            <Input
              value={row.value}
              onChange={(event) =>
                setRows((prev) =>
                  prev.map((item) =>
                    item.id === row.id ? { ...item, value: event.target.value } : item,
                  ),
                )
              }
              onBlur={() => commit(rows)}
              onKeyDown={(event) => {
                if (event.key === "Enter") event.currentTarget.blur();
              }}
              placeholder={
                inheritedVal
                  ? t("sf.default_of", { value: inheritedVal })
                  : (valuePlaceholder ?? t("sf.unset"))
              }
              spellCheck={false}
              className="h-8 min-w-0 flex-1 rounded-lg font-mono text-xs shadow-none"
            />
            {!row.fixed && (
              <button
                type="button"
                onClick={() => {
                  const next = rows.filter((item) => item.id !== row.id);
                  setRows(next);
                  commit(next);
                }}
                className="flex size-7 shrink-0 items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-surface-hover hover:text-destructive"
                aria-label={t("sp.remove_model")}
              >
                <Trash2Icon className="size-3.5" />
              </button>
            )}
          </div>
        );
      })}
      {allowCustom && (
        <div className="flex items-center gap-2">
          <Input
            value={newKey}
            onChange={(event) => setNewKey(event.target.value)}
            placeholder={keyPlaceholder ?? t("sp.kv_key_ph")}
            spellCheck={false}
            className="h-8 w-44 shrink-0 rounded-lg font-mono text-xs shadow-none"
          />
          <Input
            value={newVal}
            onChange={(event) => setNewVal(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                addRow();
              }
            }}
            placeholder={valuePlaceholder ?? t("sp.kv_val_ph")}
            spellCheck={false}
            className="h-8 min-w-0 flex-1 rounded-lg font-mono text-xs shadow-none"
          />
          <Button
            type="button"
            size="xs"
            variant="outline"
            disabled={!newKey.trim()}
            onClick={addRow}
          >
            <PlusIcon className="size-3.5" />
            {t("common.add")}
          </Button>
        </div>
      )}
    </div>
  );
}

// ---------- 按需添加 API 请求参数编辑器（替代 40+ 空框瀑布流） ----------

function fieldPathKey(field: ConfigField): string {
  return field.path.join(".");
}

export function RequestParamsEditor({
  fields,
  rawFields,
  doc,
  renderField,
}: {
  /** 结构化 API 请求参数字段（`extra_body.*` / `headers.*`）。 */
  fields: ConfigField[];
  /** 原始 JSON 容器字段（`extra_body` / `headers`），供 Raw JSON 视图切换。 */
  rawFields: ConfigField[];
  doc: SettingsDocument;
  renderField: (field: ConfigField) => React.ReactNode;
}) {
  const t = useT();
  const [pinned, setPinned] = useState<string[]>([]);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [showRaw, setShowRaw] = useState(false);

  const activeFields = useMemo(() => {
    const pinnedSet = new Set(pinned);
    return fields.filter((field) => {
      if (pinnedSet.has(fieldPathKey(field))) return true;
      const explicit = getPath(doc.document, field.path);
      if (explicit !== undefined) return true;
      const inherited = getPath(doc.inherited, field.path);
      if (inherited !== undefined) return true;
      const compatIdx = field.path.indexOf("compat");
      if (compatIdx >= 0) {
        const providerPath = [...field.path.slice(0, 2), ...field.path.slice(compatIdx + 1)];
        if (getPath(doc.effective, providerPath) !== undefined) return true;
      }
      return false;
    });
  }, [fields, pinned, doc]);

  const activeKeys = useMemo(() => new Set(activeFields.map(fieldPathKey)), [activeFields]);

  const available = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return fields.filter((field) => {
      if (activeKeys.has(fieldPathKey(field))) return false;
      if (!needle) return true;
      return `${field.label} ${field.description}`.toLowerCase().includes(needle);
    });
  }, [fields, activeKeys, query]);

  const pinField = (field: ConfigField) => {
    const key = fieldPathKey(field);
    setPinned((prev) => (prev.includes(key) ? prev : [...prev, key]));
    setPickerOpen(false);
    setQuery("");
  };

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="min-w-0">
          <h4 className="text-xs font-medium text-text-tertiary">
            {t("sp.params_title")} · {activeFields.length}/{fields.length}
          </h4>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">{t("sp.params_desc")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {rawFields.length > 0 && (
            <Button
              type="button"
              size="xs"
              variant={showRaw ? "secondary" : "ghost"}
              onClick={() => setShowRaw((prev) => !prev)}
            >
              <Code2Icon className="size-3.5" />
              Raw JSON
            </Button>
          )}
          {!showRaw && (
            <Popover open={pickerOpen} onOpenChange={setPickerOpen}>
              <PopoverTrigger asChild>
                <Button type="button" size="xs" variant="outline" disabled={available.length === 0}>
                  <PlusIcon className="size-3.5" />
                  {t("sp.add_param")}
                </Button>
              </PopoverTrigger>
              <PopoverContent align="end" className="w-80 p-0">
                <div className="flex items-center gap-2 border-b border-border-subtle px-2.5 py-2">
                  <SearchIcon className="size-3.5 shrink-0 text-text-tertiary" />
                  <input
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    placeholder={t("sp.param_search_ph", { n: available.length })}
                    className="h-6 min-w-0 flex-1 bg-transparent text-ui outline-none placeholder:text-text-tertiary"
                  />
                </div>
                <ul className="max-h-64 overflow-y-auto p-1">
                  {available.length === 0 ? (
                    <li className="px-2.5 py-4 text-center text-xs text-text-tertiary">
                      {t("sp.no_param_match")}
                    </li>
                  ) : (
                    available.map((field) => (
                      <li key={fieldPathKey(field)}>
                        <button
                          type="button"
                          onClick={() => pinField(field)}
                          className="flex w-full flex-col rounded-md px-2.5 py-1.5 text-left transition-colors hover:bg-surface-hover"
                        >
                          <span className="truncate font-mono text-xs text-foreground">
                            {field.label}
                          </span>
                          {field.description && (
                            <span className="line-clamp-1 text-2xs text-text-tertiary">
                              {field.description}
                            </span>
                          )}
                        </button>
                      </li>
                    ))
                  )}
                </ul>
              </PopoverContent>
            </Popover>
          )}
        </div>
      </div>

      {showRaw ? (
        <div className="divide-y divide-border-subtle rounded-xl bg-card px-4 ring-1 ring-inset ring-border-subtle">
          {rawFields.map((field) => (
            <div key={fieldPathKey(field)}>{renderField(field)}</div>
          ))}
        </div>
      ) : activeFields.length === 0 ? (
        <div className="rounded-xl bg-card px-4 py-5 text-center text-xs text-text-tertiary ring-1 ring-inset ring-border-subtle">
          {t("sp.params_empty")}
        </div>
      ) : (
        <div className="divide-y divide-border-subtle rounded-xl bg-card px-4 ring-1 ring-inset ring-border-subtle">
          {activeFields.map((field) => (
            <div key={fieldPathKey(field)}>{renderField(field)}</div>
          ))}
        </div>
      )}
    </div>
  );
}

/** 从配置中推断 effort_map 应展示的固定键（内置等级 + 当前模型自定义等级）。 */
export function effortMapKeys(doc: SettingsDocument, path: string[]): string[] {
  const keys: string[] = [...EFFORT_MAP_KEYS];
  const compatIdx = path.indexOf("compat");
  if (compatIdx > 0) {
    const levels = getPath(doc.effective, [...path.slice(0, compatIdx), "thinking_levels"]);
    if (Array.isArray(levels)) {
      for (const item of levels) {
        if (typeof item === "string" && !keys.includes(item)) keys.push(item);
      }
    }
  }
  return keys;
}
