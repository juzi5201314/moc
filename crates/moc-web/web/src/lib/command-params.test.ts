import { describe, expect, it } from "vitest";
import type { SlashCommand } from "@/lib/slash";
import {
  applyCommandChipEdit,
  buildCommandEntries,
  buildSubmenuItems,
  type CommandContext,
  commandTrailing,
  firstEnabledIndex,
  parseCommandChip,
  resolveStage2,
  stepEnabledIndex,
} from "./command-params";

const BASE_CTX: CommandContext = {
  isDraft: false,
  promptRunning: false,
  skillsWorking: false,
  autonomy: "dont_ask",
  permission: "workspace",
  mcpServers: [
    {
      name: "github",
      phase: "connected",
      target: "https://mcp.example/github",
      tool_count: 12,
      transport: "http",
    },
    {
      name: "local-db",
      phase: "failed",
      target: "npx db-mcp",
      error: "spawn ENOENT",
      transport: "stdio",
    },
  ],
};

const COMMANDS: SlashCommand[] = [
  {
    name: "export",
    description: "导出为 Markdown",
    group: "session",
    needsSession: true,
    availableDuringTask: true,
    params: [
      {
        name: "path",
        kind: "path",
        required: false,
        description: "自定义保存路径",
        placeholder: "例如 ./session.md",
        defaultBehavior: "保存到默认路径并下载",
      },
    ],
  },
  {
    name: "new",
    description: "新建 session",
    group: "session",
    needsSession: false,
    availableDuringTask: true,
  },
  {
    name: "goal",
    description: "管理 session 目标",
    group: "session",
    needsSession: true,
    availableDuringTask: true,
    params: [
      {
        name: "objective",
        kind: "choice",
        required: false,
        acceptsText: true,
        description: "输入目标或选择子命令",
        placeholder: "输入新的 session 目标…",
        defaultBehavior: "查看当前目标",
        choices: [
          {
            value: "edit",
            description: "修改当前目标",
            params: [
              {
                name: "objective",
                kind: "text",
                required: true,
                description: "替换目标描述",
                placeholder: "输入修改后的目标…",
              },
            ],
          },
          { value: "pause", description: "暂停自动续跑" },
          { value: "clear", description: "清除目标" },
        ],
      },
    ],
  },
  {
    name: "autonomy",
    description: "切换自主程度",
    group: "mode",
    needsSession: false,
    availableDuringTask: false,
    params: [
      {
        name: "level",
        kind: "choice",
        required: false,
        description: "选择自主档位",
        defaultBehavior: "循环切换到下一档",
        choices: [
          { value: "default", description: "常规确认" },
          { value: "dont_ask", description: "减少询问" },
          { value: "unattended", description: "无人值守" },
        ],
      },
    ],
  },
  {
    name: "permission",
    description: "切换权限模式",
    group: "mode",
    needsSession: false,
    availableDuringTask: false,
    params: [
      {
        name: "mode",
        kind: "dynamic",
        required: false,
        description: "选择权限级别",
        defaultBehavior: "查看当前权限",
        dynamic: "sandbox_mode",
        choices: [{ value: "clear", description: "清空授权" }],
      },
    ],
  },
  {
    name: "ask",
    description: "只读问答",
    group: "mode",
    needsSession: false,
    availableDuringTask: true,
    params: [
      {
        name: "question",
        kind: "text",
        required: true,
        description: "只读问题内容",
        placeholder: "输入只读问题…",
      },
    ],
  },
  {
    name: "mcp",
    description: "管理 MCP",
    group: "plugins",
    needsSession: false,
    availableDuringTask: true,
    params: [
      {
        name: "action",
        kind: "choice",
        required: false,
        description: "MCP 操作",
        defaultBehavior: "打开 MCP 面板",
        choices: [
          {
            value: "reconnect",
            description: "重连 MCP",
            params: [
              {
                name: "server",
                kind: "dynamic",
                required: false,
                description: "指定单个 server",
                placeholder: "选择 server",
                defaultBehavior: "重连全部 MCP",
                dynamic: "mcp_server",
              },
            ],
          },
        ],
      },
    ],
  },
  {
    name: "compact-skills",
    description: "压缩 skills",
    group: "context",
    needsSession: false,
    availableDuringTask: false,
    params: [
      {
        name: "action",
        kind: "choice",
        required: false,
        description: "选择维护方式",
        defaultBehavior: "增量压缩",
        choices: [
          { value: "compact", description: "增量压缩" },
          {
            value: "cleanup",
            description: "清理失效缓存",
            params: [
              {
                name: "scope",
                kind: "choice",
                required: false,
                description: "清理范围",
                choices: [{ value: "all", description: "清空全部缓存" }],
              },
            ],
          },
        ],
      },
    ],
  },
];

