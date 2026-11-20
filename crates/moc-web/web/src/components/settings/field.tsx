// 设置字段：一行一个配置项，左侧标签与说明，右侧编辑器；改动即保存。
// 显示规则：当前范围显式写过的值是输入框内容，继承值 / 默认值作为占位提示；
// 清空或点「重置」删除该键，回落到上一层。

import { RotateCcwIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { Badge } from "@/components/ui/badge";
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
import { type MsgKey, useT } from "@/lib/i18n";
import { choiceDisplay, choiceValue, formatField, getPath, parseField } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import type { ConfigField, SettingsDocument } from "@/protocol/types";
import { useSettings, writeField } from "./settings-store";

const INHERIT = "__inherit__";

/** 字段在当前范围的显式值与回落值（继承 → 默认）。 */
export function fieldValues(doc: SettingsDocument, path: string[]) {
  const explicit = getPath(doc.document, path);
  const inherited = getPath(doc.inherited, path);
  const compat = path.indexOf("compat");
  const providerDefault =
    compat >= 0 && (path[compat + 1] === "extra_body" || path[compat + 1] === "headers")
      ? getPath(doc.effective, [...path.slice(0, 2), ...path.slice(compat + 1)])
      : undefined;
  const fallback =
    inherited ??
    providerDefault ??
    getPath(doc.defaults, path) ??
    (explicit === undefined ? getPath(doc.effective, path) : undefined);
  return { explicit, fallback, effective: getPath(doc.effective, path) };
}

export function SettingRow({
  label,
  description,
  title,
  overridden,
  onReset,
  wide,
  error,
  children,
}: {
  label: ReactNode;
  description?: string;
  title?: string;
  overridden?: boolean;
  onReset?: () => void;
  /** 编辑器另起一行占满宽度（多行文本、JSON）。 */
  wide?: boolean;
  error?: string | null;
  children: ReactNode;
}) {
  const t = useT();
  const scope = useSettings((state) => state.scope);
  return (
    <div className="group/row py-3.5">
      <div
        className={cn(
          "flex",
          wide
            ? "flex-col gap-2.5"
            : "flex-col gap-2.5 md:flex-row md:items-center md:justify-between md:gap-6",
        )}
      >
        <div className="min-w-0 flex-1" title={title}>
          <div className="flex items-center gap-2 text-sm font-medium text-foreground">
            <span className="truncate">{label}</span>
            {overridden && scope === "project" && (
              <Badge variant="brand">{t("sf.scope_override")}</Badge>
            )}
            {overridden && onReset && (
              <button
                type="button"
                onClick={onReset}
                className="shrink-0 rounded p-0.5 text-text-tertiary opacity-0 transition-opacity group-hover/row:opacity-100 hover:text-foreground focus-visible:opacity-100"
                title={t("sf.reset_hint")}
                aria-label={t("sf.reset")}
              >
                <RotateCcwIcon className="size-3" />
              </button>
            )}
          </div>
          {description && (
            <p className="mt-0.5 break-words text-xs leading-5 text-muted-foreground">
              {Array.from(description.matchAll(/`([^`]+)`|([^`]+)/g), (match) =>
                match[1] ? (
                  <code key={match.index} className="font-mono text-2xs">
                    {match[1]}
                  </code>
                ) : (
                  match[2]
                ),
              )}
            </p>
          )}
        </div>
        <div
          className={cn("min-w-0 shrink-0", wide ? "w-full" : "w-full md:w-[260px] md:max-w-[45%]")}
        >
          {children}
        </div>
      </div>
      {error && <p className="mt-1.5 text-xs text-destructive">{error}</p>}
    </div>
  );
}

