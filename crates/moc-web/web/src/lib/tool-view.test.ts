import { describe, expect, it } from "vitest";
import {
  commandExit,
  findFiles,
  grepCountText,
  groupActivity,
  parseGrep,
  parseSearch,
  parseTodo,
  previewDiff,
  toggledLevel,
  toolHeadline,
  toolNotices,
} from "@/lib/tool-view";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import type { ToolDiff } from "@/protocol/types";

function tool(name: string, extra: Partial<ToolBlock> = {}): ToolBlock {
  return {
    kind: "tool",
    id: `tool-${name}-${extra.toolCallId ?? "1"}`,
    toolCallId: "1",
    name,
    arguments: "{}",
    status: "completed",
    isError: false,
    images: [],
    children: [],
    awaitingPermission: false,
    ...extra,
  };
}

it("省略与扫描警告保留，带行号源码和匹配正文不会被误识别", () => {
  const warnings = [
    "[Line 12 is 9 KB and exceeds the limit; content omitted; use bash to inspect it]",
    "[2 files, 3 matches shown; output capped at 1024 bytes. Continue with skip=2]",
    "[1 files exceeded the 1024-byte limit and were not searched]",
    "[Scan incomplete (time limit)]",
    "  [… 10 more; read the subdirectory to continue]",
    "[… 2048 bytes omitted; full output: artifact://large]",
  ];
  expect(
    toolNotices(
      ["12│[limit 10 reached]", "a.rs:1:[Scan incomplete]", "ordinary body", ...warnings].join(
        "\n",
      ),
    ),
  ).toEqual(warnings);
});

describe("grep", () => {
  it("单行与分组两种形态都只把 `:` 行计为匹配，上下文与提示行不计", () => {
    const result = [
      "src/a.rs:1:alpha",
      "src/a.rs-2-2026-10-19 12:34:56",
      "src/b.rs",
      "  3:beta",
      "  4-ctx 12:34:56",
      "  9:gamma",
      "src/c.rs-7-only context",
      "[3 files, 3 matches shown; limit reached. Continue with skip=3]",
    ].join("\n");
    expect(grepCountText(result)).toBe("3 matches · 2 files");
    const parsed = parseGrep(result);
    expect(parsed.groups.map((group) => [group.path, group.rows.length])).toEqual([
      ["src/a.rs", 2],
      ["src/b.rs", 3],
      ["src/c.rs", 1],
    ]);
    expect(parsed.notes).toHaveLength(1);
  });

  it("没有结果时单复数正确", () => {
    expect(grepCountText("No matches found")).toBe("0 matches · 0 files");
    expect(grepCountText("x.ts:1:hit")).toBe("1 match · 1 file");
  });
});

it("find 跳过空结果提示与扫描提示", () => {
  const { paths, notes } = findFiles("a.rs\nsrc/b.rs\n\n[limit 2 reached]\n");
  expect(paths).toEqual(["a.rs", "src/b.rs"]);
  expect(notes).toEqual(["[limit 2 reached]"]);
  expect(findFiles("No files found matching pattern").paths).toEqual([]);
});

describe("read", () => {
  it("标题的行号范围以真实交付的分页为准，否则按 offset/limit", () => {
    const paged = tool("read", {
      arguments: '{"path":"a.ts","offset":5,"limit":100}',
      result: "[a.ts#0000]\n5│x\n[Showing lines 5-60 of 60. Use offset=61 to continue.]",
    });
    expect(toolHeadline(paged).target).toBe("a.ts:5-60");
    expect(
      toolHeadline(tool("read", { arguments: '{"path":"a.ts","offset":5,"limit":10}' })).target,
    ).toBe("a.ts:5-14");
    expect(toolHeadline(tool("read", { arguments: '{"path":"a.ts"}' })).target).toBe("a.ts");
  });
});

