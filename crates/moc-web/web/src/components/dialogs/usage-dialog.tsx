// 用量对话框：`session/usage` 按模型汇总，加一行合计。
// 有定价的模型按 (input−cached)·input + cached·(cache_read ?? input) + output·output 估算费用（单价按每百万 token）。

import { useEffect, useState } from "react";
import { parseSelector } from "@/components/composer/parse";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { formatDuration, formatTokens } from "@/lib/format";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type { ModelInfo, ModelUsageSummary, SessionUsageResult } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useUi } from "@/store/ui";

// 选择器外兜底，保持引用稳定（zustand 按引用比较）。
const NO_MODELS: ModelInfo[] = [];

export function estimateCost(
  usage: ModelUsageSummary,
  model: ModelInfo | undefined,
): number | null {
  const pricing = model?.pricing;
  if (!pricing) return null;
  const cached = usage.cached_input_tokens ?? 0;
  const uncached = Math.max(0, usage.input_tokens - cached);
  return (
    (uncached * pricing.input +
      cached * (pricing.cache_read ?? pricing.input) +
      usage.output_tokens * pricing.output) /
    1e6
  );
}

function percent(part: number, whole: number): string {
  return whole > 0 ? `${Math.round((part / whole) * 100)}%` : "—";
}

function Row({
  usage,
  cost,
  showCost,
  strong,
}: {
  usage: ModelUsageSummary;
  cost: number | null;
  showCost: boolean;
  strong?: boolean;
}) {
  const tps = usage.gen_ms > 0 ? (usage.output_tokens / (usage.gen_ms / 1000)).toFixed(1) : "—";
  const ttft = usage.ttft_calls > 0 ? formatDuration(usage.ttft_ms / usage.ttft_calls) : "—";
  return (
    <tr className={strong ? "border-t font-medium" : "border-t"}>
      <td className="max-w-48 truncate py-1.5 pr-3 text-left" title={usage.model}>
        {usage.model}
      </td>
      <td className="px-2 text-right">{usage.calls}</td>
      <td className="px-2 text-right">{formatTokens(usage.input_tokens)}</td>
      <td className="px-2 text-right">{formatTokens(usage.output_tokens)}</td>
      <td className="px-2 text-right">
        {percent(usage.cached_input_tokens ?? 0, usage.input_tokens)}
      </td>
      <td className="px-2 text-right">{formatTokens(usage.reasoning_tokens ?? 0)}</td>
      <td className="px-2 text-right">{formatDuration(usage.gen_ms)}</td>
      <td className="px-2 text-right">{ttft}</td>
      <td className="px-2 text-right">{tps}</td>
      {showCost ? (
        <td className="pl-2 text-right">{cost === null ? "—" : `$${cost.toFixed(4)}`}</td>
      ) : null}
    </tr>
  );
}

export function UsageDialog() {
  const sessionId = useUi((state) => state.usage);
  const set = useUi((state) => state.set);
  const models = useConnection((state) => state.config?.models) ?? NO_MODELS;
  const [result, setResult] = useState<SessionUsageResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!sessionId) return;
    let alive = true;
    setResult(null);
    setError(null);
    rpc
      .call<SessionUsageResult>("session/usage", { session_id: sessionId })
      .then((value) => alive && setResult(value))
      .catch(
        (err: unknown) => alive && setError(err instanceof RpcError ? err.message : String(err)),
      );
    return () => {
      alive = false;
    };
  }, [sessionId]);

  const modelOf = (selector: string) => {
    const { id } = parseSelector(
      selector,
      (key) => models.find((m) => m.id === key)?.thinking_levels,
    );
    return models.find((m) => m.id === id);
  };
  const costs = result?.models.map((usage) => estimateCost(usage, modelOf(usage.model))) ?? [];
  const showCost = costs.some((cost) => cost !== null);
  const totalCost = costs.reduce<number>((sum, cost) => sum + (cost ?? 0), 0);

  return (
    <Dialog open={sessionId !== null} onOpenChange={(open) => !open && set({ usage: null })}>
      <DialogContent className="sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>用量</DialogTitle>
          <DialogDescription>
            {result ? `工具耗时 ${formatDuration(result.tool_ms)}` : "本会话的模型调用统计"}
          </DialogDescription>
        </DialogHeader>
        {error ? <p className="text-destructive text-sm">{error}</p> : null}
        {!result && !error ? <p className="text-muted-foreground text-sm">加载中…</p> : null}
        {result ? (
          <div className="overflow-x-auto">
            <table className="w-full text-xs tabular-nums">
              <thead className="text-muted-foreground">
                <tr>
                  <th className="pb-1.5 text-left font-normal">模型</th>
                  <th className="px-2 text-right font-normal">调用</th>
                  <th className="px-2 text-right font-normal">输入</th>
                  <th className="px-2 text-right font-normal">输出</th>
                  <th className="px-2 text-right font-normal">缓存命中</th>
                  <th className="px-2 text-right font-normal">思考</th>
                  <th className="px-2 text-right font-normal">生成时间</th>
                  <th className="px-2 text-right font-normal">TTFT</th>
                  <th className="px-2 text-right font-normal">TPS</th>
                  {showCost ? <th className="pl-2 text-right font-normal">费用</th> : null}
                </tr>
              </thead>
              <tbody>
                {result.models.map((usage, index) => (
                  <Row
                    key={usage.model}
                    usage={usage}
                    cost={costs[index] ?? null}
                    showCost={showCost}
                  />
                ))}
                <Row
                  usage={{ ...result.total, model: "合计" }}
                  cost={showCost ? totalCost : null}
                  showCost={showCost}
                  strong
                />
              </tbody>
            </table>
          </div>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}
