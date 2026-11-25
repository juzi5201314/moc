// 命令胶囊参数编辑浮层：点击输入框内带非自然语言参数的命令胶囊时弹出，
// 按参数形态自适应为枚举/子命令选择列表（支持多级展开与 MCP 状态）或紧凑标量输入框（数值/路径/token）。

import { CheckIcon, XIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverAnchor, PopoverContent } from "@/components/ui/popover";
import {
  applyCommandChipEdit,
  type CommandChipArg,
  type CommandContext,
  choiceHasChipSubparams,
  isValidNumberToken,
  type ParsedCommandChip,
  type ResolvedChoice,
  resolveParamChoices,
} from "@/lib/command-params";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { RequirementBadge, StatusDot } from "./command-menu";

function isChoiceSlot(slot: CommandChipArg, choices: ResolvedChoice[]): boolean {
  if (slot.param.kind === "choice") return true;
  if (slot.param.kind === "dynamic") return choices.length > 0;
  return false;
}

function isChoiceSelected(slot: CommandChipArg, choiceValue: string): boolean {
  if (!slot.value) return false;
  const normalized =
    slot.param.dynamic === "sandbox_mode" && slot.value.toLowerCase() === "workspace-only"
      ? "workspace_only"
      : slot.value;
  return normalized.toLowerCase() === choiceValue.toLowerCase();
}

