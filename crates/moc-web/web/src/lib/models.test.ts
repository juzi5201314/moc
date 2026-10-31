import { describe, expect, it } from "vitest";
import { sessionCost } from "@/lib/models";
import type { ModelInfo, ModelUsageSummary } from "@/protocol/types";

function row(
  model: string,
  source: ModelUsageSummary["source"],
  input: number,
  cached: number,
  output: number,
): ModelUsageSummary {
  return {
    model,
    source,
    calls: 1,
    input_tokens: input,
    output_tokens: output,
    cached_input_tokens: cached,
    active_ms: 0,
    ttft_ms: 0,
    ttft_calls: 0,
    gen_ms: 0,
  };
}

describe("sessionCost", () => {
  it("与 TUI 同算法：每行按自身模型定价，缓存命中按缓存价，无定价按 0", () => {
    const models: ModelInfo[] = [
      {
        id: "p/m",
        display_name: "m",
        pricing: { input: 2, output: 10, cache_read: 0.2 },
        remote_compaction: false,
      },
      {
        id: "p/plain",
        display_name: "plain",
        pricing: { input: 1, output: 4 },
        remote_compaction: false,
      },
    ];
    const cost = sessionCost(
      {
        models: [
          row("p/m:off", "session", 1_000_000, 400_000, 100_000),
          row("p/m:off", "sidekick", 500_000, 0, 50_000),
          // 没有缓存价时命中部分退回输入价：1M*1 + 0.5M*4 = 3。
          row("p/plain", "subagent", 1_000_000, 300_000, 500_000),
          row("x/unknown", "subagent", 9_000_000, 0, 9_000_000),
        ],
        total: row("total", "session", 0, 0, 0),
        tool_ms: 0,
      },
      models,
    );
    // 会话行 0.6M*2 + 0.4M*0.2 + 0.1M*10 = 2.28；sidekick 行 1.5；plain 行 3。
    expect(cost).toBeCloseTo(6.78, 9);
  });
});
