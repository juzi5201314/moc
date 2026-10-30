// 轮次重组：把扁平的 block 流切成「用户提示 → 按时间排列的片段」的轮次结构。
// 片段保留原始先后：连续的思考/工具/命令合成一个活动组，中间穿插的正文、
// 计划卡与告警各自成段；完成后的轮次只露出最后一段正文，其余收进「处理过程」。

import type { Block } from "@/lib/transcript-model";

type UserBlock = Extract<Block, { kind: "user" }>;
type TextBlock = Extract<Block, { kind: "text" }>;
type FooterBlock = Extract<Block, { kind: "footer" }>;
type ToolBlock = Extract<Block, { kind: "tool" }>;
type PlanBlock = Extract<Block, { kind: "plan" }>;
type StatusBlock = Extract<Block, { kind: "status" }>;

export type Segment =
  | { kind: "activity"; id: string; blocks: Block[] }
  | { kind: "text"; id: string; block: TextBlock }
  | { kind: "plan"; id: string; block: PlanBlock }
  | { kind: "notice"; id: string; block: StatusBlock };

export type Turn = {
  /** 轮次 id：优先用用户块，续接轮（开头没有用户块）退回首个块的 id。 */
  id: string;
  user?: UserBlock;
  /** 思考 / 命令 / 工具 / 状态 / 计划等非正文块，按原顺序。 */
  activity: Block[];
  texts: TextBlock[];
  /** 按时间排列的渲染片段。 */
  segments: Segment[];
  footer?: FooterBlock;
};

function pushSegment(turn: Turn, block: Block): void {
  if (block.kind === "text") {
    turn.segments.push({ kind: "text", id: block.id, block });
    return;
  }
  if (block.kind === "plan") {
    turn.segments.push({ kind: "plan", id: block.id, block });
    return;
  }
  // 告警与错误不藏进折叠组：用户必须一眼看到。
  if (block.kind === "status" && block.tone !== "info") {
    turn.segments.push({ kind: "notice", id: block.id, block });
    return;
  }
  const last = turn.segments.at(-1);
  if (last?.kind === "activity") last.blocks.push(block);
  else turn.segments.push({ kind: "activity", id: block.id, blocks: [block] });
}

export function groupTurns(blocks: Block[]): Turn[] {
  const turns: Turn[] = [];
  let current: Turn | null = null;

  for (const block of blocks) {
    if (block.kind === "user") {
      current = { id: block.id, user: block, activity: [], texts: [], segments: [] };
      turns.push(current);
      continue;
    }
    if (!current) {
      current = { id: block.id, activity: [], texts: [], segments: [] };
      turns.push(current);
    }
    if (block.kind === "footer") {
      current.footer = block;
      continue;
    }
    if (block.kind === "text") current.texts.push(block);
    else current.activity.push(block);
    pushSegment(current, block);
  }

  return turns;
}

/**
 * 活动类别：执行轨迹的种类划分——行动词、行图标与组标题归纳全部以它为准。
 * `showcase` 是 moc 自有的展示类工具，归纳时并入 `tools`。
 */
export type ActivityKind =
  | "thinking"
  | "commands"
  | "code"
  | "read"
  | "readImage"
  | "write"
  | "edit"
  | "search"
  | "webSearch"
  | "webFetch"
  | "subagents"
  | "plan"
  | "questions"
  | "tools"
  | "showcase";