/** 文本类编辑器：本地草稿，失焦或回车时解析并保存；非聚焦时跟随文档。 */
function TextEditor({
  field,
  explicitText,
  placeholder,
  onError,
}: {
  field: ConfigField;
  explicitText: string;
  placeholder: string;
  onError: (error: string | null) => void;
}) {
  const [draft, setDraft] = useState(explicitText);
  const [focused, setFocused] = useState(false);
  const multiline =
    field.kind.type === "multiline" || field.kind.type === "object" || field.kind.type === "json";

  useEffect(() => {
    if (!focused) setDraft(explicitText);
  }, [explicitText, focused]);

  const commit = () => {
    if (draft === explicitText) return;
    const parsed = parseField(draft, field.kind);
    if (!parsed.ok) {
      onError(parsed.error);
      return;
    }
    onError(null);
    void writeField(field.path, parsed.value);
  };

  const common = {
    value: draft,
    placeholder,
    onFocus: () => setFocused(true),
    onBlur: () => {
      setFocused(false);
      commit();
    },
    "aria-label": field.label,
  };

  if (multiline) {
    return (
      <Textarea
        {...common}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) commit();
        }}
        className={cn(
          "min-h-24 rounded-lg text-ui shadow-none",
          (field.kind.type === "object" || field.kind.type === "json") && "font-mono text-xs",
        )}
      />
    );
  }
  return (
    <Input
      {...common}
      type={field.kind.type === "secret" ? "password" : "text"}
      autoComplete="off"
      spellCheck={false}
      onChange={(event) => setDraft(event.target.value)}
      onKeyDown={(event) => {
        if (event.key === "Enter") event.currentTarget.blur();
        if (event.key === "Escape") {
          setDraft(explicitText);
          onError(null);
        }
      }}
      className={cn(
        "h-8 rounded-lg text-ui shadow-none",
        ["integer", "number", "signed_integer", "signed_number"].includes(field.kind.type) &&
          "font-mono tabular-nums",
      )}
    />
  );
}

