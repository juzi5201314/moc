import { describe, expect, it } from "vitest";
import type {
  SessionInfo,
  SessionUpdate,
  ToolDiff,
  TranscriptChildCall,
  TranscriptEntry,
} from "@/protocol/types";
import { applyUpdate, createView, entriesToBlocks, type ToolBlock } from "./transcript-model";

const INFO: SessionInfo = {
  id: "ses_1",
  title: "测试",
  model: "m1",
  permission: "workspace",
  engine: "standard",
  autonomy: "default",
  plan_active: false,
  engine_locked: false,
  cwd: "/tmp/work",
  created_at_ms: 0,
  updated_at_ms: 0,
};

const DIFF: ToolDiff = {
  files: [
    {
      path: "a.txt",
      hunks: [
        {
          old_start: 1,
          new_start: 1,
          lines: [
            { kind: "delete", old_no: 1, text: "old" },
            { kind: "insert", new_no: 1, text: "new" },
          ],
        },
      ],
    },
  ],
  truncated: false,
};

describe("entriesToBlocks", () => {
  const entries: TranscriptEntry[] = [
    { id: "e1", at_ms: 1, message: { role: "User", content: [{ Text: "你好" }] } },
    {
      id: "e2",
      at_ms: 2,
      message: {
        role: "Assistant",
        content: [
          { Reasoning: { Text: { text: "想一想", signature: null } } },
          { Text: "先跑代码" },
          { ToolCall: { id: "call_1", name: "run_code", arguments: '{"description":"x"}' } },
        ],
      },
    },
    {
      id: "e3",
      at_ms: 3,
      message: {
        role: "Tool",
        content: [{ ToolResult: { tool_call_id: "call_1", content: "ok", is_error: false } }],
      },
      tool: { tool_call_id: "call_1", status: "completed", duration_ms: 42 },
    },
    {
      id: "e4",
      at_ms: 4,
      message: { role: "User", content: [{ Text: "ls output" }] },
      bash: { command: "ls", exclude_from_context: false },
    },
    {
      id: "e5",
      at_ms: 5,
      message: {
        role: "Tool",
        content: [{ ToolResult: { tool_call_id: "orphan", content: "?", is_error: true } }],
      },
    },
  ];
  const childCalls: TranscriptChildCall[] = [
    {
      id: "child_1",
      parent_tool_call_id: "call_1",
      name: "read",
      arguments: "{}",
      result: "file",
      status: "completed",
      duration_ms: 5,
    },
  ];

  it("按顺序生成块并配对工具结果", () => {
    const blocks = entriesToBlocks(entries, childCalls);
    expect(blocks.map((block) => block.kind)).toEqual([
      "user",
      "reasoning",
      "text",
      "tool",
      "bash",
      "tool",
    ]);
    const tool = blocks[3] as ToolBlock;
    expect(tool.toolCallId).toBe("call_1");
    expect(tool.status).toBe("completed");
    expect(tool.result).toBe("ok");
    expect(tool.durationMs).toBe(42);
    expect(tool.children.map((child) => child.toolCallId)).toEqual(["child_1"]);
    const bash = blocks[4];
    expect(bash.kind === "bash" && bash.command).toBe("ls");
    const orphan = blocks[5] as ToolBlock;
    expect(orphan.name).toBe("工具结果");
    expect(orphan.status).toBe("error");
  });

  it("运行时提醒不进对话流", () => {
    const withReminder: TranscriptEntry[] = [
      { id: "u1", at_ms: 1, message: { role: "User", content: [{ Text: "你好" }] } },
      {
        id: "r1",
        at_ms: 2,
        message: {
          role: "User",
          content: [{ Text: "<system-reminder>Sandbox mode: full.</system-reminder>" }],
        },
        reminder: { items: [{ key: "sandbox", digest: "abc" }] },
      },
    ];
    const blocks = entriesToBlocks(withReminder, []);
    expect(blocks.map((block) => block.kind)).toEqual(["user"]);
    const user = blocks[0];
    expect(user.kind === "user" && user.text).toBe("你好");
  });

  it("历史里的 exit_plan 结果复原计划卡，失败的提交不复原", () => {
    const call = (id: string): TranscriptEntry => ({
      id: `a_${id}`,
      at_ms: 1,
      message: {
        role: "Assistant",
        content: [{ ToolCall: { id, name: "exit_plan", arguments: "{}" } }],
      },
    });
    const result = (id: string, content: string, isError: boolean): TranscriptEntry => ({
      id: `t_${id}`,
      at_ms: 2,
      message: {
        role: "Tool",
        content: [{ ToolResult: { tool_call_id: id, content, is_error: isError } }],
      },
    });
    const blocks = entriesToBlocks(
      [
        call("ok"),
        result("ok", "Plan plan_7 (v2) is ready for review at local://plan_7.md.", false),
        call("bad"),
        result("bad", "no plan draft to submit", true),
      ],
      [],
    );
    expect(blocks.map((block) => block.kind)).toEqual(["tool", "plan", "tool"]);
    expect(blocks[1]).toMatchObject({ planId: "plan_7", version: 2, path: "local://plan_7.md" });
  });
});

