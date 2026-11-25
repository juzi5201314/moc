// 斜杠命令交互浮层：
// 1. CommandMenu：一级分组/搜索列表 + 锚定在当前高亮行旁的二级面板（动作子菜单 / 结构化参数卡 / 简卡）；
// 2. CommandArgMenu：阶段二子命令与动态参数补全（含多级钻取与实时状态标记）；
// 3. CommandParamHint：阶段二自由文本参数引导条（非阻塞，必填缺省回车时轻强调）。

import { CheckIcon, ChevronRightIcon } from "lucide-react";
import { type RefObject, useEffect, useMemo, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Popover, PopoverAnchor, PopoverContent } from "@/components/ui/popover";
import {
  buildSubmenuItems,
  type ChoiceStatusTone,
  type CommandContext,
  type CommandEntry,
  commandTrailing,
  type Stage2State,
  type SubmenuAction,
  type SubmenuSection,
} from "@/lib/command-params";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import type { CommandParamMeta } from "@/protocol/types";
import { Highlighted, useSuggestionLayout } from "./composer-parts";

/** 状态小圆点（仅用于实时状态类动态候选，如 MCP server）。 */
function StatusDot({ tone }: { tone: ChoiceStatusTone }) {
  return (
    <span
      aria-hidden="true"
      className={cn(
        "size-1.5 shrink-0 rounded-full",
        tone === "success" && "bg-success",
        tone === "warn" && "bg-warn",
        tone === "destructive" && "bg-destructive",
        tone === "muted" && "bg-text-tertiary/60",
      )}
    />
  );
}

/** 必填 / 可选参数状态徽标：必填用 brand 淡底（缺省报错时切 destructive 淡底），可选用低对比中性灰底。 */
function RequirementBadge({ required, error = false }: { required: boolean; error?: boolean }) {
  const t = useT();
  if (required) {
    return (
      <Badge
        variant="brand"
        className={cn("font-normal", error && "bg-destructive/10 text-destructive")}
      >
        {t("cmd.required")}
      </Badge>
    );
  }
  return (
    <span className="inline-flex shrink-0 items-center rounded bg-[var(--inline-code-bg)] px-1.5 py-px text-2xs text-muted-foreground">
      {t("cmd.optional")}
    </span>
  );
}

/** 参数结构化说明块：用于自由文本/路径参数命令的二级说明卡。 */
function ParamSpecCard({ param }: { param: CommandParamMeta }) {
  const t = useT();
  return (
    <div className="space-y-1 px-2 py-1.5">
      <div className="flex items-center justify-between gap-2">
        <span className="font-mono text-xs font-medium text-foreground">{param.name}</span>
        <RequirementBadge required={param.required} />
      </div>
      <p className="text-xs leading-relaxed text-muted-foreground">{param.description}</p>
      {param.kind === "path" && param.placeholder && (
        <p className="font-mono text-2xs text-text-tertiary">{param.placeholder}</p>
      )}
      {param.defaultBehavior && (
        <p className="text-2xs leading-relaxed text-text-tertiary">
          <span className="font-medium text-muted-foreground">{t("cmd.default")}: </span>
          {param.defaultBehavior}
        </p>
      )}
    </div>
  );
}