export function FieldRow({ field, doc }: { field: ConfigField; doc: SettingsDocument }) {
  const t = useT();
  const [error, setError] = useState<string | null>(null);
  const { explicit, fallback } = fieldValues(doc, field.path);
  const overridden = explicit !== undefined;
  const reset = () => {
    setError(null);
    void writeField(field.path, undefined);
  };
  const kind = field.kind;

  let editor: ReactNode;
  if (kind.type === "bool") {
    editor = (
      <div className="flex justify-end">
        <Switch
          checked={Boolean(overridden ? explicit : fallback)}
          onCheckedChange={(checked) => void writeField(field.path, checked)}
          aria-label={field.label}
        />
      </div>
    );
  } else if (kind.type === "choice" || kind.type === "optional_bool") {
    const options =
      kind.type === "choice"
        ? kind.options
        : [
            { value: "true", label: t("common.on") },
            { value: "false", label: t("common.off") },
          ];
    const current = overridden ? formatField(explicit, kind) : INHERIT;
    const fallbackText = formatField(fallback, kind);
    const fallbackOption = options.find((option) => option.value === fallbackText);
    editor = (
      <Select
        value={current}
        onValueChange={(value) =>
          void writeField(
            field.path,
            value === INHERIT
              ? undefined
              : kind.type === "optional_bool"
                ? value === "true"
                : choiceValue(field.path, value),
          )
        }
      >
        <SelectTrigger
          size="sm"
          className="h-8 w-full rounded-lg text-ui shadow-none"
          aria-label={field.label}
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={INHERIT}>
            {t("common.default")}
            {fallbackOption
              ? t("sf.inherit_of", { label: fallbackOption.label })
              : fallbackText
                ? t("sf.inherit_of", { label: fallbackText })
                : ""}
          </SelectItem>
          {options.map((option) => (
            <SelectItem key={option.value} value={option.value}>
              {choiceDisplay(option)}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    );
  } else {
    const fallbackText = formatField(fallback, kind);
    const placeholder =
      kind.type === "secret"
        ? fallback
          ? t("sf.inherited_ph")
          : t("sf.unset")
        : fallbackText
          ? kind.type === "object" || kind.type === "multiline" || kind.type === "json"
            ? fallbackText
            : t("sf.default_of", { value: fallbackText })
          : t("sf.unset");
    editor = (
      <TextEditor
        field={field}
        explicitText={formatField(explicit, kind)}
        placeholder={placeholder}
        onError={setError}
      />
    );
  }

  return (
    <SettingRow
      label={field.label}
      description={field.description}
      title={field.path.join(".")}
      overridden={overridden}
      onReset={reset}
      wide={kind.type === "multiline" || kind.type === "object" || kind.type === "json"}
      error={error}
    >
      {editor}
    </SettingRow>
  );
}

const GROUP_TITLES: Record<string, MsgKey> = {
  compaction: "sg.compaction",
  agents: "sg.agents",
  plugins: "sg.plugins",
  mcp: "sg.mcp",
  skills: "sg.skills",
  sandbox: "sg.sandbox",
  session: "sg.session",
  tools: "sg.tools",
  "compaction.threshold": "sg.compaction_threshold",
  "compaction.shake": "sg.compaction_shake",
  limits: "sg.limits",
  "tools.bash": "sg.tools_bash",
  "tools.bash.limits": "sg.tools_bash_limits",
  "tools.edit": "sg.tools_edit",
  "tools.find": "sg.tools_find",
  "tools.grep": "sg.tools_grep",
  "tools.job": "sg.tools_job",
  "tools.output": "sg.tools_output",
  "tools.ptc": "sg.tools_ptc",
  "tools.read": "sg.tools_read",
  "tools.todo": "sg.tools_todo",
  "tools.write": "sg.tools_write",
  web_fetch: "sg.web_fetch",
  workspace: "sg.workspace",
  "session.retention": "sg.session_retention",
  context_files: "sg.context_files",
  system_prompt: "sg.system_prompt",
  "plugins.limits": "sg.plugins_limits",
  retry: "sg.retry",
  compat: "sg.compat",
  "compat.thinking": "sg.compat_thinking",
  headers: "sg.headers",
  extra_body: "sg.extra_body",
  "extra_body.reasoning": "sg.eb_reasoning",
  "extra_body.thinking": "sg.eb_thinking",
  "extra_body.text": "sg.eb_text",
  "extra_body.text.format": "sg.eb_text_format",
  "extra_body.response_format": "sg.eb_response_format",
  "extra_body.response_format.json_schema": "sg.eb_response_format_json",
  "extra_body.output_config": "sg.eb_output_config",
  "extra_body.output_config.format": "sg.eb_output_config_format",
  "extra_body.metadata": "sg.eb_metadata",
  "extra_body.prediction": "sg.eb_prediction",
  "extra_body.prompt": "sg.eb_prompt",
  "extra_body.prompt_cache_options": "sg.eb_prompt_cache",
  "extra_body.cache_control": "sg.eb_cache_control",
  "extra_body.stream_options": "sg.eb_stream_options",
  "extra_body.tool_choice": "sg.eb_tool_choice",
  "extra_body.moderation": "sg.eb_moderation",
  "extra_body.moderation.policy.input": "sg.eb_mod_input",
  "extra_body.moderation.policy.output": "sg.eb_mod_output",
  "extra_body.image": "sg.eb_image",
  "extra_body.access_programs": "sg.eb_access_programs",
  "extra_body.diagnostics": "sg.eb_diagnostics",
};

/** 字段归组：去掉末段后的路径；同组相邻排列。 */
function groupKey(path: string[], strip: number): string {
  const parent = path.slice(strip, -1);
  if (parent[0] === "compat" && ["extra_body", "headers"].includes(parent[1])) parent.shift();
  return parent.join(".");
}

export function FieldList({
  fields,
  doc,
  strip = 0,
  showGroups = true,
  includeAdvanced = false,
}: {
  fields: ConfigField[];
  doc: SettingsDocument;
  /** 归组时忽略的前缀段数（例如供应商内字段去掉 `providers.<id>`）。 */
  strip?: number;
  showGroups?: boolean;
  /** 由外层容器自行折叠高级项时直接渲染全部字段。 */
  includeAdvanced?: boolean;
}) {
  const t = useT();
  const [advanced, setAdvanced] = useState(false);
  const basic = includeAdvanced ? fields : fields.filter((field) => !field.advanced);
  const advancedCount = fields.length - basic.length;
  const visible = advanced ? fields : basic;

  const groups = useMemo(() => {
    const result: { key: string; fields: ConfigField[] }[] = [];
    for (const field of visible) {
      const key = groupKey(field.path, strip);
      const group = result.find((item) => item.key === key);
      if (group) group.fields.push(field);
      else result.push({ key, fields: [field] });
    }
    return result;
  }, [visible, strip]);

  return (
    <div className="space-y-5">
      {groups.map((group) => (
        <section key={group.key || "_"}>
          {showGroups && groups.length > 1 && group.key && (
            <h3 className="mb-1 text-xs font-medium text-text-tertiary">
              {GROUP_TITLES[group.key] ? t(GROUP_TITLES[group.key]) : group.key.split(".").at(-1)}
            </h3>
          )}
          <div className="divide-y divide-border-subtle rounded-xl bg-card px-4 ring-1 ring-inset ring-border-subtle">
            {group.fields.map((field) => (
              <FieldRow key={field.path.join(".")} field={field} doc={doc} />
            ))}
          </div>
        </section>
      ))}
      {advancedCount > 0 && (
        <button
          type="button"
          onClick={() => setAdvanced((value) => !value)}
          className="text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          {advanced ? t("sf.advanced_hide") : t("sf.advanced_show", { n: advancedCount })}
        </button>
      )}
    </div>
  );
}
