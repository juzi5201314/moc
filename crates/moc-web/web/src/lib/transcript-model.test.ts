import { describe, expect, it } from "vitest";
import type {
  ContentPart,
  JobInfo,
  SessionInfo,
  SessionUpdate,
  ToolDiff,
  TranscriptChildCall,
  TranscriptEntry,
} from "@/protocol/types";
import {
  applyUpdate,
  createView,
  entriesToBlocks,
  historyDiffs,
  interruptLive,
  type ToolBlock,
} from "./transcript-model";

const INFO: SessionInfo = {
  id: "ses_1",
  title: "测试",
  model: "m1",
  permission: "workspace",
  engine: "standard",
  autonomy: "default",
  plan_active: false,
  fast_mode: false,
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

describe("工具流式预览", () => {
  const preview = (id: string): SessionUpdate => ({
    type: "tool_preview",
    prompt_id: "p",
    preview_id: id,
    tool_call_id: null,
    name: "write",
    preview: { target: "a.rs", summary: "", body: "生成中", language: null, truncated: false },
  });
  it("确认身份后原位转换，正式参数与 diff 覆盖预览", () => {
    const view = createView(INFO);
    applyUpdate(view, preview("first"));
    applyUpdate(view, preview("second"));
    const firstId = view.blocks[0].id;
    applyUpdate(view, {
      type: "tool_previews_resolved",
      prompt_id: "p",
      calls: [
        { preview_id: "first", tool_call_id: "a" },
        { preview_id: "second", tool_call_id: "b" },
      ],
    });
    applyUpdate(view, {
      type: "tool_batch_started",
      prompt_id: "p",
      batch_id: "batch",
      calls: [
        { tool_call_id: "a", name: "write" },
        { tool_call_id: "b", name: "write" },
      ],
    });
    applyUpdate(view, {
      type: "tool_started",
      prompt_id: "p",
      tool_call_id: "a",
      name: "write",
      arguments: '{"content":"最终内容"}',
    });
    expect(view.blocks).toHaveLength(2);
    expect(view.blocks[0]).toMatchObject({
      id: firstId,
      status: "running",
      preview: undefined,
      batchId: "batch",
      arguments: '{"content":"最终内容"}',
    });
    applyUpdate(view, {
      type: "tool_finished",
      prompt_id: "p",
      tool_call_id: "a",
      name: "write",
      arguments: '{"content":"最终内容"}',
      status: "completed",
      result: { tool_call_id: "a", content: "已写入", is_error: false },
      diff: DIFF,
    });
    applyUpdate(view, {
      type: "generation_retry",
      prompt_id: "p",
      message: "重试",
      retry_in_ms: 0,
    });
    const tools = view.blocks.filter((block) => block.kind === "tool");
    expect(tools).toHaveLength(1);
    expect(tools[0]).toMatchObject({
      id: firstId,
      diff: DIFF,
      preview: undefined,
      status: "completed",
    });
  });
  it("正文与预览交错时保持位置，断线清理预览", () => {
    const view = createView(INFO);
    const text = (delta: string): SessionUpdate => ({
      type: "assistant_delta",
      prompt_id: "p",
      delta: { type: "text", delta },
    });
    applyUpdate(view, text("之前"));
    applyUpdate(view, preview("draft"));
    applyUpdate(view, text("之后"));
    expect(view.blocks.map((block) => block.kind)).toEqual(["text", "tool", "text"]);
    expect(view.blocks[0]).toMatchObject({ text: "之前" });
    expect(view.blocks[2]).toMatchObject({ text: "之后" });
    applyUpdate(view, {
      type: "prompt_finished",
      prompt_id: "p",
      message: { role: "Assistant", content: [{ Text: "之前之后" }] },
      finish_reason: "Stop",
      usage: { input_tokens: 0, output_tokens: 0 },
    });
    expect(view.blocks.filter((block) => block.kind === "text").map((block) => block.text)).toEqual(
      ["之前", "之后"],
    );
    applyUpdate(view, preview("another"));
    interruptLive(view);
    expect(view.blocks.some((block) => block.kind === "tool")).toBe(false);
  });
});

describe("entriesToBlocks", () => {
  it("恢复 Responses 摘要并且不显示加密数据", () => {
    const blocks = entriesToBlocks(
      [
        {
          id: "reasoning",
          at_ms: 0,
          message: {
            role: "Assistant",
            content: [
              {
                Reasoning: {
                  Item: {
                    type: "reasoning",
                    encrypted_content: "opaque",
                    summary: [],
                    _moc_display_text: "流式摘要",
                  },
                },
              },
              {
                Reasoning: {
                  Item: {
                    type: "reasoning",
                    summary: [
                      { type: "summary_text", text: "完整摘要" },
                      { type: "summary_text", text: "第二段" },
                    ],
                  },
                },
              },
              {
                Reasoning: {
                  Item: { type: "reasoning", encrypted_content: "opaque", summary: [] },
                },
              },
            ],
          },
        },
      ],
      [],
    );
    expect(blocks.filter((block) => block.kind === "reasoning").map((block) => block.text)).toEqual(
      ["流式摘要", "完整摘要\n\n第二段"],
    );
  });

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
          { ToolCall: { id: "call_1", name: "execute", arguments: '{"description":"x"}' } },
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
    expect(orphan.name).toBe("Tool result");
    expect(orphan.status).toBe("error");
  });

  it("历史改动（含子调用）复原为变更记录，刷新后变更分页不丢", () => {
    const withDiff = entries.map((entry) =>
      entry.id === "e3" && entry.tool ? { ...entry, tool: { ...entry.tool, diff: DIFF } } : entry,
    );
    const blocks = entriesToBlocks(withDiff, [{ ...childCalls[0], diff: DIFF }]);
    expect(historyDiffs(blocks).map((record) => [record.toolCallId, record.atMs])).toEqual([
      ["child_1", 2],
      ["call_1", 2],
    ]);
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
  it("工具执行后请求重试，取消、成功和耗尽失败均保留已提交正文与思考且不重复", () => {
    const cases: [boolean, "error" | "tool" | "cancel" | "success"][] = [
      [false, "error"],
      [true, "error"],
      [false, "tool"],
      [true, "cancel"],
      [true, "success"],
    ];
    for (const [retry, outcome] of cases) {
      const view = createView(INFO);
      applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m", at_ms: 0 });
      applyUpdate(view, { type: "generation_started", prompt_id: "p" });
      applyUpdate(view, {
        type: "assistant_delta",
        prompt_id: "p",
        delta: { type: "reasoning", delta: "已完成分析" },
      });
      applyUpdate(view, {
        type: "assistant_delta",
        prompt_id: "p",
        delta: { type: "text", delta: "先检查文件" },
      });
      applyUpdate(view, {
        type: "tool_started",
        prompt_id: "p",
        tool_call_id: "read",
        name: "read",
        arguments: '{"path":"test.txt"}',
      });
      applyUpdate(view, {
        type: "tool_finished",
        prompt_id: "p",
        tool_call_id: "read",
        name: "read",
        arguments: '{"path":"test.txt"}',
        status: "completed",
        result: { tool_call_id: "read", content: "文件内容", is_error: false },
      });
      const contents = () =>
        view.blocks.flatMap((block) =>
          block.kind === "text" || block.kind === "reasoning" ? [block.text] : [],
        );
      if (retry) {
        for (let attempt = 2; attempt <= 3; attempt++) {
          applyUpdate(view, {
            type: "generation_retry",
            prompt_id: "p",
            message: `连接失败 ${attempt}/3`,
            retry_in_ms: 1000,
          });
          expect(contents()).toEqual(["已完成分析", "先检查文件"]);
        }
      }
      if (outcome === "success") {
        applyUpdate(view, { type: "generation_started", prompt_id: "p" });
        applyUpdate(view, {
          type: "assistant_delta",
          prompt_id: "p",
          delta: { type: "text", delta: "检查完成" },
        });
      }
      const content: ContentPart[] =
        outcome === "cancel" || outcome === "tool"
          ? [
              { Reasoning: { Text: { text: "已完成分析", signature: null } } },
              { Text: "先检查文件" },
            ]
          : outcome === "success"
            ? [{ Text: "检查完成" }]
            : [];
      applyUpdate(view, {
        type: "prompt_finished",
        prompt_id: "p",
        message: { role: "Assistant", content },
        finish_reason:
          outcome === "success"
            ? "Stop"
            : outcome === "cancel"
              ? "Cancelled"
              : outcome === "tool"
                ? "ToolCalls"
                : { Other: "error" },
        usage: { input_tokens: 0, output_tokens: 0 },
      });
      expect(contents()).toEqual(
        outcome === "success"
          ? ["已完成分析", "先检查文件", "检查完成"]
          : ["已完成分析", "先检查文件"],
      );
      expect(view.blocks.some((block) => block.kind === "status" && block.retryWait)).toBe(false);
      expect(view.running).toBe(false);
      expect(view.live).toBeNull();
      expect(view.blocks.find((block) => block.kind === "tool")).toMatchObject({
        status: "completed",
      });
    }
  });
  it("加载更早历史后增量仍追加到原活动块", () => {
    const view = createView(INFO);
    const delta = (text: string): SessionUpdate => ({
      type: "assistant_delta",
      prompt_id: "p",
      delta: { type: "text", delta: text },
    });
    applyUpdate(view, delta("开始"), 1);
    const id = view.live?.textId;
    view.blocks.unshift({ kind: "text", id: "older", text: "旧历史" });
    applyUpdate(view, delta("继续"), 2);
    expect(view.blocks).toMatchObject([
      { id: "older", text: "旧历史" },
      { id, text: "开始继续" },
    ]);
  });

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

  it("任务事件更新 Bash 卡片、实时尾部和最终耗时", () => {
    const view = createView(INFO);
    const job: JobInfo = {
      job_id: "job_1",
      command: "cargo build",
      tool_call_id: "bash_1",
      status: "running",
      lifecycle: "monitored",
      origin: "foreground",
      stalled: false,
      output_elided: false,
      started_at_ms: 1_000,
      log_uri: "artifact://jobs/job_1.log",
    };
    applyUpdate(view, {
      type: "bash_started",
      tool_call_id: "bash_1",
      command: { command: "cargo build", exclude_from_context: false },
    });
    applyUpdate(view, { type: "job_started", job }, 1_000);
    applyUpdate(view, {
      type: "job_output",
      job_id: "job_1",
      tool_call_id: "bash_1",
      tail: "Compiling moc",
    });
    const finished: JobInfo = { ...job, status: "exited", exit_code: 0, duration_ms: 2_500 };
    applyUpdate(view, { type: "job_finished", job: finished }, 3_500);
    const bash = view.blocks[0];
    expect(bash.kind === "bash" && bash.job?.status).toBe("exited");
    expect(bash.kind === "bash" && bash.liveOutput).toBe("Compiling moc");
    expect(bash.kind === "bash" && bash.durationMs).toBe(2_500);
    expect(view.jobs.job_1).toEqual(finished);
    // 前台任务结束不推「后台任务结束」状态条——结果已随工具卡片落地。
    expect(view.blocks.some((block) => block.kind === "status")).toBe(false);
  });

  it("后台任务结束推状态条", () => {
    const view = createView(INFO);
    const job: JobInfo = {
      job_id: "job_1",
      command: "cargo build",
      tool_call_id: "bash_1",
      status: "running",
      lifecycle: "monitored",
      origin: "auto_background",
      stalled: false,
      output_elided: false,
      started_at_ms: 1_000,
      log_uri: "artifact://jobs/job_1.log",
    };
    applyUpdate(view, {
      type: "bash_started",
      tool_call_id: "bash_1",
      command: { command: "cargo build", exclude_from_context: false },
    });
    applyUpdate(view, { type: "job_started", job }, 1_000);
    applyUpdate(view, {
      type: "job_finished",
      job: { ...job, status: "exited", exit_code: 0, duration_ms: 2_500 },
    });
    const status = view.blocks.find((block) => block.kind === "status");
    expect(status?.kind === "status" && status.text).toBe(
      "后台任务已结束：cargo build（退出码 0）",
    );
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
  it("自动重试只丢弃当前草稿并保持运行状态", () => {
    const view = createView(INFO);
    applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m1", at_ms: 0 });
    applyUpdate(view, { type: "generation_started", prompt_id: "p" });
    applyUpdate(view, {
      type: "assistant_delta",
      prompt_id: "p",
      delta: { type: "text", delta: "已完成" },
    });
    applyUpdate(view, { type: "generation_started", prompt_id: "p" });
    applyUpdate(view, {
      type: "assistant_delta",
      prompt_id: "p",
      delta: { type: "reasoning", delta: "失败思考" },
    });
    applyUpdate(view, {
      type: "assistant_delta",
      prompt_id: "p",
      delta: { type: "text", delta: "失败正文" },
    });
    applyUpdate(view, { type: "generation_retry", prompt_id: "p", message: "等待重试" });
    expect(view.running).toBe(true);
    expect(
      view.blocks.flatMap((block) =>
        block.kind === "text" || block.kind === "reasoning" ? [block.text] : [],
      ),
    ).toEqual(["已完成"]);
    applyUpdate(view, { type: "generation_started", prompt_id: "p" });
    applyUpdate(view, {
      type: "assistant_delta",
      prompt_id: "p",
      delta: { type: "text", delta: "新正文" },
    });
    expect(view.blocks.flatMap((block) => (block.kind === "text" ? [block.text] : []))).toEqual([
      "已完成",
      "新正文",
    ]);
  });

  it("重试提示更新同一块，结束、断线及回退均清理临时提示而不删除其他警告", () => {
    const endings: (SessionUpdate | null)[] = [
      { type: "generation_started", prompt_id: "p" },
      {
        type: "prompt_finished",
        prompt_id: "p",
        message: { role: "Assistant", content: [] },
        finish_reason: "Stop",
        usage: { input_tokens: 0, output_tokens: 0 },
      },
      {
        type: "prompt_finished",
        prompt_id: "p",
        message: { role: "Assistant", content: [] },
        finish_reason: "Cancelled",
        usage: { input_tokens: 0, output_tokens: 0 },
      },
      { type: "guard_notice", prompt_id: "p", message: "终止", terminal: true },
      { type: "prompt_rewound", replayed_tools: false },
      null,
    ];
    for (const ending of endings) {
      const view = createView(INFO);
      applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m", at_ms: 0 }, 0);
      applyUpdate(
        view,
        { type: "guard_notice", prompt_id: "p", message: "保留其他警告", terminal: false },
        0,
      );
      let id: string | undefined;
      for (let attempt = 2; attempt <= 8; attempt++) {
        applyUpdate(
          view,
          { type: "generation_retry", prompt_id: "p", message: `${attempt}/10`, retry_in_ms: 2000 },
          attempt * 1000,
        );
        const notices = view.blocks.filter((block) => block.kind === "status" && block.retryWait);
        expect(notices).toHaveLength(1);
        id ??= notices[0].id;
        expect(notices[0]).toMatchObject({
          id,
          text: `${attempt}/10`,
          retryWait: { promptId: "p", startedAt: attempt * 1000, deadline: attempt * 1000 + 2000 },
        });
      }
      if (ending) applyUpdate(view, ending, 10000);
      else interruptLive(view);
      expect(view.blocks.some((block) => block.kind === "status" && block.retryWait)).toBe(false);
      expect(
        view.blocks.some((block) => block.kind === "status" && block.text === "保留其他警告"),
      ).toBe(true);
    }
  });

  it("tool_preview 不丢失本轮思考 token，历史回放还原思考耗时/token、bash 排除标记与 IRC 投递状态", () => {
    const view = createView(INFO);
    applyUpdate(view, { type: "prompt_started", prompt_id: "p", model: "m", at_ms: 0 }, 1000);
    applyUpdate(
      view,
      { type: "assistant_delta", prompt_id: "p", delta: { type: "reasoning", delta: "先看代码" } },
      1000,
    );
    applyUpdate(
      view,
      {
        type: "tool_preview",
        prompt_id: "p",
        preview_id: "pv1",
        tool_call_id: null,
        name: "read",
        preview: { target: "a.rs", summary: "", body: "", language: null, truncated: false },
      },
      2500,
    );
    applyUpdate(
      view,
      {
        type: "assistant_delta",
        prompt_id: "p",
        delta: {
          type: "usage",
          usage: { input_tokens: 10, output_tokens: 20, reasoning_tokens: 345 },
        },
      },
      2600,
    );
    const reasoning = view.blocks.find((block) => block.kind === "reasoning");
    expect(reasoning).toMatchObject({ durationMs: 1500, tokens: 345 });

    // 新一轮生成不再把 usage 回填到旧思考块
    applyUpdate(view, { type: "generation_started", prompt_id: "p" }, 3000);
    applyUpdate(
      view,
      {
        type: "assistant_delta",
        prompt_id: "p",
        delta: {
          type: "usage",
          usage: { input_tokens: 10, output_tokens: 20, reasoning_tokens: 999 },
        },
      },
      3100,
    );
    expect(reasoning).toMatchObject({ tokens: 345 });

    const restored = entriesToBlocks([
      {
        id: "e1",
        at_ms: 100,
        message: { role: "User", content: [{ Text: "out" }] },
        bash: { command: "ls", exclude_from_context: true },
        tool: { tool_call_id: "b1", status: "error", duration_ms: 161 },
      },
      {
        id: "e2",
        at_ms: 200,
        message: { role: "Custom", content: [{ Text: "msg" }] },
        agent: { from_id: "a1", from_title: "Scout", delivery: "woken" },
      },
      {
        id: "e3",
        at_ms: 300,
        message: {
          role: "Assistant",
          content: [{ Reasoning: { Text: { text: "思考" } } }, { Text: "结论" }],
        },
        response: { model: "p/m", reasoning_duration_ms: 2400, reasoning_tokens: 512 },
      },
    ]);
    expect(restored[0]).toMatchObject({
      kind: "bash",
      excludeFromContext: true,
      isError: true,
      durationMs: 161,
    });
    expect(restored[1]).toMatchObject({ kind: "agent-message", from: "Scout", delivery: "woken" });
    expect(restored[2]).toMatchObject({ kind: "reasoning", durationMs: 2400, tokens: 512 });
  });
});