it("bash 只在非零退出 / 被杀时给尾注", () => {
  expect(commandExit("ok\n")).toBeNull();
  expect(commandExit("boom\n(exit 2)")).toBe("exit 2");
  expect(commandExit("...\n(killed after 30s timeout)")).toBe("timeout");
  expect(commandExit("...\n(killed after output limit)")).toBe("killed");
  // 正文里出现的 "(exit 1)" 不在末尾就不算
  expect(commandExit("echo '(exit 1)'\ndone")).toBeNull();
});

it("todo 结果还原阶段、状态与原因", () => {
  const phases = parseTodo("Rust\n  [x] 协议\n  [/] 迁移\nWeb\n  [!] 截图 (等服务)\n  [ ] 文档\n");
  expect(phases.map((phase) => phase.phase)).toEqual(["Rust", "Web"]);
  expect(phases[0].items.map((item) => item.status)).toEqual(["completed", "in_progress"]);
  expect(phases[1].items[0]).toEqual({ status: "blocked", task: "截图", reason: "等服务" });
  expect(parseTodo("No todos.")).toEqual([]);
});

it("web_search 结果拆出回答与来源", () => {
  const parsed = parseSearch(
    "简短回答\n\n## Sources (2)\n[1] 标题一 (2026-10-01)\n    https://a.dev/x\n    摘要一\n[2] 标题二\n    https://b.dev\n",
  );
  expect(parsed.answer).toBe("简短回答");
  expect(parsed.hits).toEqual([
    { title: "标题一 (2026-10-01)", url: "https://a.dev/x", snippet: "摘要一" },
    { title: "标题二", url: "https://b.dev" },
  ]);
  expect(parseSearch("No results found for: q").hits).toEqual([]);
});

it("diff 预览跨 hunk 计行，超出部分折成计数", () => {
  const line = (n: number) => ({ kind: "insert" as const, new_no: n, text: `l${n}` });
  const diff: ToolDiff = {
    truncated: false,
    files: [
      {
        path: "/r/a.ts",
        hunks: [
          { old_start: 1, new_start: 1, lines: [1, 2, 3, 4].map(line) },
          { old_start: 20, new_start: 20, lines: [20, 21, 22].map(line) },
        ],
      },
      { path: "/r/b.ts", hunks: [{ old_start: 1, new_start: 1, lines: [1].map(line) }] },
    ],
  };
  const preview = previewDiff(diff, 6);
  expect(preview.hidden).toBe(2);
  expect(preview.diff.files).toHaveLength(1);
  expect(preview.diff.files[0].hunks.map((hunk) => hunk.lines.length)).toEqual([4, 2]);
});

it("手动切换在完整与当前档位的收起形态之间往返", () => {
  expect(toggledLevel("brief", "brief")).toBe("full");
  expect(toggledLevel("brief", "full")).toBe("brief");
  expect(toggledLevel("hidden", "full")).toBe("hidden");
  expect(toggledLevel("full", "full")).toBe("hidden");
  expect(toggledLevel("full", "hidden")).toBe("full");
});

describe("groupActivity", () => {
  it("同批次连续同名的只看标题工具合并，其他保持原样", () => {
    const blocks: Block[] = [
      tool("read", { toolCallId: "a", batchId: "e1" }),
      tool("read", { toolCallId: "b", batchId: "e1" }),
      tool("read", { toolCallId: "c", batchId: "e1" }),
      tool("edit", { toolCallId: "d", batchId: "e1" }),
      tool("edit", { toolCallId: "e", batchId: "e1" }),
      tool("read", { toolCallId: "f", batchId: "e2" }),
    ];
    const items = groupActivity(blocks);
    expect(
      items.map((item) => (item.kind === "group" ? `group:${item.tools.length}` : "one")),
    ).toEqual(["group:3", "one", "one", "one"]);
  });

  it("不同批次或中间隔了别的块不合并", () => {
    const items = groupActivity([
      tool("grep", { toolCallId: "a", batchId: "e1" }),
      { kind: "reasoning", id: "r", text: "想想" },
      tool("grep", { toolCallId: "b", batchId: "e1" }),
    ]);
    expect(items.every((item) => item.kind === "block")).toBe(true);
  });
});