describe("applyUpdate", () => {
  it("手动 !命令的 tool_finished 回填 bash 块，不产生重复行", () => {
    const view = createView(INFO);
    applyUpdate(view, {
      type: "bash_started",
      tool_call_id: "bash_1",
      command: { command: "ls -la", exclude_from_context: false },
    });
    applyUpdate(view, {
      type: "tool_finished",
      prompt_id: "bash_1",
      tool_call_id: "bash_1",
      name: "bash",
      arguments: '{"command":"ls -la","exclude_from_context":false}',
      status: "completed",
      result: { tool_call_id: "bash_1", content: "total 8", is_error: false },
      duration_ms: 5,
    });
    expect(view.blocks.map((block) => block.kind)).toEqual(["bash"]);
    const bash = view.blocks[0];
    expect(bash.kind === "bash" && bash.command).toBe("ls -la");
    expect(bash.kind === "bash" && bash.output).toBe("total 8");
    expect(bash.kind === "bash" && bash.isError).toBe(false);
  });

  it("流式两段生成，最后一段以 message 为准", () => {
    const view = createView(INFO);
    const updates: SessionUpdate[] = [
      { type: "prompt_started", prompt_id: "p1", model: "m1", at_ms: 0 },
      { type: "generation_started", prompt_id: "p1" },
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "reasoning", delta: "思考" } },
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "text", delta: "第一段" } },
      {
        type: "tool_started",
        prompt_id: "p1",
        tool_call_id: "c1",
        name: "edit",
        arguments: "{}",
      },
      {
        type: "tool_finished",
        prompt_id: "p1",
        tool_call_id: "c1",
        name: "edit",
        arguments: "{}",
        status: "completed",
        result: { tool_call_id: "c1", content: "done", is_error: false },
        duration_ms: 10,
        diff: DIFF,
      },
      { type: "generation_started", prompt_id: "p1" },
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "text", delta: "第二段草稿" } },
      {
        type: "prompt_finished",
        prompt_id: "p1",
        message: { role: "Assistant", content: [{ Text: "第二段定稿" }] },
        finish_reason: "Stop",
        usage: {
          input_tokens: 1,
          output_tokens: 1,
          total_tokens: 2,
          cached_input_tokens: null,
          cache_creation_input_tokens: null,
          reasoning_tokens: null,
        },
      },
    ];
    for (const [index, update] of updates.entries()) applyUpdate(view, update, 1000 + index * 100);

    const texts = view.blocks.flatMap((block) => (block.kind === "text" ? [block.text] : []));
    expect(texts).toEqual(["第一段", "第二段定稿"]);
    expect(view.blocks.some((block) => block.kind === "reasoning" && block.text === "思考")).toBe(
      true,
    );
    const tool = view.blocks.find((block) => block.kind === "tool") as ToolBlock;
    expect(tool.status).toBe("completed");
    expect(tool.diff).toEqual(DIFF);
    expect(view.diffs).toHaveLength(1);
    expect(view.running).toBe(false);
    expect(view.live).toBeNull();
    expect(view.blocks.at(-1)?.kind).toBe("footer");
  });

  it("未知类型不改变状态", () => {
    const view = createView(INFO);
    const before = JSON.stringify(view);
    applyUpdate(view, { type: "future_thing" } as unknown as SessionUpdate);
    expect(JSON.stringify(view)).toBe(before);
  });

  it("回退失败生成时保留历史已提交内容", () => {
    const view = createView(INFO);
    const usage = {
      input_tokens: 1,
      output_tokens: 1,
      total_tokens: 2,
      cached_input_tokens: null,
      cache_creation_input_tokens: null,
      reasoning_tokens: null,
    };
    applyUpdate(view, { type: "prompt_started", prompt_id: "p0", model: "m1", at_ms: 0 }, 0);
    applyUpdate(view, { type: "generation_started", prompt_id: "p0" }, 1);
    applyUpdate(
      view,
      { type: "assistant_delta", prompt_id: "p0", delta: { type: "text", delta: "历史" } },
      2,
    );
    applyUpdate(
      view,
      {
        type: "prompt_finished",
        prompt_id: "p0",
        message: { role: "Assistant", content: [{ Text: "历史" }] },
        finish_reason: "Stop",
        usage,
      },
      3,
    );

    applyUpdate(view, { type: "prompt_started", prompt_id: "p1", model: "m1", at_ms: 4 }, 4);
    applyUpdate(view, { type: "generation_started", prompt_id: "p1" }, 5);
    applyUpdate(
      view,
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "text", delta: "半截" } },
      6,
    );
    applyUpdate(
      view,
      {
        type: "prompt_finished",
        prompt_id: "p1",
        message: { role: "Assistant", content: [{ Text: "半截" }] },
        finish_reason: "Cancelled",
        usage,
      },
      7,
    );
    applyUpdate(
      view,
      { type: "guard_notice", prompt_id: "p1", message: "保护机制中止", terminal: true },
      8,
    );
    applyUpdate(view, { type: "prompt_rewound", replayed_tools: false }, 9);

    expect(view.blocks.flatMap((block) => (block.kind === "text" ? [block.text] : []))).toEqual([
      "历史",
    ]);
    expect(view.blocks.filter((block) => block.kind === "footer")).toHaveLength(1);
    expect(view.blocks.some((block) => block.kind === "status")).toBe(false);
    expect(view.live).toBeNull();
    expect(view.running).toBe(false);
  });

  it("重放工具时保留已提交正文但移除失败工具与尾栏", () => {
    const view = createView(INFO);
    const usage = {
      input_tokens: 1,
      output_tokens: 1,
      total_tokens: 2,
      cached_input_tokens: null,
      cache_creation_input_tokens: null,
      reasoning_tokens: null,
    };
    applyUpdate(view, { type: "prompt_started", prompt_id: "p1", model: "m1", at_ms: 0 }, 0);
    applyUpdate(view, { type: "generation_started", prompt_id: "p1" }, 1);
    applyUpdate(
      view,
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "text", delta: "正文" } },
      2,
    );
    applyUpdate(
      view,
      {
        type: "tool_started",
        prompt_id: "p1",
        tool_call_id: "c1",
        name: "read",
        arguments: "{}",
      },
      3,
    );
    applyUpdate(
      view,
      {
        type: "prompt_finished",
        prompt_id: "p1",
        message: {
          role: "Assistant",
          content: [{ Text: "正文" }, { ToolCall: { id: "c1", name: "read", arguments: "{}" } }],
        },
        finish_reason: "Cancelled",
        usage,
      },
      4,
    );
    applyUpdate(view, { type: "prompt_rewound", replayed_tools: true }, 5);

    expect(view.blocks.flatMap((block) => (block.kind === "text" ? [block.text] : []))).toEqual([
      "正文",
    ]);
    expect(view.blocks.some((block) => block.kind === "tool")).toBe(false);
    expect(view.blocks.some((block) => block.kind === "footer")).toBe(false);
  });

  it("只回退同一 prompt 的最后 generation", () => {
    const view = createView(INFO);
    const usage = {
      input_tokens: 1,
      output_tokens: 1,
      total_tokens: 2,
      cached_input_tokens: null,
      cache_creation_input_tokens: null,
      reasoning_tokens: null,
    };
    applyUpdate(view, { type: "prompt_started", prompt_id: "p1", model: "m1", at_ms: 0 }, 0);
    applyUpdate(view, { type: "generation_started", prompt_id: "p1" }, 1);
    applyUpdate(
      view,
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "text", delta: "前一轮" } },
      2,
    );
    applyUpdate(
      view,
      {
        type: "tool_started",
        prompt_id: "p1",
        tool_call_id: "c1",
        name: "read",
        arguments: "{}",
      },
      3,
    );
    applyUpdate(
      view,
      {
        type: "tool_finished",
        prompt_id: "p1",
        tool_call_id: "c1",
        name: "read",
        arguments: "{}",
        status: "completed",
        result: { tool_call_id: "c1", content: "ok", is_error: false },
      },
      4,
    );
    applyUpdate(view, { type: "generation_started", prompt_id: "p1" }, 5);
    applyUpdate(
      view,
      { type: "assistant_delta", prompt_id: "p1", delta: { type: "text", delta: "失败尾巴" } },
      6,
    );
    applyUpdate(
      view,
      {
        type: "prompt_finished",
        prompt_id: "p1",
        message: { role: "Assistant", content: [{ Text: "失败尾巴" }] },
        finish_reason: "Cancelled",
        usage,
      },
      7,
    );
    applyUpdate(view, { type: "prompt_rewound", replayed_tools: false }, 8);

    expect(view.blocks.flatMap((block) => (block.kind === "text" ? [block.text] : []))).toEqual([
      "前一轮",
    ]);
    const tool = view.blocks.find((block) => block.kind === "tool") as ToolBlock;
    expect(tool?.toolCallId).toBe("c1");
    expect(tool?.status).toBe("completed");
  });
});