/** 二级面板正文：按命令形态自适应为动作/枚举子菜单、结构化参数卡或精简说明卡。 */
function CommandDetailBody({
  entry,
  sections,
  flatActions,
  focusPane,
  subActive,
  onPickSubAction,
  onHoverSubAction,
}: {
  entry: CommandEntry;
  sections: SubmenuSection[];
  flatActions: SubmenuAction[];
  focusPane: "list" | "sub";
  subActive: number;
  onPickSubAction: (action: SubmenuAction) => void;
  onHoverSubAction: (index: number) => void;
}) {
  const t = useT();
  const { command, disabledReason } = entry;
  const disabled = disabledReason !== null;
  const hasSubmenu = flatActions.length > 0;
  const textParams = !hasSubmenu && command.params?.length ? command.params : undefined;
  const legacyUsage = !command.params?.length ? command.usage : undefined;

  let actionCursor = 0;

  return (
    <div className="space-y-1 text-xs">
      <div className="px-2 pt-1.5 pb-1">
        <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span className="font-mono text-xs font-medium text-foreground">/{command.name}</span>
          {command.aliases && command.aliases.length > 0 && (
            <span className="font-mono text-2xs text-text-tertiary">
              {command.aliases.map((alias) => `/${alias}`).join(" · ")}
            </span>
          )}
          {command.plugin && (
            <span className="ml-auto text-2xs text-text-tertiary">
              {t("cmd.plugin_source", { name: command.plugin })}
            </span>
          )}
        </div>
        {command.description && (
          <p className="mt-1 text-xs leading-relaxed break-words text-muted-foreground">
            {command.description}
          </p>
        )}
        {disabledReason && (
          <p className="mt-1.5 rounded-md bg-muted/80 px-2 py-1 text-2xs text-muted-foreground">
            {disabledReason}
          </p>
        )}
      </div>

      {hasSubmenu && (
        <div
          className="border-t border-border-subtle pt-1"
          role="menu"
          aria-label={`/${command.name}`}
        >
          {sections.map((section, sectionIndex) => (
            <div
              key={section.key}
              className={cn(sectionIndex > 0 && "mt-1 border-t border-border-subtle pt-1")}
            >
              {section.heading && (
                <div className="px-2 py-1 text-2xs font-medium text-text-tertiary">
                  {section.heading}
                </div>
              )}
              <div className="space-y-0.5">
                {section.items.map((action) => {
                  const index = actionCursor;
                  actionCursor += 1;
                  const highlighted = focusPane === "sub" && index === subActive;
                  return (
                    <button
                      key={action.key}
                      type="button"
                      role="menuitem"
                      data-sub-index={index}
                      disabled={disabled}
                      tabIndex={-1}
                      onMouseDown={(event) => event.preventDefault()}
                      onMouseMove={() => onHoverSubAction(index)}
                      onClick={() => onPickSubAction(action)}
                      className={cn(
                        "flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left transition-colors",
                        action.nested && "pl-4",
                        highlighted ? "bg-accent text-foreground" : "hover:bg-accent/70",
                        disabled && "pointer-events-none opacity-50",
                      )}
                    >
                      <span className="min-w-0 flex-1">
                        <span className="flex items-center gap-1.5">
                          {action.statusTone && <StatusDot tone={action.statusTone} />}
                          <span
                            className={cn(
                              "truncate text-xs font-medium text-foreground",
                              action.mono && "font-mono",
                            )}
                          >
                            {action.label}
                          </span>
                          {action.nextParam && (
                            <span className="inline-flex shrink-0 items-center gap-1 text-2xs text-text-tertiary">
                              <span className="font-mono">{action.nextParam.name}</span>
                              {action.nextParam.required && (
                                <Badge variant="brand" className="px-1 py-0 font-normal">
                                  {t("cmd.required")}
                                </Badge>
                              )}
                            </span>
                          )}
                        </span>
                        <span className="mt-0.5 block text-2xs leading-4 text-text-tertiary">
                          {action.description}
                        </span>
                      </span>
                      {action.current && (
                        <CheckIcon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
                      )}
                    </button>
                  );
                })}
              </div>
            </div>
          ))}
        </div>
      )}

      {textParams && (
        <div className="border-t border-border-subtle pt-1">
          <div className="px-2 pt-1 text-2xs font-medium text-text-tertiary">{t("cmd.params")}</div>
          {textParams.map((param) => (
            <ParamSpecCard key={param.name} param={param} />
          ))}
        </div>
      )}

      {legacyUsage && (
        <div className="border-t border-border-subtle px-2 pt-2 pb-1.5 space-y-1">
          <div className="flex items-center justify-between gap-2">
            <span className="text-2xs font-medium text-text-tertiary">{t("cmd.params")}</span>
            <RequirementBadge required={legacyUsage.startsWith("<")} />
          </div>
          <p className="rounded-md bg-[var(--inline-code-bg)] px-2 py-1 font-mono text-2xs leading-relaxed text-muted-foreground [overflow-wrap:anywhere]">
            {legacyUsage}
          </p>
        </div>
      )}
    </div>
  );
}

