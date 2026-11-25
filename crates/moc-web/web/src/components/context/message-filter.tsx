// 消息目录多选筛选小窗：按来源（Origin）、线上角色（Role）与正文可见性（Content）组合过滤当前页。
// 过滤语义为标准 Faceted Filter：同组多选取并集（OR），跨组取交集（AND），空组表示不限制。

import { CheckIcon, ListFilterIcon, XIcon } from "lucide-react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import type { ContextNode } from "@/protocol/types";
import { ORIGIN_ORDER, originLabel, originTone } from "./section-tone";

export type MessageVisibility = "readable" | "hidden";

export type MessageFilterState = {
  origins: ReadonlySet<string>;
  roles: ReadonlySet<string>;
  visibility: ReadonlySet<MessageVisibility>;
};

export const EMPTY_MESSAGE_FILTER: MessageFilterState = {
  origins: new Set(),
  roles: new Set(),
  visibility: new Set(),
};

export type FacetOption<T extends string = string> = {
  value: T;
  count: number;
};

export type FilteredMessages = {
  shown: ContextNode[];
  origins: FacetOption[];
  roles: FacetOption[];
  visibility: FacetOption<MessageVisibility>[];
  showVisibility: boolean;
  activeCount: number;
};

const ORIGIN_RANK = new Map(ORIGIN_ORDER.map((key, index) => [key, index]));
const ROLE_ORDER = ["user", "assistant", "tool", "developer", "system"] as const;
const ROLE_RANK = new Map<string, number>(ROLE_ORDER.map((key, index) => [key, index]));

function compareByRank(a: string, b: string, rank: Map<string, number>): number {
  const ra = rank.get(a);
  const rb = rank.get(b);
  if (ra !== undefined && rb !== undefined) return ra - rb;
  if (ra !== undefined) return -1;
  if (rb !== undefined) return 1;
  return a.localeCompare(b);
}

function toggleSet<T>(source: ReadonlySet<T>, value: T): Set<T> {
  const next = new Set(source);
  if (next.has(value)) next.delete(value);
  else next.add(value);
  return next;
}

/**
 * 单次线性扫描当前页消息：同时产出匹配列表与各维度的交叉联动计数（Disjunctive Faceting）。
 * 同组内计入不受本组限制、但满足其余组的条数，以便准确预览勾选该项后可见的条数。
 */
export function applyMessageFilter(
  rows: readonly ContextNode[],
  filter: MessageFilterState,
): FilteredMessages {
  const shown: ContextNode[] = [];
  const presentOrigins = new Set<string>(filter.origins);
  const presentRoles = new Set<string>(filter.roles);
  const originCounts = new Map<string, number>();
  const roleCounts = new Map<string, number>();
  let readableCount = 0;
  let hiddenCount = 0;
  let hasHiddenRow = false;

  for (const row of rows) {
    const origin = row.origin ?? null;
    const role = row.role ?? null;
    const isHidden = Boolean(row.hidden);
    const vis: MessageVisibility = isHidden ? "hidden" : "readable";

    if (origin) presentOrigins.add(origin);
    if (role) presentRoles.add(role);
    if (isHidden) hasHiddenRow = true;

    const originOk = filter.origins.size === 0 || (origin !== null && filter.origins.has(origin));
    const roleOk = filter.roles.size === 0 || (role !== null && filter.roles.has(role));
    const visOk = filter.visibility.size === 0 || filter.visibility.has(vis);

    if (origin && roleOk && visOk) {
      originCounts.set(origin, (originCounts.get(origin) ?? 0) + 1);
    }
    if (role && originOk && visOk) {
      roleCounts.set(role, (roleCounts.get(role) ?? 0) + 1);
    }
    if (originOk && roleOk) {
      if (isHidden) hiddenCount += 1;
      else readableCount += 1;
    }
    if (originOk && roleOk && visOk) {
      shown.push(row);
    }
  }

  const origins = [...presentOrigins]
    .sort((a, b) => compareByRank(a, b, ORIGIN_RANK))
    .map((value) => ({ value, count: originCounts.get(value) ?? 0 }));
  const roles = [...presentRoles]
    .sort((a, b) => compareByRank(a, b, ROLE_RANK))
    .map((value) => ({ value, count: roleCounts.get(value) ?? 0 }));
  const showVisibility = filter.visibility.size > 0 || hasHiddenRow;
  const visibility: FacetOption<MessageVisibility>[] = showVisibility
    ? [
        { value: "readable", count: readableCount },
        { value: "hidden", count: hiddenCount },
      ]
    : [];

  return {
    shown,
    origins,
    roles,
    visibility,
    showVisibility,
    activeCount: filter.origins.size + filter.roles.size + filter.visibility.size,
  };
}

