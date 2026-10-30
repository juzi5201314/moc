import { describe, expect, it } from "vitest";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import {
  activitySnippet,
  activityVerb,
  formatDuration,
  groupTurns,
  summarizeActivity,
  turnDurationMs,
} from "@/lib/turn";

function tool(name: string, extra: Partial<ToolBlock> = {}): Block {
  return {
    kind: "tool",
    id: `tool-${name}`,
    toolCallId: `call-${name}`,
    name,
    arguments: '{"path":"src/index.css"}',
    status: "completed",
    isError: false,
    images: [],
    children: [],
    awaitingPermission: false,
    ...extra,
  };
}

const userBlock = (id: string, text = "改一下配色"): Extract<Block, { kind: "user" }> => ({
  kind: "user",
  id,
  text,
  images: [],
  atMs: 1,
});

describe("groupTurns", () => {
  it("把一轮里的正文、活动与尾部统计分开", () => {
    const turns = groupTurns([
      userBlock("u1"),
      { kind: "reasoning", id: "r1", text: "先看主题包" },
      tool("read_file"),
      { kind: "text", id: "t1", text: "改完了" },
      { kind: "footer", id: "f1", atMs: 2, durationMs: 23_000 },
    ]);

    expect(turns).toHaveLength(1);
    expect(turns[0]?.user?.text).toBe("改一下配色");
    expect(turns[0]?.activity.map((block) => block.kind)).toEqual(["reasoning", "tool"]);
    expect(turns[0]?.texts).toHaveLength(1);
    expect(turns[0]?.footer?.durationMs).toBe(23_000);
  });

  it("按用户块切轮", () => {
    const turns = groupTurns([
      userBlock("u1"),
      { kind: "text", id: "t1", text: "第一轮" },
      userBlock("u2"),
      { kind: "text", id: "t2", text: "第二轮" },
    ]);

    expect(turns.map((turn) => turn.id)).toEqual(["u1", "u2"]);
    expect(turns[1]?.texts[0]?.text).toBe("第二轮");
  });

  it("开头没有用户块时也能成轮", () => {
    const turns = groupTurns([{ kind: "text", id: "t1", text: "续接" }]);
    expect(turns).toHaveLength(1);
    expect(turns[0]?.user).toBeUndefined();
    expect(turns[0]?.id).toBe("t1");
  });
});

describe("summarizeActivity", () => {
  it("用完成式归纳动作，按类别频次降序、思考不计入", () => {
    const summary = summarizeActivity([
      { kind: "bash", id: "b1", command: "ls", atMs: 1 },
      tool("read_file"),
      tool("read_file"),
      { kind: "reasoning", id: "r1", text: "想想" },
    ]);
    // read 出现两次排在 commands 之前；两类用「并」连接。
    expect(summary).toBe("已读取文件并执行了命令");
  });

  it("两类都带「已」前缀时，后者去掉重复前缀", () => {
    const summary = summarizeActivity([tool("read_file"), tool("grep")]);
    expect(summary).toBe("已读取文件并搜索代码");
  });

  it("三类并列用「，」，类别超过三个时补「等」", () => {
    const summary = summarizeActivity([
      { kind: "bash", id: "b1", command: "ls", atMs: 1 },
      tool("read_file"),
      tool("write_file"),
      tool("grep"),
      { kind: "reasoning", id: "r1", text: "想想" },
    ]);
    expect(summary).toBe("执行了命令，已读取文件，已写入文件等");
  });

  it("纯思考时归纳成「已完成分析」", () => {
    expect(summarizeActivity([{ kind: "reasoning", id: "r1", text: "想想" }])).toBe("已完成分析");
    expect(summarizeActivity([])).toBe("已完成分析");
    expect(summarizeActivity([{ kind: "text", id: "t1", text: "只有正文" }])).toBe("已完成分析");
  });

  it("进行中的组标题取最近动作的进行式", () => {
    expect(
      summarizeActivity(
        [
          { kind: "reasoning", id: "r1", text: "想想" },
          { kind: "bash", id: "b1", command: "ls", atMs: 1 },
        ],
        false,
      ),
    ).toBe("正在运行命令");
    expect(summarizeActivity([{ kind: "reasoning", id: "r1", text: "想想" }], false)).toBe(
      "正在分析请求",
    );
  });
});

