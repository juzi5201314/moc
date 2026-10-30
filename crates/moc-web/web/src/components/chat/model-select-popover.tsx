// 模型选择器：按供应商分组的可搜索列表 + 当前模型的思考等级。
// selector 形如 `provider/model[:level]`；规范 Fusion selector 经组合面板配置。

import {
  BrainIcon,
  CheckIcon,
  ChevronDownIcon,
  ChevronLeftIcon,
  ImageIcon,
  Settings2Icon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  compactTokens,
  fusionSelector,
  groupByProvider,
  levelLabel,
  modelName,
  parseFusion,
  parseSelector,
  reasoningLevels,
} from "@/lib/models";
import { cn } from "@/lib/utils";
import type { ModelInfo } from "@/protocol/types";
import { selectModels, useConnection } from "@/store/connection";
import { setSessionModel } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { openSettings, useUi } from "@/store/ui";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function apply(sessionId: string, selector: string): Promise<void> {
  try {
    const result = await setSessionModel(sessionId, selector);
    if (result?.compaction_error) {
      toast.error(`模型已切换，但上下文压缩失败：${result.compaction_error}`);
    } else if (result?.overflow) {
      const { tokens, context_window } = result.overflow;
      toast(
        `当前上下文 ${compactTokens(tokens)} 超出目标模型容量 ${compactTokens(context_window)}`,
        {
          description: "下一次请求前会自动压缩上下文。",
        },
      );
    }
  } catch (error) {
    toast.error(`切换模型失败：${errorText(error)}`);
  }
}

// core 的 display_name 固定为 `provider/model @默认等级`，和会话当前等级会互相矛盾，界面只用模型名。
function ModelRow({ model, selected }: { model: ModelInfo; selected: boolean }) {
  return (
    <>
      <span className="min-w-0 flex-1 truncate text-[13px] text-foreground">
        {modelName(model.id)}
      </span>
      <div className="flex shrink-0 items-center gap-1.5 text-text-tertiary">
        {reasoningLevels(model).length > 0 && (
          <BrainIcon className="size-3.5" aria-label="支持思考" />
        )}
        {model.inputs?.includes("image") && (
          <ImageIcon className="size-3.5" aria-label="支持图片" />
        )}
        {model.context_window ? (
          <span className="min-w-8 text-right font-mono text-[11px] tabular-nums">
            {compactTokens(model.context_window)}
          </span>
        ) : null}
        <CheckIcon className={cn("size-3.5 text-brand", !selected && "invisible")} />
      </div>
    </>
  );
}

type FusionDraft = { lead: string; sidekick: string };

/** selector 的短模型名（去供应商前缀与思考等级），组合展示用。 */
function shortName(selector: string, models: ModelInfo[]): string {
  return modelName(parseSelector(selector, models).id);
}

