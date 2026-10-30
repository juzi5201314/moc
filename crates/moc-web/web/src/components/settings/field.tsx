// 设置字段：一行一个配置项，左侧标签与说明，右侧编辑器；改动即保存。
// 显示规则：当前范围显式写过的值是输入框内容，继承值 / 默认值作为占位提示；
// 清空或点「重置」删除该键，回落到上一层。

import { RotateCcwIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
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
import { choiceDisplay, choiceValue, formatField, getPath, parseField } from "@/lib/settings-doc";
import { cn } from "@/lib/utils";
import type { ConfigField, SettingsDocument } from "@/protocol/types";
import { useSettings, writeField } from "./settings-store";

const INHERIT = "__inherit__";

/** 字段在当前范围的显式值与回落值（继承 → 默认）。 */
export function fieldValues(doc: SettingsDocument, path: string[]) {
  const explicit = getPath(doc.document, path);
  const inherited = getPath(doc.inherited, path);
  const fallback = inherited !== undefined ? inherited : getPath(doc.defaults, path);
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
  const scope = useSettings((state) => state.scope);
  return (
    <div className="group/row py-3.5">
      <div className={cn("flex gap-6", wide ? "flex-col gap-2.5" : "items-center justify-between")}>
        <div className="min-w-0 flex-1" title={title}>
          <div className="flex items-center gap-2 text-[13.5px] font-medium text-foreground">
            <span className="truncate">{label}</span>
            {overridden && scope === "project" && (
              <span className="shrink-0 rounded bg-brand/10 px-1.5 py-px text-[10.5px] font-medium text-brand">
                项目覆盖
              </span>
            )}
            {overridden && onReset && (
              <button
                type="button"
                onClick={onReset}
                className="shrink-0 rounded p-0.5 text-text-tertiary opacity-0 transition-opacity group-hover/row:opacity-100 hover:text-foreground focus-visible:opacity-100"
                title="恢复默认（删除此项）"
                aria-label="恢复默认"
              >
                <RotateCcwIcon className="size-3" />
              </button>
            )}
          </div>
          {description && (
            <p className="mt-0.5 text-[12.5px] leading-5 text-muted-foreground">{description}</p>
          )}
        </div>
        <div className={cn("shrink-0", wide ? "w-full" : "w-[260px] max-w-[45%]")}>{children}</div>
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
  const multiline = field.kind.type === "multiline" || field.kind.type === "object";

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
          "min-h-24 rounded-lg text-[13px] shadow-none",
          field.kind.type === "object" && "font-mono text-[12px]",
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
        "h-8 rounded-lg text-[13px] shadow-none",
        (field.kind.type === "integer" || field.kind.type === "number") && "font-mono tabular-nums",
      )}
    />
  );
}

export function FieldRow({ field, doc }: { field: ConfigField; doc: SettingsDocument }) {
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
  } else if (kind.type === "choice") {
    const current = overridden ? formatField(explicit, kind) : INHERIT;
    const fallbackText = formatField(fallback, kind);
    const fallbackOption = kind.options.find((option) => option.value === fallbackText);
    editor = (
      <Select
        value={current}
        onValueChange={(value) =>
          void writeField(
            field.path,
            value === INHERIT ? undefined : choiceValue(field.path, value),
          )
        }
      >
        <SelectTrigger
          size="sm"
          className="h-8 w-full rounded-lg text-[13px] shadow-none"
          aria-label={field.label}
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={INHERIT}>
            默认
            {fallbackOption
              ? `（${fallbackOption.label}）`
              : fallbackText
                ? `（${fallbackText}）`
                : ""}
          </SelectItem>
          {kind.options.map((option) => (
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
          ? "已从上层配置继承"
          : "未设置"
        : fallbackText
          ? kind.type === "object" || kind.type === "multiline"
            ? fallbackText
            : `默认 ${fallbackText}`
          : "未设置";
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
      wide={kind.type === "multiline" || kind.type === "object"}
      error={error}
    >
      {editor}
    </SettingRow>
  );
}

const GROUP_TITLES: Record<string, string> = {
  compaction: "上下文压缩",
  agents: "子代理",
  plugins: "插件",
  mcp: "MCP",
  skills: "技能",
  sandbox: "沙盒",
  session: "会话存储",
  tools: "通用",
  "compaction.threshold": "触发条件",
  "compaction.remote": "服务端压缩",
  "compaction.shake": "旧结果修剪",
  limits: "请求与循环",
  "tools.bash": "Bash",
  "tools.bash.limits": "Bash 资源限制",
  "tools.edit": "编辑",
  "tools.find": "文件查找",
  "tools.grep": "内容搜索",
  "tools.job": "后台任务",
  "tools.output": "工具输出",
  "tools.ptc": "PTC 代码沙盒",
  "tools.read": "读取",
  "tools.todo": "任务清单",
  "tools.write": "写入",
  web_fetch: "网页抓取",
  web_search: "网页搜索",
  workspace: "工作区",
  "session.retention": "保留策略",
  context_files: "指令文件",
  system_prompt: "系统提示",
  "plugins.limits": "插件限制",
  retry: "失败重试",
  compat: "协议兼容",
  "compat.thinking": "思考参数",
};

/** 字段归组：去掉末段后的路径；同组相邻排列。 */
function groupKey(path: string[], strip: number): string {
  return path.slice(strip, -1).join(".");
}

export function FieldList({
  fields,
  doc,
  strip = 0,
  showGroups = true,
}: {
  fields: ConfigField[];
  doc: SettingsDocument;
  /** 归组时忽略的前缀段数（例如供应商内字段去掉 `providers.<id>`）。 */
  strip?: number;
  showGroups?: boolean;
}) {
  const [advanced, setAdvanced] = useState(false);
  const basic = fields.filter((field) => !field.advanced);
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
              {GROUP_TITLES[group.key] ?? group.key.split(".").at(-1)}
            </h3>
          )}
          <div className="divide-y divide-border-subtle rounded-xl border border-border bg-card px-4">
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
          {advanced ? "隐藏高级选项" : `显示高级选项（${advancedCount}）`}
        </button>
      )}
    </div>
  );
}