export function MessageFilterBar({
  filter,
  filtered,
  totalCount,
  onChange,
}: {
  filter: MessageFilterState;
  filtered: FilteredMessages;
  totalCount: number;
  onChange: (next: MessageFilterState) => void;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);

  const toggleOrigin = (value: string) =>
    onChange({ ...filter, origins: toggleSet(filter.origins, value) });
  const toggleRole = (value: string) =>
    onChange({ ...filter, roles: toggleSet(filter.roles, value) });
  const toggleVisibility = (value: MessageVisibility) =>
    onChange({ ...filter, visibility: toggleSet(filter.visibility, value) });
  const reset = () => onChange(EMPTY_MESSAGE_FILTER);

  const visibilityLabel = (value: MessageVisibility) =>
    value === "readable" ? t("cv.filter_readable") : t("cv.filter_hidden");

  const activeOrigins = filtered.origins.filter((item) => filter.origins.has(item.value));
  const activeRoles = filtered.roles.filter((item) => filter.roles.has(item.value));
  const activeVisibility = filtered.visibility.filter((item) => filter.visibility.has(item.value));
  const hasAnyGroups =
    filtered.origins.length > 0 || filtered.roles.length > 0 || filtered.showVisibility;

  return (
    <div className="flex flex-wrap items-center gap-1.5 border-b border-border-subtle px-5 pb-2.5">
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="xs"
            disabled={!hasAnyGroups}
            className={cn(
              "h-7 gap-1.5 px-2 text-xs font-medium text-muted-foreground hover:text-foreground",
              (open || filtered.activeCount > 0) && "bg-accent text-foreground",
            )}
          >
            <ListFilterIcon className="size-3.5" />
            <span>{t("cv.filter")}</span>
            {filtered.activeCount > 0 && (
              <Badge variant="brand" className="px-1.5 py-0 font-mono tabular-nums">
                {filtered.activeCount}
              </Badge>
            )}
          </Button>
        </PopoverTrigger>
        <PopoverContent
          align="start"
          side="bottom"
          sideOffset={6}
          // 阻止外层 Dialog 的滚动锁吞掉浮层内部滚轮事件
          onWheel={(event) => event.stopPropagation()}
          className="w-60 rounded-[10px] bg-popover/95 p-1 backdrop-blur-md"
        >
          <div className="max-h-80 overflow-y-auto">
            {filtered.origins.length > 0 && (
              <FilterGroup title={t("cv.filter_origin")}>
                {filtered.origins.map((item) => (
                  <FilterOptionRow
                    key={item.value}
                    checked={filter.origins.has(item.value)}
                    count={item.count}
                    dotClassName={originTone(item.value)}
                    label={originLabel(item.value)}
                    onToggle={() => toggleOrigin(item.value)}
                  />
                ))}
              </FilterGroup>
            )}
            {filtered.origins.length > 0 && filtered.roles.length > 0 && (
              <div className="-mx-1 my-1 h-px bg-border-subtle" />
            )}
            {filtered.roles.length > 0 && (
              <FilterGroup title={t("cv.filter_role")}>
                {filtered.roles.map((item) => (
                  <FilterOptionRow
                    key={item.value}
                    checked={filter.roles.has(item.value)}
                    count={item.count}
                    label={item.value}
                    mono
                    onToggle={() => toggleRole(item.value)}
                  />
                ))}
              </FilterGroup>
            )}
            {(filtered.origins.length > 0 || filtered.roles.length > 0) &&
              filtered.showVisibility && <div className="-mx-1 my-1 h-px bg-border-subtle" />}
            {filtered.showVisibility && (
              <FilterGroup title={t("cv.filter_content")}>
                {filtered.visibility.map((item) => (
                  <FilterOptionRow
                    key={item.value}
                    checked={filter.visibility.has(item.value)}
                    count={item.count}
                    label={visibilityLabel(item.value)}
                    onToggle={() => toggleVisibility(item.value)}
                  />
                ))}
              </FilterGroup>
            )}
          </div>
          {filtered.activeCount > 0 && (
            <>
              <div className="-mx-1 my-1 h-px bg-border-subtle" />
              <button
                type="button"
                onClick={reset}
                className="flex w-full items-center justify-center rounded-md py-1.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:bg-accent focus-visible:outline-none"
              >
                {t("cv.filter_clear")}
              </button>
            </>
          )}
        </PopoverContent>
      </Popover>

      {activeOrigins.map((item) => {
        const label = originLabel(item.value);
        return (
          <FilterChip
            key={`origin:${item.value}`}
            dotClassName={originTone(item.value)}
            label={label}
            removeLabel={t("cv.filter_remove", { label })}
            onRemove={() => toggleOrigin(item.value)}
          />
        );
      })}
      {activeRoles.map((item) => (
        <FilterChip
          key={`role:${item.value}`}
          prefix={t("cv.filter_role").toLowerCase()}
          label={item.value}
          mono
          removeLabel={t("cv.filter_remove", { label: item.value })}
          onRemove={() => toggleRole(item.value)}
        />
      ))}
      {activeVisibility.map((item) => {
        const label = visibilityLabel(item.value);
        return (
          <FilterChip
            key={`visibility:${item.value}`}
            label={label}
            removeLabel={t("cv.filter_remove", { label })}
            onRemove={() => toggleVisibility(item.value)}
          />
        );
      })}

      {filtered.activeCount > 0 && (
        <Button
          type="button"
          variant="ghost"
          size="xs"
          onClick={reset}
          className="h-6 px-1.5 text-xs text-muted-foreground hover:text-foreground"
        >
          {t("cv.filter_reset")}
        </Button>
      )}

      <span className="ml-auto font-mono text-2xs text-text-tertiary tabular-nums">
        {filtered.activeCount > 0 ? `${filtered.shown.length} / ${totalCount}` : `${totalCount}`}
      </span>
    </div>
  );
}

