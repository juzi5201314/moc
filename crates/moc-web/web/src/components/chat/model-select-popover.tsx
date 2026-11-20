// 模型选择器：左侧详情卡 + 右侧可搜索列表。列表高亮项（悬停或方向键）决定详情卡内容；
// 详情卡里改思考强度会直接切到该模型，Fast Mode 是会话级开关。
// selector 形如 `provider/model[:level]`；规范 Fusion selector 经 Fusion 卡配置。

import { CheckIcon, ChevronDownIcon, Settings2Icon } from "lucide-react";
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
import { t, useT } from "@/lib/i18n";
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
      toast.error(t("mp.compact_failed", { error: result.compaction_error }));
    } else if (result?.overflow) {
      const { tokens, context_window } = result.overflow;
      toast(
        t("mp.overflow", {
          tokens: compactTokens(tokens),
          window: compactTokens(context_window),
        }),
        {
          description: t("mp.overflow_hint"),
        },
      );
    }
  } catch (error) {
    toast.error(t("mp.switch_failed", { error: errorText(error) }));
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
  const t = useT();
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger
        size="sm"
        aria-label={t("mp.level")}
        className="h-7 gap-1 border-0 bg-transparent px-2 text-ui text-foreground shadow-none hover:bg-accent dark:bg-transparent dark:hover:bg-accent [&_svg]:size-3.5"
      >
        <SelectValue placeholder={t("mp.default")} />
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
  const t = useT();
  const cells: [string, number][] = [[t("mp.input"), pricing.input]];
  if (pricing.cache_read != null) cells.push([t("mp.cache_read"), pricing.cache_read]);
  if (pricing.cache_write != null) cells.push([t("mp.cache_write"), pricing.cache_write]);
  cells.push([t("mp.output"), pricing.output]);
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
  /** 会话级开关；非会话目标（新会话模型）不传，行整体隐藏。 */
  onFast?: (next: boolean) => void;
}) {
  const t = useT();
  const levels = model.thinking_levels ?? [];
  const meta = [
    providerOf(model.id),
    model.context_window ? t("mp.ctx", { n: compactTokens(model.context_window) }) : "",
    model.max_output_tokens ? t("mp.out", { n: compactTokens(model.max_output_tokens) }) : "",
    model.inputs?.includes("image") ? t("mp.images") : "",
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
            <CardRow label={t("mp.level")}>
              <LevelSelect value={level} levels={levels} onChange={onLevel} />
            </CardRow>
          )}
          {model.fast_mode && onFast && <FastModeRow enabled={fast} onChange={onFast} />}
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
  const t = useT();
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
          <SelectValue placeholder={t("mp.unselected")} />
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
            aria-label={t("mp.level")}
          >
            <SelectValue placeholder={t("mp.level_short")} />
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
  onFast?: (next: boolean) => void;
}) {
  const t = useT();
  const lead = parseSelector(draft.lead, models).model;
  const ready = Boolean(draft.lead && draft.sidekick);
  return (
    <>
      <div className="px-4 pt-3.5 pb-2">
        <div className="text-sm font-medium text-foreground">Fusion</div>
        <div className="mt-1 text-xs text-text-tertiary">{t("mp.fusion_desc")}</div>
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
        {lead?.fast_mode && onFast && <FastModeRow enabled={fast} onChange={onFast} />}
      </div>
      <div className="px-4 pt-1 pb-3.5">
        <Button
          size="sm"
          className="w-full"
          disabled={!ready || fusionSelector(draft.lead, draft.sidekick) === current}
          onClick={onApply}
        >
          {t("mp.apply")}
        </Button>
      </div>
    </>
  );
}

/** 弹层内容：每次打开都重新挂载，草稿与高亮项随之按当前 selector 初始化。
 *  受控组件：`selector` 是当前值，`onPick` 收到新 selector；`sessionId` 仅
 *  会话目标需要（Fast Mode 行）。 */
