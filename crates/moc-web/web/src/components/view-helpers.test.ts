import { describe, expect, it } from "vitest";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import type { DiffLine, ModelInfo, ModelUsageSummary } from "@/protocol/types";
import { breadcrumbs } from "./dialogs/folder-picker";
import { estimateCost } from "./dialogs/usage-dialog";
import { toSplitRows } from "./transcript/diff-view";
import { toRows } from "./transcript/transcript-view";

function tool(id: string, name: string, batchId?: string): ToolBlock {
  return {
    kind: "tool",
    id,
    toolCallId: id,
    name,
    arguments: "{}",
    status: "completed",
    isError: false,
    images: [],
    children: [],
    awaitingPermission: false,
    batchId,
  } as ToolBlock;
}

describe("toRows", () => {
  it("groups consecutive same-name tools in one batch", () => {
    const text: Block = { kind: "text", id: "t", text: "hi" };
    const rows = toRows([
      text,
      tool("a", "read", "b1"),
      tool("b", "read", "b1"),
      tool("c", "read", "b1"),
      tool("d", "grep", "b1"),
      tool("e", "read", "b2"),
    ]);
    expect(rows.map((row) => ("group" in row ? row.group.map((t) => t.id) : row.key))).toEqual([
      "t",
      ["a", "b", "c"],
      "d",
      "e",
    ]);
    // 组的 key 沿用第一个块，避免虚拟列表重新测量
    expect(rows[1]?.key).toBe("a");
  });

  it("never groups tools without a batch id", () => {
    expect(toRows([tool("a", "read"), tool("b", "read")])).toHaveLength(2);
  });
});

describe("toSplitRows", () => {
  const line = (kind: DiffLine["kind"], text: string): DiffLine => ({ kind, text });

  it("pairs deletes with following inserts and pads the shorter side", () => {
    const rows = toSplitRows([
      line("equal", "a"),
      line("delete", "b"),
      line("delete", "c"),
      line("insert", "B"),
      line("equal", "d"),
      line("insert", "e"),
    ]);
    expect(rows.map((row) => [row.left?.text, row.right?.text])).toEqual([
      ["a", "a"],
      ["b", "B"],
      ["c", undefined],
      ["d", "d"],
      [undefined, "e"],
    ]);
  });
});

describe("breadcrumbs", () => {
  it("splits posix paths with cumulative paths", () => {
    expect(breadcrumbs("/home/me/proj")).toEqual([
      { name: "/", path: "/" },
      { name: "home", path: "/home" },
      { name: "me", path: "/home/me" },
      { name: "proj", path: "/home/me/proj" },
    ]);
    expect(breadcrumbs("/")).toEqual([{ name: "/", path: "/" }]);
  });

  it("handles windows drive roots", () => {
    expect(breadcrumbs("C:\\Users\\me")).toEqual([
      { name: "C:", path: "C:\\" },
      { name: "Users", path: "C:\\Users" },
      { name: "me", path: "C:\\Users\\me" },
    ]);
  });
});

describe("estimateCost", () => {
  const usage = {
    model: "m",
    calls: 1,
    input_tokens: 1_000_000,
    cached_input_tokens: 400_000,
    output_tokens: 500_000,
    active_ms: 0,
    ttft_ms: 0,
    ttft_calls: 0,
    gen_ms: 0,
  } satisfies ModelUsageSummary;

  it("returns null without pricing", () => {
    expect(estimateCost(usage, undefined)).toBeNull();
    expect(estimateCost(usage, { id: "m", display_name: "m" } as ModelInfo)).toBeNull();
  });

  it("bills cached input at cache_read and falls back to input price", () => {
    const priced = (cache_read?: number) =>
      ({ id: "m", display_name: "m", pricing: { input: 2, output: 10, cache_read } }) as ModelInfo;
    // 600k·2 + 400k·0.5 + 500k·10 = 1.2 + 0.2 + 5
    expect(estimateCost(usage, priced(0.5))).toBeCloseTo(6.4);
    // 无 cache_read：缓存部分按 input 计
    expect(estimateCost(usage, priced())).toBeCloseTo(7);
  });
});