describe("activityVerb / activitySnippet", () => {
  it("识别常见工具动词", () => {
    expect(activityVerb(tool("bash"))).toBe("运行命令");
    expect(activityVerb(tool("run_code"))).toBe("运行代码");
    expect(activityVerb(tool("sidekick"))).toBe("协调子智能体");
    expect(activityVerb(tool("read_file"))).toBe("读取文件");
    expect(activityVerb(tool("write_file"))).toBe("写入文件");
    expect(activityVerb(tool("mcp__fs__str_replace_editor"))).toBe("编辑文件");
    expect(activityVerb(tool("grep"))).toBe("搜索代码");
    expect(activityVerb(tool("web_search"))).toBe("搜索网页");
    expect(activityVerb(tool("web_fetch"))).toBe("访问网页");
    expect(activityVerb(tool("some_unknown_tool"))).toBe("调用工具");
    expect(activityVerb({ kind: "text", id: "t1", text: "x" })).toBeNull();
  });

  it("摘要折叠空白并截断", () => {
    const long = "a".repeat(120);
    expect(activitySnippet({ kind: "reasoning", id: "r1", text: long })).toHaveLength(81);
    expect(
      activitySnippet({ kind: "bash", id: "b1", command: "git  status\n--short", atMs: 1 }),
    ).toBe("git status --short");
    expect(
      activitySnippet(tool("bash", { arguments: '{"command":"cargo run -- --version"}' })),
    ).toBe("cargo run -- --version");
    expect(
      activitySnippet(
        tool("bash", {
          arguments: '{"description":"确认提交结果","command":"git status --short"}',
        }),
      ),
    ).toBe("确认提交结果");
    expect(activitySnippet(tool("read_file", { arguments: '{"path":"src/index.css"}' }))).toBe(
      "src/index.css",
    );
  });

  it("回放里被截断成非法 JSON 的参数仍能捞出 description", () => {
    // session 落盘的 arguments 可能被预算截断，description 在截断点之前时要保住语义标题。
    const truncated =
      '{"description":"确认待提交内容","command":"git diff --cached --stat && git s';
    expect(activitySnippet(tool("bash", { arguments: truncated }))).toBe("确认待提交内容");
  });
});

describe("formatDuration", () => {
  it("按分秒输出", () => {
    expect(formatDuration(23_000)).toBe("23秒");
    expect(formatDuration(737_000)).toBe("12分17秒");
    expect(formatDuration(0)).toBe("0秒");
  });
});

describe("turnDurationMs", () => {
  const firstTurn = (blocks: Block[]) => {
    const turn = groupTurns(blocks).at(0);
    if (!turn) throw new Error("期望至少分出一轮");
    return turn;
  };

  it("实时轮直接用 footer.durationMs", () => {
    const turn = firstTurn([
      userBlock("u1"),
      { kind: "text", id: "t1", text: "好了" },
      { kind: "footer", id: "f1", atMs: 100_000, durationMs: 23_000 },
    ]);
    expect(turnDurationMs(turn)).toBe(23_000);
  });

  it("回放历史会话没有记录耗时时按时间戳推算", () => {
    const turn = firstTurn([
      { ...userBlock("u1"), atMs: 10_000 },
      { kind: "text", id: "t1", text: "好了" },
      { kind: "footer", id: "f1", atMs: 70_000 },
    ]);
    expect(turnDurationMs(turn)).toBe(60_000);
  });

  it("缺少 footer 时给不出用时", () => {
    const turn = firstTurn([userBlock("u1"), { kind: "text", id: "t1", text: "进行中" }]);
    expect(turnDurationMs(turn)).toBeUndefined();
  });
});