function FilterGroup({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <fieldset className="m-0 border-0 p-0">
      <legend className="px-2 pt-1.5 pb-1 text-2xs font-medium text-text-tertiary">{title}</legend>
      {children}
    </fieldset>
  );
}

/**
 * 筛选小窗内的复选行：原生 checkbox 语义配合 DESIGN.md 灰阶发丝框与 muted-foreground 勾，
 * 不用强调色大块填充，勾选前后几何尺寸与位置零跳变。
 */
function FilterOptionRow({
  checked,
  count,
  dotClassName,
  label,
  mono,
  onToggle,
}: {
  checked: boolean;
  count: number;
  dotClassName?: string;
  label: string;
  mono?: boolean;
  onToggle: () => void;
}) {
  return (
    <label className="flex w-full cursor-pointer select-none items-center gap-2 rounded-md px-2 py-1.5 text-left text-ui text-foreground transition-colors hover:bg-accent has-[:focus-visible]:bg-accent">
      <input type="checkbox" checked={checked} onChange={onToggle} className="sr-only" />
      <span
        aria-hidden="true"
        className={cn(
          "flex size-3.5 shrink-0 items-center justify-center rounded-[4px] border transition-colors",
          checked
            ? "border-muted-foreground/50 bg-accent text-muted-foreground"
            : "border-border bg-transparent",
        )}
      >
        <CheckIcon className={cn("size-2.5", !checked && "invisible")} />
      </span>
      {dotClassName && (
        <span className={cn("size-1.5 shrink-0 rounded-full", dotClassName)} aria-hidden="true" />
      )}
      <span className={cn("min-w-0 flex-1 truncate", mono && "font-mono text-xs")}>{label}</span>
      <span
        aria-hidden="true"
        className="ml-auto font-mono text-2xs text-text-tertiary tabular-nums"
      >
        {count}
      </span>
    </label>
  );
}

function FilterChip({
  dotClassName,
  prefix,
  label,
  mono,
  removeLabel,
  onRemove,
}: {
  dotClassName?: string;
  prefix?: string;
  label: string;
  mono?: boolean;
  removeLabel: string;
  onRemove: () => void;
}) {
  return (
    <span className="inline-flex h-6 items-center gap-1.5 rounded-md bg-accent pl-2 pr-1 text-xs text-foreground">
      {dotClassName && (
        <span className={cn("size-1.5 shrink-0 rounded-full", dotClassName)} aria-hidden="true" />
      )}
      {prefix && <span className="text-2xs text-text-tertiary">{prefix}</span>}
      <span className={cn(mono && "font-mono text-2xs")}>{label}</span>
      <button
        type="button"
        onClick={onRemove}
        aria-label={removeLabel}
        className="inline-flex size-4 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-surface-hover hover:text-foreground focus-visible:outline-none"
      >
        <XIcon className="size-3" />
      </button>
    </span>
  );
}