export function CommandChipEditor({
  open,
  side,
  parsed,
  text,
  ctx,
  getAnchorElement,
  onApply,
  onClose,
}: {
  open: boolean;
  side: "top" | "bottom";
  parsed: ParsedCommandChip;
  text: string;
  ctx: CommandContext;
  /** 返回当前命令胶囊 DOM（或降级容器），避免参数变更时锚点失联。 */
  getAnchorElement: () => HTMLElement | null;
  onApply: (nextText: string, caret: number, close: boolean, expectsText: boolean) => void;
  onClose: (restoreFocus: boolean) => void;
}) {
  const t = useT();
  const firstInputRef = useRef<HTMLInputElement>(null);
  const getAnchorRef = useRef(getAnchorElement);
  getAnchorRef.current = getAnchorElement;

  const slotDescriptors = useMemo(() => {
    const seen = new Map<string, number>();
    return parsed.editableSlots.map((slot) => {
      const choices =
        slot.param.kind === "choice" || slot.param.kind === "dynamic"
          ? resolveParamChoices(parsed.command, slot.param, ctx)
          : [];
      const baseKey = `${slot.param.name}:${slot.param.kind}`;
      const count = seen.get(baseKey) ?? 0;
      seen.set(baseKey, count + 1);
      return {
        key: count === 0 ? baseKey : `${baseKey}#${count}`,
        slot,
        choices,
        asChoices: isChoiceSlot(slot, choices),
      };
    });
  }, [parsed.command, parsed.editableSlots, ctx]);

  const hasScalarInputs = slotDescriptors.some((entry) => !entry.asChoices);
  const slotSignature = parsed.editableSlots
    .map((slot) => `${slot.param.name}:${slot.param.kind}:${slot.value}`)
    .join("|");

  const [drafts, setDrafts] = useState<string[]>(() =>
    parsed.editableSlots.map((slot) => slot.value),
  );
  const [errors, setErrors] = useState<Record<number, string>>({});

  // 打开浮层或外部槽位变更时同步标量草稿并清掉旧错误。
  // biome-ignore lint/correctness/useExhaustiveDependencies: slotSignature 是 editableSlots 的内容指纹
  useEffect(() => {
    if (!open) return;
    setDrafts(parsed.editableSlots.map((slot) => slot.value));
    setErrors({});
  }, [open, slotSignature]);

  // 含标量输入时自动聚焦并全选首个输入框，方便直接覆盖数字或路径后回车确认。
  useEffect(() => {
    if (!open || !hasScalarInputs) return;
    const frame = requestAnimationFrame(() => {
      const input = firstInputRef.current;
      if (!input) return;
      input.focus();
      input.select();
    });
    return () => cancelAnimationFrame(frame);
  }, [open, hasScalarInputs]);

  const anchorRef = useMemo(
    () => ({
      current: {
        getBoundingClientRect: () =>
          getAnchorRef.current()?.getBoundingClientRect() ?? new DOMRect(0, 0, 0, 0),
        get contextElement() {
          return getAnchorRef.current() ?? undefined;
        },
      },
    }),
    [],
  );

  const pickChoice = (slotIndex: number, value: string, selectedChoice?: ResolvedChoice) => {
    const descriptor = slotDescriptors[slotIndex];
    if (!descriptor) return;
    const isBranching = Boolean(
      descriptor.slot.matchedChoice?.params?.length || selectedChoice?.params?.length,
    );
    const nextValues = slotDescriptors.map((entry, idx) => {
      if (idx === slotIndex) return value;
      if (idx < slotIndex)
        return entry.asChoices ? entry.slot.value : (drafts[idx] ?? entry.slot.value);
      return isBranching
        ? ""
        : entry.asChoices
          ? entry.slot.value
          : (drafts[idx] ?? entry.slot.value);
    });
    const trimmedValues = isBranching ? nextValues.slice(0, slotIndex + 1) : nextValues;
    const result = applyCommandChipEdit(parsed, text, trimmedValues, ctx);
    const keepOpen = choiceHasChipSubparams(selectedChoice) || hasScalarInputs;
    onApply(result.text, result.caret, !keepOpen, result.expectsText);
  };

  /** 校验并写回全部标量输入槽；`strict=true` 用于回车/确认按钮，`strict=false` 用于点击外部收起。 */
  const commitScalarEdits = (strict: boolean) => {
    if (!hasScalarInputs) {
      if (strict) onClose(true);
      return;
    }
    const nextErrors: Record<number, string> = {};
    const nextValues: string[] = [];

    for (let i = 0; i < slotDescriptors.length; i += 1) {
      const { slot, asChoices } = slotDescriptors[i];
      if (asChoices) {
        nextValues.push(slot.value);
        continue;
      }
      const raw = (drafts[i] ?? "").trim();
      if (slot.param.required && raw === "") {
        nextErrors[i] = t("cmd.missing_required", { name: slot.param.name });
      } else if (slot.param.kind === "number" && raw !== "" && !isValidNumberToken(raw)) {
        nextErrors[i] = t("cmd.invalid_number");
      }
      nextValues.push(raw);
    }

    if (Object.keys(nextErrors).length > 0) {
      if (strict) setErrors(nextErrors);
      return;
    }

    const result = applyCommandChipEdit(parsed, text, nextValues, ctx);
    if (result.text !== text) {
      onApply(result.text, result.caret, strict, result.expectsText);
    } else if (strict) {
      onClose(true);
    }
  };

  let scalarCursor = 0;

  return (
    <Popover
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onClose(false);
      }}
    >
      <PopoverAnchor virtualRef={anchorRef} />
      <PopoverContent
        side={side}
        align="start"
        sideOffset={6}
        collisionPadding={12}
        role="dialog"
        aria-label={t("cmd.edit_params")}
        data-slot="command-chip-editor"
        onOpenAutoFocus={(event) => {
          if (hasScalarInputs) event.preventDefault();
        }}
        onCloseAutoFocus={(event) => event.preventDefault()}
        onEscapeKeyDown={(event) => {
          event.preventDefault();
          onClose(true);
        }}
        onInteractOutside={(event) => {
          const target = event.target as Node | null;
          if (target && getAnchorElement()?.contains(target)) {
            event.preventDefault();
            return;
          }
          commitScalarEdits(false);
        }}
        className="w-80 max-w-[var(--radix-popover-content-available-width)] p-1.5"
      >
        <div className="px-2 pt-1 pb-1.5">
          <div className="flex items-baseline gap-2">
            <span className="font-mono text-xs font-medium text-foreground">
              /{parsed.typedName}
            </span>
            {parsed.command.plugin && (
              <span className="ml-auto text-2xs text-text-tertiary">
                {t("cmd.plugin_source", { name: parsed.command.plugin })}
              </span>
            )}
          </div>
          {parsed.command.description && (
            <p className="mt-0.5 text-2xs leading-4 text-text-tertiary">
              {parsed.command.description}
            </p>
          )}
        </div>

        <div className="max-h-72 space-y-1.5 overflow-y-auto">
          {slotDescriptors.map(({ key, slot, choices, asChoices }, slotIndex) => {
            if (asChoices) {
              const showDefaultOption = !slot.param.required;
              const isDefaultActive = slot.value === "";
              return (
                <div key={key} className="border-t border-border-subtle pt-1">
                  <div className="flex items-center justify-between gap-2 px-2 py-1">
                    <span className="font-mono text-2xs font-medium text-text-tertiary">
                      {slot.param.name}
                    </span>
                    <RequirementBadge required={slot.param.required} />
                  </div>
                  <div className="space-y-0.5" role="menu" aria-label={slot.param.name}>
                    {showDefaultOption && (
                      <button
                        type="button"
                        role="menuitemradio"
                        aria-checked={isDefaultActive}
                        onClick={() => pickChoice(slotIndex, "")}
                        className={cn(
                          "flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left transition-colors",
                          isDefaultActive ? "bg-accent text-foreground" : "hover:bg-accent/70",
                        )}
                      >
                        <span className="min-w-0 flex-1">
                          <span className="block text-xs font-medium text-foreground">
                            {slot.param.defaultBehavior
                              ? t("cmd.default_action")
                              : t("cmd.keep_default")}
                          </span>
                          {slot.param.defaultBehavior && (
                            <span className="mt-0.5 block text-2xs leading-4 text-text-tertiary">
                              {slot.param.defaultBehavior}
                            </span>
                          )}
                        </span>
                        {isDefaultActive && (
                          <CheckIcon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
                        )}
                      </button>
                    )}

                    {choices.map((choice) => {
                      const selected = isChoiceSelected(slot, choice.value);
                      const subParam = choice.params?.[0];
                      return (
                        <button
                          key={choice.value}
                          type="button"
                          role="menuitemradio"
                          aria-checked={selected}
                          onClick={() => pickChoice(slotIndex, choice.value, choice)}
                          className={cn(
                            "flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left transition-colors",
                            selected ? "bg-accent text-foreground" : "hover:bg-accent/70",
                          )}
                        >
                          <span className="min-w-0 flex-1">
                            <span className="flex items-center gap-1.5">
                              {choice.statusTone && <StatusDot tone={choice.statusTone} />}
                              <span className="truncate font-mono text-xs font-medium text-foreground">
                                {choice.label}
                              </span>
                              {subParam && (
                                <span className="inline-flex shrink-0 items-center gap-1 text-2xs text-text-tertiary">
                                  <span className="font-mono">{subParam.name}</span>
                                  {subParam.required && (
                                    <Badge variant="brand" className="px-1 py-0 font-normal">
                                      {t("cmd.required")}
                                    </Badge>
                                  )}
                                </span>
                              )}
                            </span>
                            {choice.description && (
                              <span className="mt-0.5 block text-2xs leading-4 text-text-tertiary">
                                {choice.description}
                              </span>
                            )}
                          </span>
                          {selected && (
                            <CheckIcon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
                          )}
                        </button>
                      );
                    })}
                  </div>
                </div>
              );
            }

            const isFirstScalar = scalarCursor === 0;
            scalarCursor += 1;
            const value = drafts[slotIndex] ?? "";
            const error = errors[slotIndex];
            const emptyMcp = slot.param.dynamic === "mcp_server" && choices.length === 0;

            return (
              <div key={key} className="space-y-1.5 border-t border-border-subtle px-2 pt-2 pb-1">
                <div className="flex items-center justify-between gap-2">
                  <label
                    htmlFor={`chip-param-${slotIndex}`}
                    className="font-mono text-xs font-medium text-foreground"
                  >
                    {slot.param.name}
                  </label>
                  <RequirementBadge required={slot.param.required} error={Boolean(error)} />
                </div>
                <div className="relative flex items-center">
                  <Input
                    ref={isFirstScalar ? firstInputRef : undefined}
                    id={`chip-param-${slotIndex}`}
                    type="text"
                    inputMode={slot.param.kind === "number" ? "decimal" : undefined}
                    value={value}
                    placeholder={slot.param.placeholder ?? slot.param.name}
                    aria-invalid={Boolean(error) || undefined}
                    onChange={(event) => {
                      const next = event.target.value;
                      setDrafts((current) => {
                        const copy = [...current];
                        copy[slotIndex] = next;
                        return copy;
                      });
                      if (errors[slotIndex]) {
                        setErrors((current) => {
                          const copy = { ...current };
                          delete copy[slotIndex];
                          return copy;
                        });
                      }
                    }}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" && !event.nativeEvent.isComposing) {
                        event.preventDefault();
                        commitScalarEdits(true);
                      }
                    }}
                    className={cn(
                      "h-8 border-border bg-muted/40 px-2.5 font-mono text-xs shadow-none focus-visible:border-border-strong focus-visible:ring-0",
                      !slot.param.required && value.length > 0 && "pr-7",
                    )}
                  />
                  {!slot.param.required && value.length > 0 && (
                    <button
                      type="button"
                      aria-label={t("common.clear")}
                      onClick={() => {
                        setDrafts((current) => {
                          const copy = [...current];
                          copy[slotIndex] = "";
                          return copy;
                        });
                        setErrors((current) => {
                          const copy = { ...current };
                          delete copy[slotIndex];
                          return copy;
                        });
                      }}
                      className="absolute right-1.5 flex size-5 items-center justify-center rounded text-text-tertiary transition-colors hover:text-foreground"
                    >
                      <XIcon className="size-3.5" />
                    </button>
                  )}
                </div>
                {error ? (
                  <p className="text-2xs font-medium text-destructive">{error}</p>
                ) : emptyMcp ? (
                  <p className="text-2xs leading-4 text-text-tertiary">{t("cmd.mcp_no_servers")}</p>
                ) : (
                  <>
                    {slot.param.description && (
                      <p className="text-2xs leading-4 text-text-tertiary">
                        {slot.param.description}
                      </p>
                    )}
                    {slot.param.defaultBehavior && (
                      <p className="text-2xs leading-4 text-text-tertiary">
                        <span className="font-medium text-muted-foreground">
                          {t("cmd.default")}:{" "}
                        </span>
                        {slot.param.defaultBehavior}
                      </p>
                    )}
                  </>
                )}
              </div>
            );
          })}
        </div>

        {hasScalarInputs && (
          <div className="mt-1.5 flex items-center justify-end gap-1.5 border-t border-border-subtle px-2 pt-2 pb-0.5">
            <Button
              type="button"
              size="sm"
              className="h-7 rounded-md px-2.5 text-xs"
              onClick={() => commitScalarEdits(true)}
            >
              {t("cmd.apply")}
            </Button>
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}
