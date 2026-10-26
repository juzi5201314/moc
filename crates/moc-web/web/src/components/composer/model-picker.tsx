// 模型选择器：可搜索的模型列表 + 思考等级。selector 为 `{id}:{level}`，
// 调 `session/set_model {overflow:"ask"}`；返回 overflow 时弹窗让用户选 switch / compact / 取消。

import { Check, ChevronDown } from "lucide-react";
import { useState } from "react";
import { runAction } from "@/components/layout/session-actions";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
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
import { formatTokens } from "@/lib/format";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type { ModelInfo, ModelSwitchOverflow, SessionSetModelResult } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { buildSelector, parseSelector } from "./parse";

// 选择器里不能返回新的 `[]`：zustand 按引用比较，会导致无限重渲染。
const NO_MODELS: ModelInfo[] = [];

function priceLabel(model: ModelInfo): string | null {
  if (!model.pricing) return null;
  return `$${model.pricing.input}/${model.pricing.output}`;
}

export function useCurrentModel(sessionId: string): {
  model: ModelInfo | undefined;
  id: string;
  level: string | null;
} {
  const raw = useTranscript((state) => state.bySession[sessionId]?.info.model ?? "");
  const models = useConnection((state) => state.config?.models) ?? NO_MODELS;
  const parsed = parseSelector(raw, (id) => models.find((m) => m.id === id)?.thinking_levels);
  return { ...parsed, model: models.find((m) => m.id === parsed.id) };
}

/** 调 set_model；overflow 由调用方决定如何处理。 */
async function setModel(
  sessionId: string,
  selector: string,
  overflow: "ask" | "switch" | "compact",
): Promise<SessionSetModelResult> {
  const result = await rpc.call<SessionSetModelResult>("session/set_model", {
    session_id: sessionId,
    selector,
    overflow,
  });
  if (!result.overflow || overflow !== "ask") {
    useTranscript.getState().mutate(sessionId, (view) => {
      view.info = result.session;
    });
    useSessions.getState().upsert(result.session);
  }
  return result;
}

export function ModelPicker({ sessionId, disabled }: { sessionId: string; disabled?: boolean }) {
  const models = useConnection((state) => state.config?.models) ?? NO_MODELS;
  const current = useCurrentModel(sessionId);
  const openFor = useUi((state) => state.modelPicker);
  const setUi = useUi((state) => state.set);
  const open = openFor === sessionId;
  const [pendingOverflow, setPendingOverflow] = useState<{
    selector: string;
    overflow: ModelSwitchOverflow;
  } | null>(null);
  const [focusId, setFocusId] = useState<string | null>(null);

  const setOpen = (value: boolean) => setUi({ modelPicker: value ? sessionId : null });

  const choose = (selector: string) => {
    setOpen(false);
    void runAction(async () => {
      const result = await setModel(sessionId, selector, "ask");
      if (result.overflow) setPendingOverflow({ selector, overflow: result.overflow });
    });
  };

  const resolveOverflow = (action: "switch" | "compact") => {
    const target = pendingOverflow;
    setPendingOverflow(null);
    if (!target) return;
    void runAction(async () => {
      const result = await setModel(sessionId, target.selector, action);
      if (result.compaction_error) throw new Error(`压缩失败：${result.compaction_error}`);
    }, "已切换模型");
  };

  const focused = models.find((m) => m.id === (focusId ?? current.id));
  const levels = focused?.thinking_levels ?? [];
  const label = current.model?.display_name ?? (current.id || "选择模型");

  return (
    <>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button
            variant="ghost"
            size="xs"
            disabled={disabled}
            aria-label="选择模型"
            className="max-w-56 text-muted-foreground"
          >
            <span className="truncate">{label}</span>
            {current.level ? <span className="opacity-70">· {current.level}</span> : null}
            <ChevronDown aria-hidden />
          </Button>
        </PopoverTrigger>
        <PopoverContent align="end" className="w-[420px] p-0">
          <Command>
            <CommandInput placeholder="搜索模型…" />
            <CommandList className="max-h-72">
              <CommandEmpty>没有匹配的模型</CommandEmpty>
              <CommandGroup>
                {models.map((model) => (
                  <CommandItem
                    key={model.id}
                    value={`${model.display_name} ${model.id}`}
                    onSelect={() => {
                      const level =
                        model.id === current.id
                          ? current.level
                          : (model.thinking_levels?.[0] ?? null);
                      choose(buildSelector(model.id, level));
                    }}
                    onFocus={() => setFocusId(model.id)}
                    onMouseEnter={() => setFocusId(model.id)}
                  >
                    <Check
                      className={cn(
                        "size-3.5",
                        model.id === current.id ? "opacity-100" : "opacity-0",
                      )}
                      aria-hidden
                    />
                    <span className="min-w-0 flex-1 truncate">{model.display_name}</span>
                    <span className="flex gap-2 text-muted-foreground text-xs">
                      {model.context_window ? (
                        <span>{formatTokens(model.context_window)} 上下文</span>
                      ) : null}
                      {model.max_output_tokens ? (
                        <span>{formatTokens(model.max_output_tokens)} 输出</span>
                      ) : null}
                      {priceLabel(model) ? <span>{priceLabel(model)}</span> : null}
                    </span>
                  </CommandItem>
                ))}
              </CommandGroup>
            </CommandList>
          </Command>
          {focused && levels.length > 0 ? (
            <fieldset className="flex flex-wrap items-center gap-1 border-t p-2">
              <legend className="sr-only">思考等级</legend>
              <span className="mr-1 text-muted-foreground text-xs">思考</span>
              {levels.map((level) => {
                const active = focused.id === current.id && (current.level ?? levels[0]) === level;
                return (
                  <Button
                    key={level}
                    size="xs"
                    variant={active ? "secondary" : "ghost"}
                    aria-pressed={active}
                    onClick={() => choose(buildSelector(focused.id, level))}
                  >
                    {level}
                  </Button>
                );
              })}
            </fieldset>
          ) : null}
        </PopoverContent>
      </Popover>
      <AlertDialog
        open={pendingOverflow !== null}
        onOpenChange={(value) => !value && setPendingOverflow(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>目标模型上下文不足</AlertDialogTitle>
            <AlertDialogDescription>
              {pendingOverflow
                ? `当前上下文约 ${formatTokens(pendingOverflow.overflow.tokens)} tokens，目标模型窗口 ${formatTokens(
                    pendingOverflow.overflow.context_window,
                  )}（压缩触发线 ${formatTokens(pendingOverflow.overflow.threshold)}）。`
                : null}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <Button variant="outline" onClick={() => resolveOverflow("switch")}>
              直接切换
            </Button>
            <AlertDialogAction onClick={() => resolveOverflow("compact")}>
              切换并压缩
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