export function CommandMenu({
  id,
  side,
  entries,
  active,
  focusPane,
  subActive,
  ctx,
  onPickCommand,
  onHoverCommand,
  onPickSubAction,
  onHoverSubAction,
  inputRef,
}: {
  id: string;
  side: "bottom" | "top";
  entries: CommandEntry[];
  active: number;
  focusPane: "list" | "sub";
  subActive: number;
  ctx: CommandContext;
  onPickCommand: (index: number) => void;
  onHoverCommand: (index: number) => void;
  onPickSubAction: (action: SubmenuAction) => void;
  onHoverSubAction: (index: number) => void;
  inputRef: RefObject<HTMLElement | null>;
}) {
  const t = useT();
  const listRef = useRef<HTMLDivElement>(null);
  const detailRef = useRef<HTMLElement>(null);
  const [menuEl, setMenuEl] = useState<HTMLDivElement | null>(null);
  const [activeRowEl, setActiveRowEl] = useState<HTMLButtonElement | null>(null);
  const [scrollTick, setScrollTick] = useState(0);

  const safeActive = Math.min(Math.max(active, 0), Math.max(entries.length - 1, 0));
  const current = entries[safeActive];
  const { placed, actualSide, dockSide, anchorKey } = useSuggestionLayout(menuEl, side);

  const submenu = useMemo(
    () => (current ? buildSubmenuItems(current.command, ctx) : { sections: [], flat: [] }),
    [current, ctx],
  );

  useEffect(() => {
    listRef.current
      ?.querySelector(`[data-index="${safeActive}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [safeActive]);

  useEffect(() => {
    if (focusPane !== "sub" || subActive < 0) return;
    detailRef.current
      ?.querySelector(`[data-sub-index="${subActive}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [focusPane, subActive]);

  // 锚定到当前高亮行（横向对齐主浮层边界、纵向跟随高亮行且钳在主浮层范围内），消除视觉割裂。
  // biome-ignore lint/correctness/useExhaustiveDependencies: scrollTick 与 safeActive 触发重新定位
  const anchorRef = useMemo(
    () =>
      menuEl && anchorKey
        ? {
            current: {
              getBoundingClientRect: () => {
                const menuRect = menuEl.getBoundingClientRect();
                const rowRect = activeRowEl?.getBoundingClientRect();
                if (!rowRect || rowRect.height <= 0) {
                  return new DOMRect(
                    menuRect.left,
                    actualSide === "top"
                      ? Math.max(menuRect.top, menuRect.bottom - 30)
                      : menuRect.top,
                    menuRect.width,
                    30,
                  );
                }
                const maxTop = Math.max(menuRect.top, menuRect.bottom - rowRect.height);
                const top = Math.min(Math.max(rowRect.top, menuRect.top), maxTop);
                return new DOMRect(menuRect.left, top, menuRect.width, rowRect.height);
              },
              contextElement: activeRowEl ?? menuEl,
            },
          }
        : undefined,
    [menuEl, activeRowEl, anchorKey, actualSide, safeActive, scrollTick],
  );

  const showSideDetail = Boolean(placed && dockSide && current && anchorRef);
  const showInlineDetail = Boolean(placed && !dockSide && current);

  const inlineDetail = showInlineDetail && current && (
    <section
      ref={detailRef}
      aria-label={`/${current.command.name}`}
      data-slot="command-detail"
      data-placement={actualSide === "top" ? "inline-top" : "inline-bottom"}
      className={cn(
        "max-h-48 shrink-0 overflow-y-auto px-0.5",
        actualSide === "top"
          ? "mb-1 border-b border-border-subtle pb-1.5"
          : "mt-1 border-t border-border-subtle pt-1.5",
      )}
    >
      <CommandDetailBody
        entry={current}
        sections={submenu.sections}
        flatActions={submenu.flat}
        focusPane={focusPane}
        subActive={subActive}
        onPickSubAction={onPickSubAction}
        onHoverSubAction={onHoverSubAction}
      />
    </section>
  );

  return (
    <PopoverContent
      ref={setMenuEl}
      side={side}
      align="start"
      sideOffset={4}
      alignOffset={8}
      collisionPadding={12}
      role="presentation"
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onInteractOutside={(event) => {
        const target = event.target as Node | null;
        if (inputRef.current?.contains(target) || detailRef.current?.contains(target)) {
          event.preventDefault();
        }
      }}
      className="flex max-h-[var(--radix-popover-content-available-height)] w-[22rem] max-w-[var(--radix-popover-content-available-width)] flex-col p-1.5"
    >
      {actualSide === "top" && inlineDetail}
      <div className="px-2 py-1 text-2xs text-text-tertiary">{t("cb.cmds")}</div>
      <div
        ref={listRef}
        id={id}
        onScroll={() => setScrollTick((n) => n + 1)}
        className="min-h-0 max-h-64 overflow-y-auto"
        role="listbox"
        aria-label={t("cb.cmds")}
      >
        {entries.map((entry, index) => {
          const isActive = index === safeActive;
          const disabled = entry.disabledReason !== null;
          const trailing = commandTrailing(entry.command, ctx);
          return (
            <div key={entry.command.name}>
              {entry.showGroupHeading && (
                <div
                  className={cn(
                    "px-2 pb-1 text-2xs font-medium text-text-tertiary",
                    index === 0 ? "pt-0.5" : "mt-1 border-t border-border-subtle pt-2",
                  )}
                >
                  {entry.groupLabel}
                </div>
              )}
              <button
                ref={isActive ? setActiveRowEl : undefined}
                type="button"
                role="option"
                id={`${id}-${index}`}
                aria-selected={isActive}
                aria-disabled={disabled || undefined}
                data-index={index}
                tabIndex={-1}
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => onPickCommand(index)}
                onMouseMove={() => onHoverCommand(index)}
                className={cn(
                  "flex h-[30px] w-full items-center gap-2 rounded-md px-2 text-left text-ui transition-colors",
                  isActive
                    ? focusPane === "sub"
                      ? "bg-accent/70 text-foreground"
                      : "bg-accent text-foreground"
                    : "text-popover-foreground",
                  disabled && "opacity-50",
                )}
              >
                <span data-slot="command-title" className="min-w-0 truncate">
                  <Highlighted text={`/${entry.command.name}`} matches={entry.titleMatches} />
                </span>
                {trailing?.kind === "submenu" && (
                  <span className="ml-auto flex shrink-0 items-center gap-1 text-text-tertiary">
                    {trailing.currentValue && (
                      <span className="font-mono text-2xs">{trailing.currentValue}</span>
                    )}
                    <ChevronRightIcon className="size-3.5" />
                  </span>
                )}
                {trailing?.kind === "param" && (
                  <span className="ml-auto shrink-0">
                    {trailing.required ? (
                      <Badge variant="brand" className="font-normal">
                        {t("cmd.required")}
                      </Badge>
                    ) : (
                      <span className="text-2xs text-text-tertiary">{t("cmd.optional")}</span>
                    )}
                  </span>
                )}
              </button>
            </div>
          );
        })}
      </div>
      {actualSide === "bottom" && inlineDetail}

      {/* 锚定在高亮行旁的独立二级面板：承载交互子菜单与结构化参数说明。 */}
      <Popover open={showSideDetail}>
        {anchorRef && <PopoverAnchor virtualRef={anchorRef} />}
        {current && (
          <PopoverContent
            ref={(el) => {
              detailRef.current = el;
            }}
            role="region"
            aria-label={`/${current.command.name}`}
            data-slot="command-detail"
            data-placement="side"
            side="right"
            align="start"
            sideOffset={8}
            alignOffset={-6}
            collisionPadding={12}
            onOpenAutoFocus={(event) => event.preventDefault()}
            onCloseAutoFocus={(event) => event.preventDefault()}
            onInteractOutside={(event) => event.preventDefault()}
            onEscapeKeyDown={(event) => event.preventDefault()}
            className="max-h-[min(24rem,var(--radix-popover-content-available-height))] w-80 max-w-[var(--radix-popover-content-available-width)] overflow-y-auto p-1.5"
          >
            <CommandDetailBody
              entry={current}
              sections={submenu.sections}
              flatActions={submenu.flat}
              focusPane={focusPane}
              subActive={subActive}
              onPickSubAction={onPickSubAction}
              onHoverSubAction={onHoverSubAction}
            />
          </PopoverContent>
        )}
      </Popover>
    </PopoverContent>
  );
}

/** 阶段二：子命令 / 枚举 / 动态参数候选浮层。 */
export function CommandArgMenu({
  id,
  side,
  state,
  active,
  onPick,
  onHover,
  inputRef,
}: {
  id: string;
  side: "bottom" | "top";
  state: Extract<Stage2State, { kind: "choices" }>;
  active: number;
  onPick: (index: number) => void;
  onHover: (index: number) => void;
  inputRef: RefObject<HTMLElement | null>;
}) {
  const t = useT();
  const listRef = useRef<HTMLDivElement>(null);
  const safeActive = Math.min(Math.max(active, 0), Math.max(state.items.length - 1, 0));

  useEffect(() => {
    listRef.current
      ?.querySelector(`[data-index="${safeActive}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [safeActive]);

  return (
    <PopoverContent
      side={side}
      align="start"
      sideOffset={4}
      alignOffset={8}
      collisionPadding={12}
      role="presentation"
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onInteractOutside={(event) => {
        if (inputRef.current?.contains(event.target as Node | null)) event.preventDefault();
      }}
      className="flex max-h-[var(--radix-popover-content-available-height)] w-[25rem] max-w-[var(--radix-popover-content-available-width)] flex-col p-1.5"
    >
      <div className="flex items-center justify-between gap-2 px-2 py-1">
        <span className="flex min-w-0 items-baseline gap-1.5 truncate">
          <span className="font-mono text-2xs font-medium text-foreground">
            {state.commandPath}
          </span>
          <span className="font-mono text-2xs text-text-tertiary">{state.param.name}</span>
        </span>
        {state.acceptsText ? (
          <span className="shrink-0 text-2xs text-text-tertiary">{t("cmd.accepts_text_hint")}</span>
        ) : (
          <RequirementBadge required={state.param.required} />
        )}
      </div>

      <div
        ref={listRef}
        id={id}
        role="listbox"
        aria-label={state.commandPath}
        className="min-h-0 max-h-64 space-y-0.5 overflow-y-auto"
      >
        {state.items.map((item, index) => {
          const isActive = index === safeActive;
          return (
            <button
              key={item.key}
              type="button"
              role="option"
              id={`${id}-${index}`}
              aria-selected={isActive}
              data-index={index}
              tabIndex={-1}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onPick(index)}
              onMouseMove={() => onHover(index)}
              className={cn(
                "flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left transition-colors",
                isActive ? "bg-accent text-foreground" : "text-popover-foreground",
              )}
            >
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-1.5">
                  {item.statusTone && <StatusDot tone={item.statusTone} />}
                  <span className="truncate font-mono text-ui text-foreground">
                    <Highlighted text={item.label} matches={item.matches} />
                  </span>
                  {item.nextParam && (
                    <span className="inline-flex shrink-0 items-center gap-1 text-2xs text-text-tertiary">
                      <span className="font-mono">{item.nextParam.name}</span>
                      {item.nextParam.required && (
                        <Badge variant="brand" className="px-1 py-0 font-normal">
                          {t("cmd.required")}
                        </Badge>
                      )}
                    </span>
                  )}
                </span>
                <span className="mt-0.5 block text-xs leading-4 text-text-tertiary">
                  {item.description}
                </span>
              </span>
              {item.current && (
                <CheckIcon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
              )}
            </button>
          );
        })}
      </div>

      {state.param.defaultBehavior && (
        <div className="mt-1 border-t border-border-subtle px-2 pt-1.5 pb-0.5 text-2xs text-text-tertiary">
          <span className="font-medium text-muted-foreground">{t("cmd.default")}: </span>
          {state.param.defaultBehavior}
        </div>
      )}
    </PopoverContent>
  );
}

/** 阶段二：自由文本 / 路径参数的轻量非阻塞 Signature Hint 浮层。 */
export function CommandParamHint({
  side,
  state,
  missingError,
  inputRef,
}: {
  side: "bottom" | "top";
  state: Extract<Stage2State, { kind: "hint" }>;
  missingError: boolean;
  inputRef: RefObject<HTMLElement | null>;
}) {
  const t = useT();
  const paramName = state.param?.name ?? state.legacyUsage ?? "";
  const description =
    state.emptyDynamicNotice ?? state.param?.description ?? state.command.description;

  return (
    <PopoverContent
      side={side}
      align="start"
      sideOffset={4}
      alignOffset={8}
      collisionPadding={12}
      role="status"
      data-slot="command-param-hint"
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onInteractOutside={(event) => {
        if (inputRef.current?.contains(event.target as Node | null)) event.preventDefault();
      }}
      className="w-[24rem] max-w-[var(--radix-popover-content-available-width)] space-y-1.5 p-2.5 text-xs"
    >
      <div className="flex items-center justify-between gap-2">
        <span className="flex min-w-0 items-baseline gap-1.5 truncate">
          <span className="font-mono text-xs font-medium text-foreground">{state.commandPath}</span>
          {paramName && (
            <span className="truncate font-mono text-xs text-muted-foreground">{paramName}</span>
          )}
        </span>
        <RequirementBadge required={state.required} error={missingError} />
      </div>

      {description && (
        <p className="text-xs leading-relaxed text-muted-foreground">{description}</p>
      )}

      {state.param?.defaultBehavior && !state.emptyDynamicNotice && (
        <p className="text-2xs leading-relaxed text-text-tertiary">
          <span className="font-medium text-muted-foreground">{t("cmd.default")}: </span>
          {state.param.defaultBehavior}
        </p>
      )}

      {missingError && state.param && (
        <p className="text-2xs font-medium text-destructive">
          {t("cmd.missing_required", { name: state.param.name })}
        </p>
      )}
    </PopoverContent>
  );
}