function PickerPanel({
  selector,
  sessionId,
  onPick,
  onClose,
}: {
  selector: string;
  sessionId?: string;
  onPick: (next: string) => void;
  onClose: () => void;
}) {
  const t = useT();
  const models = useConnection(selectModels);
  const fusionDefaults = useConnection((state) => state.config?.fusion);
  const info = useTranscript((state) => (sessionId ? state.bySession[sessionId]?.info : undefined));
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
    if (next !== selector) onPick(next);
  };

  const pickLevel = (model: ModelInfo, level: string) => {
    const next = `${model.id}:${level}`;
    if (next !== selector) onPick(next);
  };

  const applyFusion = () => {
    if (!draft.lead || !draft.sidekick) return;
    const next = fusionSelector(draft.lead, draft.sidekick);
    onClose();
    if (next !== selector) onPick(next);
  };

  const toggleFast = (next: boolean) => {
    if (!sessionId) return;
    setSessionFastMode(sessionId, next).catch((error) =>
      toast.error(t("mp.fast_failed", { error: errorText(error) })),
    );
  };

  const activeModel =
    active === FUSION_VALUE ? undefined : models.find((model) => model.id === active);

  return (
    // DOM 顺序先列表后详情，Tab 与初始焦点落在搜索框；桌面详情卡在左，窄屏在下。
    <div className="flex w-[calc(100vw-2rem)] flex-col items-start gap-2 md:w-auto md:flex-row-reverse">
      <div className="w-full shrink-0 overflow-hidden rounded-xl bg-popover shadow-[var(--elevation-popover)] md:w-72">
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
          <CommandInput placeholder={t("mp.search")} className="h-10 text-ui" />
          <CommandList className="max-h-80 p-1">
            <CommandEmpty className="py-8 text-center text-xs text-text-tertiary">
              {models.length === 0 ? t("mp.empty") : t("mp.no_match")}
            </CommandEmpty>
            <CommandGroup heading={t("mp.group_combo")} className="p-0 pb-1">
              <CommandItem
                value={FUSION_VALUE}
                keywords={["lead", "sidekick", t("mp.fusion_kw")]}
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
          {t("mp.manage")}
        </button>
      </div>

      {(active === FUSION_VALUE || activeModel) && (
        <div className="w-full shrink-0 overflow-hidden rounded-xl bg-popover pb-1 shadow-[var(--elevation-popover)] md:w-80">
          {activeModel ? (
            <ModelCard
              model={activeModel}
              level={levelFor(activeModel)}
              fast={fast}
              onLevel={(level) => pickLevel(activeModel, level)}
              onFast={sessionId ? toggleFast : undefined}
            />
          ) : (
            <FusionCard
              draft={draft}
              models={models}
              current={selector}
              fast={fast}
              onDraft={setDraft}
              onApply={applyFusion}
              onFast={sessionId ? toggleFast : undefined}
            />
          )}
        </div>
      )}
    </div>
  );
}

export function ModelPicker({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const t = useT();
  const open = useUi((state) => state.modelPicker === sessionId);
  const models = useConnection(selectModels);
  const fallback = useConnection((state) => state.config?.default_model ?? "");
  const selector = useTranscript((state) => state.bySession[sessionId]?.info.model) || fallback;

  const fusion = parseFusion(selector);
  const parsed = parseSelector(selector, models);
  const label = fusion ? "Fusion" : modelName(parsed.id) || t("mp.default_model");

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
          aria-label={t("mp.pick")}
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
        className="max-h-[var(--radix-popover-content-available-height)] w-auto overflow-y-auto rounded-none bg-transparent p-0 shadow-none md:max-h-none md:overflow-visible"
      >
        <PickerPanel
          selector={selector}
          sessionId={sessionId}
          onPick={(next) => void apply(sessionId, next)}
          onClose={() => setOpen(false)}
        />
      </PopoverContent>
    </Popover>
  );
}

/**
 * 计划审阅「新会话模型」的受控选择器：只把 selector 交给 onChange，
 * 不写回会话。触发器用与同行「新会话执行」一致的 outline 小按钮。
 */
export function ExecutionModelPicker({
  value,
  onChange,
  disabled,
}: {
  /** 生效的 selector；未单独选择时调用方传入会话当前模型。 */
  value: string;
  onChange: (next: string) => void;
  disabled?: boolean;
}) {
  const t = useT();
  const [open, setOpen] = useState(false);
  const models = useConnection(selectModels);

  const fusion = parseFusion(value);
  const parsed = parseSelector(value, models);
  const label = fusion ? "Fusion" : modelName(parsed.id) || t("mp.default_model");

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          size="sm"
          variant="outline"
          disabled={disabled}
          title={t("pr.new_model_title")}
          className="gap-1 px-2.5 text-ui shadow-none"
        >
          <span className="max-w-[140px] truncate">{label}</span>
          <ChevronDownIcon className="opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        side="top"
        sideOffset={8}
        className="w-auto rounded-none bg-transparent p-0 shadow-none"
      >
        <PickerPanel selector={value} onPick={onChange} onClose={() => setOpen(false)} />
      </PopoverContent>
    </Popover>
  );
}