/** 工具名 → 活动类别。覆盖 MCP 工具前缀（`mcp__fs__read_file`）与驼峰写法。 */
function toolKind(name: string): ActivityKind {
  const normalized = name.toLowerCase();
  if (normalized === "run_code") return "code";
  if (/(^|_)(web_search|websearch|search_web|google|bing|tavily|brave)(_|$)/.test(normalized)) {
    return "webSearch";
  }
  if (/(^|_)(web_fetch|webfetch|fetch|http_get|open_url|visit)(_|$)/.test(normalized)) {
    return "webFetch";
  }
  if (/(^|_)(bash|shell|terminal|exec|command|run|sh)(_|$)/.test(normalized)) return "commands";
  if (
    /(^|_)(subagent|task|workflow|agent|spawn|delegate|dispatch|sidekick)(_|$)/.test(normalized)
  ) {
    return "subagents";
  }
  if (/(^|_)(read|view|capture)(_|$)/.test(normalized)) {
    return normalized.includes("image") ? "readImage" : "read";
  }
  if (/(^|_)(write|create|save)(_|$)/.test(normalized)) return "write";
  if (/(^|_)(edit|str_replace|apply_patch|patch|replace|insert|multi_edit)(_|$)/.test(normalized)) {
    return "edit";
  }
  if (/(^|_)(plan|update_plan)(_|$)/.test(normalized)) return "plan";
  if (/(^|_)(ask|question|clarify)(_|$)/.test(normalized)) return "questions";
  if (/(^|_)(search|grep|glob|find|query|rg|list)(_|$)/.test(normalized)) return "search";
  if (/(^|_)(present|render|image|screenshot|preview|artifact|display)(_|$)/.test(normalized)) {
    return "showcase";
  }
  return "tools";
}

/** 单个活动块的类别；状态更新等非动作块不计入归纳。 */
export function activityKind(block: Block): ActivityKind | null {
  switch (block.kind) {
    case "reasoning":
      return "thinking";
    case "bash":
      return "commands";
    case "plan":
      return "plan";
    case "agent-message":
      return "subagents";
    case "tool":
      return toolKind((block as ToolBlock).name);
    default:
      return null;
  }
}

/** 活动行首的动作名（进行式去掉「正在」前缀的形态）。 */
const ROW_VERB: Record<ActivityKind, string> = {
  thinking: "思考",
  commands: "运行命令",
  code: "运行代码",
  read: "读取文件",
  readImage: "读取图片",
  write: "写入文件",
  edit: "编辑文件",
  search: "搜索代码",
  webSearch: "搜索网页",
  webFetch: "访问网页",
  subagents: "协调子智能体",
  plan: "更新计划",
  questions: "提问",
  tools: "调用工具",
  showcase: "展示过程",
};

/** 单个活动块的动作名。 */
export function activityVerb(block: Block): string | null {
  if (block.kind === "status") return "状态更新";
  const kind = activityKind(block);
  return kind ? ROW_VERB[kind] : null;
}

/** 分组标题的完成式文案。 */
const DONE_TITLE: Record<ActivityKind, string> = {
  thinking: "已完成分析",
  commands: "执行了命令",
  code: "运行了代码",
  read: "已读取文件",
  readImage: "已读取图片",
  write: "已写入文件",
  edit: "修改了文件",
  search: "已搜索代码",
  webSearch: "已搜索网页",
  webFetch: "已访问网页",
  subagents: "已协调子智能体",
  plan: "更新了计划",
  questions: "向用户提出了问题",
  tools: "已调用工具",
  showcase: "已调用工具",
};

/** 分组标题的进行式文案。 */
const PRESENT_TITLE: Record<ActivityKind, string> = {
  thinking: "正在分析请求",
  commands: "正在运行命令",
  code: "正在运行代码",
  read: "正在读取文件",
  readImage: "正在读取图片",
  write: "正在写入文件",
  edit: "正在编辑文件",
  search: "正在搜索代码",
  webSearch: "正在搜索网页",
  webFetch: "正在访问网页",
  subagents: "正在协调子智能体",
  plan: "正在更新计划",
  questions: "等待你的操作",
  tools: "正在调用工具",
  showcase: "正在调用工具",
};

/**
 * 归纳本轮活动，产出组标题。完成态按类别频次降序取前三拼接：
 * 两类用「A并B」、三类以上用「，」并列、相邻「已」前缀只保留一个、
 * 类别总数超过三时补「等」；进行中则只取最近动作的进行式。
 * 纯思考/无可归纳动作时退回「已完成分析」/「正在分析请求」。
 */