describe("command-params 结构化参数引擎", () => {
  it("空查询按规范分组排序，输入过滤词拍平，草稿态与运行态正确置灰并跳过首焦", () => {
    const draftCtx: CommandContext = { ...BASE_CTX, isDraft: true };
    const entries = buildCommandEntries("/", COMMANDS, undefined, draftCtx);
    expect(entries.map((e) => e.command.name)).toEqual([
      "export",
      "new",
      "goal",
      "autonomy",
      "permission",
      "ask",
      "compact-skills",
      "mcp",
    ]);
    // 只有每组第一项展示分组标题。
    expect(entries.filter((e) => e.showGroupHeading).map((e) => e.groupKey)).toEqual([
      "session",
      "mode",
      "context",
      "plugins",
    ]);
    // export 在草稿态不可用，首个可用命令为 new（下标 1），步进也自动跳过不可用项。
    expect(entries[0].disabledReason).toBeTruthy();
    expect(firstEnabledIndex(entries)).toBe(1);
    expect(stepEnabledIndex(entries, 1, 1)).toBe(3);

    // 过滤查询时拍平且高亮前缀匹配字符（含前导 `/` 偏移）。
    const searched = buildCommandEntries("/au", COMMANDS, undefined, BASE_CTX);
    expect(searched).toHaveLength(1);
    expect(searched[0].showGroupHeading).toBe(false);
    expect(searched[0].titleMatches).toEqual([1, 2]);
  });

  it("一级行尾指示不暴露原始语法，状态类附当前生效值", () => {
    expect(commandTrailing(COMMANDS[1], BASE_CTX)).toBeNull();
    expect(commandTrailing(COMMANDS[0], BASE_CTX)).toEqual({ kind: "param", required: false });
    expect(commandTrailing(COMMANDS[5], BASE_CTX)).toEqual({ kind: "param", required: true });
    expect(commandTrailing(COMMANDS[3], BASE_CTX)).toEqual({
      kind: "submenu",
      currentValue: "dont_ask",
    });
    expect(commandTrailing(COMMANDS[4], BASE_CTX)).toEqual({
      kind: "submenu",
      currentValue: "workspace",
    });
  });

  it("二级菜单构建一律填入输入框，展开 1 级子选项并标记当前值与 MCP 状态", () => {
    const goalSub = buildSubmenuItems(COMMANDS[2], BASE_CTX);
    expect(goalSub.flat.map((a) => [a.key, a.fillText, a.closeMenu])).toEqual([
      ["__default__", "/goal", true],
      ["__text__", "/goal ", false],
      ["choice:edit", "/goal edit ", false],
      ["choice:pause", "/goal pause", true],
      ["choice:clear", "/goal clear", true],
    ]);

    const mcpSub = buildSubmenuItems(COMMANDS[6], BASE_CTX);
    expect(mcpSub.flat.map((a) => [a.label, a.fillText, a.statusTone ?? null])).toEqual([
      [expect.any(String), "/mcp", null],
      ["reconnect", "/mcp reconnect", null],
      ["reconnect github", "/mcp reconnect github", "success"],
      ["reconnect local-db", "/mcp reconnect local-db", "destructive"],
    ]);

    const autonomySub = buildSubmenuItems(COMMANDS[3], BASE_CTX);
    const currentItem = autonomySub.flat.find((a) => a.current);
    expect(currentItem?.fillText).toBe("/autonomy dont_ask");
  });

  it("阶段二支持自由文本引导、多级子命令钻取、动态 MCP 列表与精确完成态放行", () => {
    // 必填自由文本：空参数给出 ghostPlaceholder 与 missingRequired。
    const askEmpty = resolveStage2("/ask ", 5, COMMANDS, undefined, BASE_CTX);
    expect(askEmpty?.kind).toBe("hint");
    if (askEmpty?.kind === "hint") {
      expect(askEmpty.missingRequired).toBe(true);
      expect(askEmpty.ghostPlaceholder).toBe("输入只读问题…");
    }

    // 多级钻取 `/goal edit ` 进入必填 objective 引导；输入文本后不再缺失。
    const goalEditEmpty = resolveStage2("/goal edit ", 11, COMMANDS, undefined, BASE_CTX);
    expect(goalEditEmpty?.kind).toBe("hint");
    if (goalEditEmpty?.kind === "hint") {
      expect(goalEditEmpty.commandPath).toBe("/goal edit");
      expect(goalEditEmpty.missingRequired).toBe(true);
      expect(goalEditEmpty.ghostPlaceholder).toBe("输入修改后的目标…");
    }

    // 混合命令 `/goal 新目标` 不命中子命令时切回非阻塞文本提示。
    const goalFreeText = resolveStage2("/goal 修复单测", 10, COMMANDS, undefined, BASE_CTX);
    expect(goalFreeText?.kind).toBe("hint");

    // 已完整输入唯一叶子子命令 `/goal pause` 时返回 null，不拦截回车执行。
    expect(resolveStage2("/goal pause", 11, COMMANDS, undefined, BASE_CTX)).toBeNull();

    // 多级动态钻取 `/mcp reconnect ` 列出实时 MCP server；无 server 时给出空态提示。
    const mcpServers = resolveStage2("/mcp reconnect ", 15, COMMANDS, undefined, BASE_CTX);
    expect(mcpServers?.kind).toBe("choices");
    if (mcpServers?.kind === "choices") {
      expect(mcpServers.items.map((item) => item.fillText)).toEqual([
        "/mcp reconnect github",
        "/mcp reconnect local-db",
      ]);
    }
    const mcpEmpty = resolveStage2("/mcp reconnect ", 15, COMMANDS, undefined, {
      ...BASE_CTX,
      mcpServers: [],
    });
    expect(mcpEmpty?.kind).toBe("hint");
    if (mcpEmpty?.kind === "hint") {
      expect(mcpEmpty.emptyDynamicNotice).toBeTruthy();
    }

    // `/compact-skills cleanup ` 钻取 `all`。
    const cleanup = resolveStage2("/compact-skills cleanup ", 24, COMMANDS, undefined, BASE_CTX);
    expect(cleanup?.kind).toBe("choices");
    if (cleanup?.kind === "choices") {
      expect(cleanup.items[0].fillText).toBe("/compact-skills cleanup all");
    }
  });

  it("命令胶囊收拢非自然语言参数（含多级子命令、路径与插件 usage），保留自然语言正文并支持编辑写回", () => {
    const loopCmd: SlashCommand = {
      name: "loop",
      description: "重复执行指令",
      usage: "<count> <command>",
      plugin: "demo",
    };
    const all = [...COMMANDS, loopCmd];

    // 非自然语言子命令进入胶囊；自由文本自然语言留在胶囊外。
    const goalPause = parseCommandChip("/goal pause", all, BASE_CTX);
    expect(goalPause?.tokens).toEqual(["goal", "pause"]);
    expect(goalPause?.trailingText).toBe("");
    expect(goalPause?.editable).toBe(true);

    const goalEdit = parseCommandChip("/goal edit 修复单测", all, BASE_CTX);
    expect(goalEdit?.tokens).toEqual(["goal", "edit"]);
    expect(goalEdit?.trailingText).toBe(" 修复单测");

    const goalText = parseCommandChip("/goal 修复单测", all, BASE_CTX);
    expect(goalText?.tokens).toEqual(["goal"]);
    expect(goalText?.trailingText).toBe(" 修复单测");

    // 键入中的参数 token 暂不吞入，避免逐字输入/退格时跳变。
    const goalTyping = parseCommandChip("/goal pause", all, BASE_CTX, 11);
    expect(goalTyping?.tokens).toEqual(["goal"]);
    expect(goalTyping?.trailingText).toBe(" pause");

    // 多级子命令、动态参数与路径参数全部收拢进胶囊。
    expect(parseCommandChip("/compact-skills cleanup all", all, BASE_CTX)?.tokens).toEqual([
      "compact-skills",
      "cleanup",
      "all",
    ]);
    expect(parseCommandChip("/mcp reconnect github", all, BASE_CTX)?.tokens).toEqual([
      "mcp",
      "reconnect",
      "github",
    ]);
    expect(parseCommandChip("/export ./notes/session.md", all, BASE_CTX)?.tokens).toEqual([
      "export",
      "./notes/session.md",
    ]);
    // 纯自然语言或无参命令不进入可编辑态。
    expect(parseCommandChip("/ask 这个函数做什么", all, BASE_CTX)).toMatchObject({
      tokens: ["ask"],
      trailingText: " 这个函数做什么",
      editable: false,
    });
    expect(parseCommandChip("/new", all, BASE_CTX)).toMatchObject({
      tokens: ["new"],
      editable: false,
    });

    // 插件命令 `/loop 3 echo pong`：`3` 进胶囊，` echo pong` 留作正文。
    const loopParsed = parseCommandChip("/loop 3 echo pong", all, BASE_CTX);
    expect(loopParsed?.tokens).toEqual(["loop", "3"]);
    expect(loopParsed?.trailingText).toBe(" echo pong");
    expect(loopParsed?.editable).toBe(true);
    expect(
      loopParsed && applyCommandChipEdit(loopParsed, "/loop 3 echo pong", ["5"], BASE_CTX),
    ).toEqual({
      text: "/loop 5 echo pong",
      caret: 17,
      expectsText: true,
    });

    // 混合命令切分支：edit ↔ 直接目标保留正文；切到无正文叶子动作时清理原正文。
    expect(
      goalEdit && applyCommandChipEdit(goalEdit, "/goal edit 修复单测", [""], BASE_CTX),
    ).toMatchObject({ text: "/goal 修复单测" });
    expect(
      goalEdit && applyCommandChipEdit(goalEdit, "/goal edit 修复单测", ["pause"], BASE_CTX),
    ).toMatchObject({ text: "/goal pause" });
    expect(goalPause && applyCommandChipEdit(goalPause, "/goal pause", ["edit"], BASE_CTX)).toEqual(
      { text: "/goal edit ", caret: 11, expectsText: true },
    );
  });
});
