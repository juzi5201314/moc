// 模型选择器：左侧详情卡 + 右侧可搜索列表。列表高亮项（悬停或方向键）决定详情卡内容；
// 详情卡里改思考强度会直接切到该模型，Fast Mode 是会话级开关。
// selector 形如 `provider/model[:level]`；规范 Fusion selector 经 Fusion 卡配置。

import { CheckIcon, Settings2Icon } from "lucide-react";
import { type ReactNode, useState } from "react";
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
import { Switch } from "@/components/ui/switch";
import {
  compactTokens,
  fusionSelector,
  groupByProvider,
  modelName,
  parseFusion,
  parseSelector,
  providerOf,
  reasoningLevels,
} from "@/lib/models";
import { cn } from "@/lib/utils";
import type { ModelInfo, ModelPricing } from "@/protocol/types";
import { selectModels, useConnection } from "@/store/connection";
import { setSessionFastMode, setSessionModel } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { openSettings, useUi } from "@/store/ui";

/** 列表里 Fusion 项的 cmdk value；模型 ID 都带 `provider/` 前缀，不会撞名。 */
const FUSION_VALUE = "fusion";

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

/** 每百万 token 单价：最多三位小数并去掉尾随零，`$1`、`$0.075`、`$1.25`。 */
function price(value: number): string {
  return `$${Number(value.toFixed(3))}`;
}

/** selector 的短模型名（去供应商前缀与思考等级），组合展示用。 */
function shortName(selector: string, models: ModelInfo[]): string {
  return modelName(parseSelector(selector, models).id);
}

/** 详情卡的一行：左标签、右控件。 */
function CardRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex h-8 items-center justify-between gap-3">
      <span className="text-ui text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