/** Fusion 组合的一侧：模型下拉（按供应商分组）+ 思考强度下拉（模型不支持时省略）。 */
function FusionSlotRow({
  label,
  value,
  models,
  onChange,
}: {
  label: string;
  value: string;
  models: ModelInfo[];
  onChange: (selector: string) => void;
}) {
  const parsed = parseSelector(value, models);
  const efforts = reasoningLevels(parsed.model);
  return (
    <div className="flex items-center gap-2">
      <span className="w-14 shrink-0 text-xs text-text-tertiary">{label}</span>
      <Select
        value={parsed.id || undefined}
        onValueChange={(id) => {
          const next = models.find((model) => model.id === id);
          const level =
            parsed.level && next?.thinking_levels?.includes(parsed.level)
              ? parsed.level
              : undefined;
          onChange(level ? `${id}:${level}` : id);
        }}
      >
        <SelectTrigger size="sm" className="h-8 min-w-0 flex-1 rounded-lg text-[13px] shadow-none">
          <SelectValue placeholder="未选择" />
        </SelectTrigger>
        <SelectContent>
          {groupByProvider(models).map(([provider, list]) => (
            <SelectGroup key={provider}>
              <SelectLabel className="text-[11px]">{provider}</SelectLabel>
              {list.map((model) => (
                <SelectItem key={model.id} value={model.id}>
                  {modelName(model.id)}
                </SelectItem>
              ))}
            </SelectGroup>
          ))}
        </SelectContent>
      </Select>
      {efforts.length > 0 && (
        <Select
          value={parsed.level === "off" ? undefined : parsed.level}
          onValueChange={(level) => onChange(`${parsed.id}:${level}`)}
        >
          <SelectTrigger
            size="sm"
            className="h-8 w-[84px] shrink-0 rounded-lg text-[13px] shadow-none"
            aria-label="思考强度"
          >
            <SelectValue placeholder="强度" />
          </SelectTrigger>
          <SelectContent>
            {efforts.map((level) => (
              <SelectItem key={level} value={level}>
                {levelLabel(level)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}
    </div>
  );
}

export function ModelPicker({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const open = useUi((state) => state.modelPicker === sessionId);
  const models = useConnection(selectModels);
  const fallback = useConnection((state) => state.config?.default_model ?? "");
  const fusionDefaults = useConnection((state) => state.config?.fusion);
  const selector = useTranscript((state) => state.bySession[sessionId]?.info.model) || fallback;
  const [view, setView] = useState<"list" | "fusion">("list");
  const [draft, setDraft] = useState<FusionDraft>({ lead: "", sidekick: "" });

  const fusion = parseFusion(selector);
  const parsed = parseSelector(selector, models);
  const levels = parsed.model?.thinking_levels ?? [];
  const label = fusion ? "Fusion" : modelName(parsed.id) || "默认模型";

  const setOpen = (next: boolean) => {
    useUi.getState().set({ modelPicker: next ? sessionId : null });
    if (!next) setView("list");
  };

  const openFusion = () => {
    // 初始草稿：当前就是 Fusion 沿用两侧；否则取 models.fusion 默认；
    // 都没有就把非 Fusion 的当前值放进 lead。
    setDraft(fusion ?? fusionDefaults ?? { lead: selector, sidekick: "" });
    setView("fusion");
  };

  const pickModel = (model: ModelInfo) => {
    setOpen(false);
    const keep = parsed.level && model.thinking_levels?.includes(parsed.level);
    const next = keep ? `${model.id}:${parsed.level}` : model.id;
    if (next !== selector) void apply(sessionId, next);
  };

  const pickLevel = (level: string) => {
    if (level === parsed.level) return;
    void apply(sessionId, `${parsed.id}:${level}`);
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          className="inline-flex h-8 min-w-0 items-center gap-1.5 rounded-lg px-2.5 text-[13px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-50 data-[state=open]:bg-accent data-[state=open]:text-foreground"
          aria-label="选择模型"
        >
          <span className="max-w-[180px] truncate">{label}</span>
          {fusion ? (
            <span className="max-w-[140px] truncate text-text-tertiary">
              · {shortName(fusion.lead, models)} + {shortName(fusion.sidekick, models)}
            </span>
          ) : (
            parsed.level &&
            parsed.level !== "off" && (
              <span className="text-text-tertiary">· {levelLabel(parsed.level)}</span>
            )
          )}
          <ChevronDownIcon className="size-3.5 opacity-60" />
        </button>
      </PopoverTrigger>

      <PopoverContent align="end" side="top" sideOffset={8} className="w-80 p-0">
        {view === "fusion" ? (
          <div className="p-2">
            <button
              type="button"
              onClick={() => setView("list")}
              className="flex h-7 items-center gap-1 rounded-md px-1.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            >
              <ChevronLeftIcon className="size-3.5" />
              返回
            </button>
            <div className="px-1.5 pb-2 pt-1">
              <div className="text-[13px] font-medium text-foreground">Fusion</div>
              <div className="mt-0.5 text-[11px] text-text-tertiary">
                前沿模型负责规划、调查与评审，高性价比模型执行实现与验证
              </div>
            </div>
            <div className="flex flex-col gap-2 px-1.5 pb-2">
              <FusionSlotRow
                label="Lead"
                value={draft.lead}
                models={models}
                onChange={(selector) => setDraft((prev) => ({ ...prev, lead: selector }))}
              />
              <FusionSlotRow
                label="Sidekick"
                value={draft.sidekick}
                models={models}
                onChange={(selector) => setDraft((prev) => ({ ...prev, sidekick: selector }))}
              />
            </div>
            <div className="border-t border-border-subtle p-1.5">
              <Button
                size="sm"
                className="w-full"
                disabled={
                  !draft.lead ||
                  !draft.sidekick ||
                  fusionSelector(draft.lead, draft.sidekick) === selector
                }
                onClick={() => {
                  void apply(sessionId, fusionSelector(draft.lead, draft.sidekick));
                  setOpen(false);
                }}
              >
                应用
              </Button>
            </div>
          </div>
        ) : (
          <>
            <Command
              loop
              filter={(value, search) =>
                value.toLowerCase().includes(search.toLowerCase()) ? 1 : 0
              }
            >
              <CommandInput placeholder="搜索模型" className="h-10 text-[13px]" />
              <CommandList className="max-h-72 p-1">
                <CommandEmpty className="py-8 text-center text-xs text-text-tertiary">
                  {models.length === 0 ? "还没有配置模型" : "没有匹配的模型"}
                </CommandEmpty>
                <CommandGroup heading="组合" className="p-0 pb-1">
                  <CommandItem
                    value="fusion lead sidekick 组合"
                    onSelect={openFusion}
                    className="gap-3 rounded-lg px-2 py-1.5"
                  >
                    <div className="flex min-w-0 flex-1 flex-col">
                      <span className="truncate text-[13px] text-foreground">Fusion</span>
                      <span className="truncate text-[11px] text-text-tertiary">
                        前沿模型主导 · 高性价比执行
                      </span>
                    </div>
                    <CheckIcon className={cn("size-3.5 text-brand", !fusion && "invisible")} />
                  </CommandItem>
                </CommandGroup>
                {groupByProvider(models).map(([provider, list]) => (
                  <CommandGroup key={provider} heading={provider} className="p-0 pb-1">
                    {list.map((model) => (
                      <CommandItem
                        key={model.id}
                        value={model.id}
                        onSelect={() => pickModel(model)}
                        className="gap-3 rounded-lg px-2 py-1.5"
                      >
                        <ModelRow model={model} selected={model.id === parsed.id} />
                      </CommandItem>
                    ))}
                  </CommandGroup>
                ))}
              </CommandList>
            </Command>

            {!fusion && levels.length > 0 && (
              <div className="border-t border-border-subtle px-3 py-2.5">
                <div className="mb-2 text-[11px] font-medium text-text-tertiary">思考强度</div>
                <div className="flex flex-wrap gap-1">
                  {levels.map((level) => (
                    <button
                      key={level}
                      type="button"
                      onClick={() => pickLevel(level)}
                      className={cn(
                        "h-6 rounded-md px-2 text-xs transition-colors",
                        (parsed.level ?? "") === level
                          ? "bg-foreground text-background"
                          : "bg-muted text-muted-foreground hover:text-foreground",
                      )}
                    >
                      {levelLabel(level)}
                    </button>
                  ))}
                </div>
              </div>
            )}

            <button
              type="button"
              onClick={() => {
                setOpen(false);
                openSettings("providers");
              }}
              className="flex h-9 w-full items-center gap-2 border-t border-border-subtle px-3 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            >
              <Settings2Icon className="size-3.5" />
              管理供应商与模型
            </button>
          </>
        )}
      </PopoverContent>
    </Popover>
  );
}