export function summarizeActivity(blocks: Block[], done = true): string {
  const titles = done ? DONE_TITLE : PRESENT_TITLE;
  if (!done) {
    for (let index = blocks.length - 1; index >= 0; index--) {
      const kind = activityKind(blocks[index]);
      if (kind && kind !== "thinking") return titles[kind === "showcase" ? "tools" : kind];
    }
    return titles.thinking;
  }
  const counts = new Map<ActivityKind, number>();
  for (const block of blocks) {
    const kind = activityKind(block);
    if (!kind || kind === "thinking") continue;
    const counted = kind === "showcase" ? "tools" : kind;
    counts.set(counted, (counts.get(counted) ?? 0) + 1);
  }
  const labels = [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, 3)
    .map(([kind]) => titles[kind]);
  const first = labels[0];
  if (!first) return titles.thinking;
  const second = labels[1];
  if (!second) return first;
  if (labels.length === 2) {
    const stripped = first.startsWith("已") && second.startsWith("已") ? second.slice(1) : second;
    return `${first}并${stripped}`;
  }
  const title = labels.join("，");
  return counts.size > 3 ? `${title}等` : title;
}

/** 用时：`23秒` / `12分17秒`——总秒数向下取整。 */
export function formatDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return minutes > 0 ? `${minutes}分${seconds}秒` : `${seconds}秒`;
}

/**
 * 一轮的耗时。实时轮带 footer.durationMs；回放历史会话没有这一项，
 * 用 footer 与首个带时间戳的块（用户块优先）推算。
 */
export function turnDurationMs(turn: Turn): number | undefined {
  if (turn.footer?.durationMs !== undefined) return turn.footer.durationMs;
  if (!turn.footer) return undefined;
  let startedAt = turn.user?.atMs;
  if (startedAt === undefined) {
    for (const block of turn.activity) {
      if ("atMs" in block) {
        startedAt = block.atMs;
        break;
      }
    }
  }
  return startedAt === undefined ? undefined : Math.max(1000, turn.footer.atMs - startedAt);
}

/** 从工具参数里挑一个最能说明动作的字段；description 优先（工具侧的语义化行标题）。 */
function argumentSummary(raw: string): string {
  if (!raw) return "";
  try {
    const args = JSON.parse(raw) as Record<string, unknown>;
    for (const key of ["description", "command", "cmd", "path", "file_path", "query", "pattern"]) {
      const value = args[key];
      if (typeof value === "string" && value.trim()) return value;
    }
    const first = Object.values(args).find((value) => typeof value === "string");
    return typeof first === "string" ? first : "";
  } catch {
    // 回放历史时 arguments 可能被预算截断成非法 JSON，退回正则捞关键字段。
    const match =
      /"(?:description|command|cmd|path|file_path|query|pattern)"\s*:\s*"((?:[^"\\]|\\.)*)"/.exec(
        raw,
      );
    return match?.[1] ?? raw;
  }
}

/** 活动行的一行摘要文本：思考取正文片段，命令取命令行，工具取参数摘要。 */
export function activitySnippet(block: Block, limit = 80): string {
  const clip = (text: string): string => {
    const flat = text.replace(/\s+/g, " ").trim();
    return flat.length > limit ? `${flat.slice(0, limit)}…` : flat;
  };

  switch (block.kind) {
    case "reasoning":
      return clip(block.text);
    case "bash":
      return clip(block.command);
    case "tool": {
      const tool = block as ToolBlock;
      const args = argumentSummary(tool.arguments);
      if (args) return clip(args);
      return clip(tool.result?.split("\n")[0] ?? "");
    }
    case "plan":
      return block.path;
    case "agent-message":
      return clip(block.body);
    case "status":
      return clip(block.text);
    default:
      return "";
  }
}