/** 详情卡里的思考强度下拉：无边框、只在悬停时出底色。 */
function LevelSelect({
  value,
  levels,
  onChange,
}: {
  value: string | undefined;
  levels: string[];
  onChange: (level: string) => void;
}) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger
        size="sm"
        aria-label="思考强度"
        className="h-7 gap-1 border-0 bg-transparent px-2 text-ui text-foreground shadow-none hover:bg-accent dark:bg-transparent dark:hover:bg-accent [&_svg]:size-3.5"
      >
        <SelectValue placeholder="默认" />
      </SelectTrigger>
      <SelectContent position="popper" align="end" className="min-w-32">
        {levels.map((level) => (
          <SelectItem key={level} value={level} className="text-ui">
            {level}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function FastModeRow({
  enabled,
  onChange,
}: {
  enabled: boolean;
  onChange: (next: boolean) => void;
}) {
  return (
    <CardRow label="Fast Mode">
      <Switch checked={enabled} onCheckedChange={onChange} aria-label="Fast Mode" />
    </CardRow>
  );
}

/** 价格格：只画配置了的单价，模型没配 pricing 时整段不出现。 */
function PriceGrid({ pricing }: { pricing: ModelPricing }) {
  const cells: [string, number][] = [["输入", pricing.input]];
  if (pricing.cache_read !== undefined) cells.push(["缓存命中", pricing.cache_read]);
  if (pricing.cache_write !== undefined) cells.push(["缓存写入", pricing.cache_write]);
  cells.push(["输出", pricing.output]);
  return (
    <div
      className={cn(
        "grid gap-1 border-t border-border-subtle px-2 pt-2.5 pb-1",
        cells.length === 2 && "grid-cols-2",
        cells.length === 3 && "grid-cols-3",
        cells.length === 4 && "grid-cols-4",
      )}
    >
      {cells.map(([label, value]) => (
        <div key={label} className="rounded-lg px-2 py-1.5">
          <div className="text-2xs text-text-tertiary">{label}</div>
          <div className="mt-0.5 text-ui text-foreground tabular-nums">
            {price(value)}
            <span className="text-2xs text-text-tertiary"> / 1M</span>
          </div>
        </div>
      ))}
    </div>
  );
}

function ModelCard({
  model,
  level,
  fast,
  onLevel,
  onFast,
}: {
  model: ModelInfo;
  level: string | undefined;
  fast: boolean;
  onLevel: (level: string) => void;
  onFast: (next: boolean) => void;
}) {
  const levels = model.thinking_levels ?? [];
  const meta = [
    providerOf(model.id),
    model.context_window ? `${compactTokens(model.context_window)} 上下文` : "",
    model.max_output_tokens ? `${compactTokens(model.max_output_tokens)} 输出` : "",
    model.inputs?.includes("image") ? "支持图片" : "",
  ].filter(Boolean);
  return (
    <>
      <div className="px-4 pt-3.5 pb-2">
        <div className="truncate text-sm font-medium text-foreground">{modelName(model.id)}</div>
        <div className="mt-1 text-xs text-text-tertiary">{meta.join(" · ")}</div>
      </div>
      {(reasoningLevels(model).length > 0 || model.fast_mode) && (
        <div className="px-4 pb-2">
          {reasoningLevels(model).length > 0 && (
            <CardRow label="思考强度">
              <LevelSelect value={level} levels={levels} onChange={onLevel} />
            </CardRow>
          )}
          {model.fast_mode && <FastModeRow enabled={fast} onChange={onFast} />}
        </div>
      )}
      {model.pricing && <PriceGrid pricing={model.pricing} />}
    </>
  );
}

type FusionDraft = { lead: string; sidekick: string };

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
      <span className="w-16 shrink-0 text-ui text-muted-foreground">{label}</span>
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
        <SelectTrigger size="sm" className="h-8 min-w-0 flex-1 rounded-lg text-ui shadow-none">
          <SelectValue placeholder="未选择" />
        </SelectTrigger>
        <SelectContent>
          {groupByProvider(models).map(([provider, list]) => (
            <SelectGroup key={provider}>
              <SelectLabel className="text-2xs">{provider}</SelectLabel>
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
            className="h-8 w-[84px] shrink-0 rounded-lg text-ui shadow-none"
            aria-label="思考强度"
          >
            <SelectValue placeholder="强度" />
          </SelectTrigger>
          <SelectContent>
            {efforts.map((level) => (
              <SelectItem key={level} value={level}>
                {level}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}
    </div>
  );
}

function FusionCard({
  draft,
  models,
  current,
  fast,
  onDraft,
  onApply,
  onFast,
}: {
  draft: FusionDraft;
  models: ModelInfo[];
  current: string;
  fast: boolean;
  onDraft: (draft: FusionDraft) => void;
  onApply: () => void;
  onFast: (next: boolean) => void;
}) {
  const lead = parseSelector(draft.lead, models).model;
  const ready = Boolean(draft.lead && draft.sidekick);
  return (
    <>
      <div className="px-4 pt-3.5 pb-2">
        <div className="text-sm font-medium text-foreground">Fusion</div>
        <div className="mt-1 text-xs text-text-tertiary">
          前沿模型负责规划、调查与评审，高性价比模型执行实现与验证
        </div>
      </div>
      <div className="flex flex-col gap-2 px-4 pt-1 pb-2">
        <FusionSlotRow
          label="Lead"
          value={draft.lead}
          models={models}
          onChange={(selector) => onDraft({ ...draft, lead: selector })}
        />
        <FusionSlotRow
          label="Sidekick"
          value={draft.sidekick}
          models={models}
          onChange={(selector) => onDraft({ ...draft, sidekick: selector })}
        />
        {lead?.fast_mode && <FastModeRow enabled={fast} onChange={onFast} />}
      </div>
      <div className="px-4 pt-1 pb-3.5">
        <Button
          size="sm"
          className="w-full"
          disabled={!ready || fusionSelector(draft.lead, draft.sidekick) === current}
          onClick={onApply}
        >
          应用
        </Button>
      </div>
    </>
  );
}

/** 弹层内容：每次打开都重新挂载，草稿与高亮项随之按当前会话初始化。 */
function PickerPanel({ sessionId, onClose }: { sessionId: string; onClose: () => void }) {
  const models = useConnection(selectModels);
  const fallback = useConnection((state) => state.config?.default_model ?? "");
  const fusionDefaults = useConnection((state) => state.config?.fusion);
  const info = useTranscript((state) => state.bySession[sessionId]?.info);
  const selector = info?.model || fallback;
  const fast = Boolean(info?.fast_mode);

  const fusion = parseFusion(selector);
  const parsed = parseSelector(selector, models);
  const [active, setActive] = useState(fusion ? FUSION_VALUE : parsed.id);
  // 初始草稿：当前就是 Fusion 沿用两侧；否则取 models.fusion 默认；都没有就把当前值放进 lead。
  const [draft, setDraft] = useState<FusionDraft>(
    () => fusion ?? fusionDefaults ?? { lead: selector, sidekick: "" },
  );

  /** 该模型被选中时实际生效的思考等级：当前模型取会话等级，其余沿用能保留的等级。 */
  const levelFor = (model: ModelInfo): string | undefined => {
    if (!fusion && model.id === parsed.id) return parsed.level;
    return parsed.level && model.thinking_levels?.includes(parsed.level) ? parsed.level : undefined;
  };

  const pickModel = (model: ModelInfo) => {
    onClose();
    const level = levelFor(model);
    const next = level ? `${model.id}:${level}` : model.id;
    if (next !== selector) void apply(sessionId, next);
  };

  const pickLevel = (model: ModelInfo, level: string) => {
    const next = `${model.id}:${level}`;
    if (next !== selector) void apply(sessionId, next);
  };

  const applyFusion = () => {
    if (!draft.lead || !draft.sidekick) return;
    const next = fusionSelector(draft.lead, draft.sidekick);
    onClose();
    if (next !== selector) void apply(sessionId, next);
  };

  const toggleFast = (next: boolean) => {
    setSessionFastMode(sessionId, next).catch((error) =>
      toast.error(`切换 Fast Mode 失败：${errorText(error)}`),
    );
  };

  const activeModel =
    active === FUSION_VALUE ? undefined : models.find((model) => model.id === active);

  return (
    // DOM 顺序先列表后详情，Tab 与初始焦点落在搜索框；视觉上详情卡在左。
    <div className="flex flex-row-reverse items-start gap-2">
      <div className="w-72 overflow-hidden rounded-xl bg-popover shadow-[var(--elevation-popover)]">
        <Command
          loop
          value={active}
          onValueChange={setActive}
          filter={(value, search, keywords) => {
            const query = search.toLowerCase();
            return [value, ...(keywords ?? [])].some((text) => text.toLowerCase().includes(query))
              ? 1
              : 0;
          }}
        >
          <CommandInput placeholder="搜索模型" className="h-10 text-ui" />
          <CommandList className="max-h-80 p-1">
            <CommandEmpty className="py-8 text-center text-xs text-text-tertiary">
              {models.length === 0 ? "还没有配置模型" : "没有匹配的模型"}
            </CommandEmpty>
            <CommandGroup heading="组合" className="p-0 pb-1">
              <CommandItem
                value={FUSION_VALUE}
                keywords={["lead", "sidekick", "组合"]}
                onSelect={applyFusion}
                className="gap-3 rounded-lg px-2 py-1.5"
              >
                <span className="min-w-0 flex-1 truncate text-ui text-foreground">
                  Fusion
                  {fusion && (
                    <span className="text-text-tertiary">
                      {" "}
                      {shortName(fusion.lead, models)} + {shortName(fusion.sidekick, models)}
                    </span>
                  )}
                </span>
                <CheckIcon
                  className={cn("size-3.5 text-muted-foreground", !fusion && "invisible")}
                />
              </CommandItem>
            </CommandGroup>
            {groupByProvider(models).map(([provider, list]) => (
              <CommandGroup key={provider} heading={provider} className="p-0 pb-1">
                {list.map((model) => {
                  const selected = !fusion && model.id === parsed.id;
                  return (
                    <CommandItem
                      key={model.id}
                      value={model.id}
                      onSelect={() => pickModel(model)}
                      className="gap-3 rounded-lg px-2 py-1.5"
                    >
                      <span className="min-w-0 flex-1 truncate text-ui text-foreground">
                        {modelName(model.id)}
                        {selected && parsed.level && parsed.level !== "off" && (
                          <span className="text-text-tertiary"> {parsed.level}</span>
                        )}
                      </span>
                      <CheckIcon
                        className={cn("size-3.5 text-muted-foreground", !selected && "invisible")}
                      />
                    </CommandItem>
                  );
                })}
              </CommandGroup>
            ))}
          </CommandList>
        </Command>
        <button
          type="button"
          onClick={() => {
            onClose();
            openSettings("providers");
          }}
          className="flex h-9 w-full items-center gap-2 border-t border-border-subtle px-3 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          <Settings2Icon className="size-3.5" />
          管理供应商与模型
        </button>
      </div>

      {(active === FUSION_VALUE || activeModel) && (
        <div className="w-80 overflow-hidden rounded-xl bg-popover pb-1 shadow-[var(--elevation-popover)]">
          {activeModel ? (
            <ModelCard
              model={activeModel}
              level={levelFor(activeModel)}
              fast={fast}
              onLevel={(level) => pickLevel(activeModel, level)}
              onFast={toggleFast}
            />
          ) : (
            <FusionCard
              draft={draft}
              models={models}
              current={selector}
              fast={fast}
              onDraft={setDraft}
              onApply={applyFusion}
              onFast={toggleFast}
            />
          )}
        </div>
      )}
    </div>
  );
}

export function ModelPicker({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const open = useUi((state) => state.modelPicker === sessionId);
  const models = useConnection(selectModels);
  const fallback = useConnection((state) => state.config?.default_model ?? "");
  const selector = useTranscript((state) => state.bySession[sessionId]?.info.model) || fallback;

  const fusion = parseFusion(selector);
  const parsed = parseSelector(selector, models);
  const label = fusion ? "Fusion" : modelName(parsed.id) || "默认模型";

  const setOpen = (next: boolean) => {
    useUi.getState().set({ modelPicker: next ? sessionId : null });
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          disabled={disabled}
          className="inline-flex h-8 min-w-0 items-center gap-1.5 rounded-lg px-2.5 text-ui text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-50 data-[state=open]:bg-accent data-[state=open]:text-foreground"
          aria-label="选择模型"
        >
          <span className="max-w-[180px] truncate">{label}</span>
          {fusion ? (
            <span className="max-w-[140px] truncate text-text-tertiary">
              · {shortName(fusion.lead, models)} + {shortName(fusion.sidekick, models)}
            </span>
          ) : (
            parsed.level &&
            parsed.level !== "off" && <span className="text-text-tertiary">· {parsed.level}</span>
          )}
        </button>
      </PopoverTrigger>

      {/* 外层透明：列表与详情卡各自是一张浮层卡片。 */}
      <PopoverContent
        align="end"
        side="top"
        sideOffset={8}
        className="w-auto rounded-none bg-transparent p-0 shadow-none"
      >
        <PickerPanel sessionId={sessionId} onClose={() => setOpen(false)} />
      </PopoverContent>
    </Popover>
  );
}
