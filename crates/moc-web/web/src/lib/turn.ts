// 轮次重组：把扁平的 block 流切成"用户提示 → 活动流 → 正文"的轮次结构。
// 消息流按 DSH 的三级折叠渲染：轮次摘要 → 活动分组 → 单条活动行，
// 分组标题由本轮实际出现的动作类型现场归纳，不需要协议侧提供字段。

import type { Block } from "@/lib/transcript-model";

type UserBlock = Extract<Block, { kind: "user" }>;
type TextBlock = Extract<Block, { kind: "text" }>;
type FooterBlock = Extract<Block, { kind: "footer" }>;
type ToolBlock = Extract<Block, { kind: "tool" }>;

export interface Turn {
  /** 轮次 id：优先用用户块，续接轮（开头没有用户块）退回首个块的 id。 */
  id: string;
  user?: UserBlock;
  /** 思考 / 命令 / 工具 / 状态 / 计划等非正文块，按原顺序。 */
  activity: Block[];
  texts: TextBlock[];
  footer?: FooterBlock;
}

/** 活动块是否需要在折叠区里占一行。 */
function isActivityBlock(block: Block): boolean {
  return block.kind !== "user" && block.kind !== "text" && block.kind !== "footer";
}

export function groupTurns(blocks: Block[]): Turn[] {
  const turns: Turn[] = [];
  let current: Turn | null = null;

  for (const block of blocks) {
    if (block.kind === "user") {
      current = { id: block.id, user: block, activity: [], texts: [] };
      turns.push(current);
      continue;
    }
    if (!current) {
      current = { id: block.id, activity: [], texts: [] };
      turns.push(current);
    }
    const turn = current;
    if (block.kind === "text") turn.texts.push(block);
    else if (block.kind === "footer") turn.footer = block;
    else if (isActivityBlock(block)) turn.activity.push(block);
  }

  return turns;
}

/** 工具名 → 中文动作名。覆盖 MCP 工具前缀（`mcp__fs__read_file`）与驼峰写法。 */
function toolVerb(name: string): string {
  const normalized = name.toLowerCase();
  if (/(^|_)(bash|shell|terminal|exec|command|run)(_|$)/.test(normalized)) return "运行命令";
  if (/(^|_)(read|view|capture)(_|$)/.test(normalized)) return "读取文件";
  if (/(^|_)(write|edit|str_replace|apply_patch|create|insert)(_|$)/.test(normalized)) {
    return "编辑文件";
  }
  if (/(^|_)(search|grep|glob|find|query)(_|$)/.test(normalized)) return "搜索";
  if (/(^|_)(present|render|image|screenshot)(_|$)/.test(normalized)) return "展示过程";
  if (/(^|_)(subagent|task|workflow|agent)(_|$)/.test(normalized)) return "调度子代理";
  return "调用工具";
}

/** 单个活动块的动作名。 */
export function activityVerb(block: Block): string | null {
  switch (block.kind) {
    case "reasoning":
      return "思考";
    case "bash":
      return "运行命令";
    case "tool":
      return toolVerb((block as ToolBlock).name);
    case "plan":
      return "提交计划";
    case "agent-message":
      return "子代理消息";
    case "status":
      return "状态更新";
    default:
      return null;
  }
}

/**
 * 归纳本轮活动：「运行命令、读取文件」这种。
 * 按出现顺序去重，超过三类截断为「A、B、C 等」，与 DSH 的 "Ran commands and read files" 对齐。
 */
export function summarizeActivity(blocks: Block[], limit = 3): string {
  const seen = new Set<string>();
  const verbs: string[] = [];
  for (const block of blocks) {
    const verb = activityVerb(block);
    if (!verb || seen.has(verb)) continue;
    seen.add(verb);
    verbs.push(verb);
  }
  if (verbs.length === 0) return "";
  if (verbs.length <= limit) return verbs.join("、");
  return `${verbs.slice(0, limit).join("、")} 等`;
}

/** 用时：`23秒` / `12分17秒`，与输入框下的实时计时保持同一口径。 */
export function formatDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return minutes > 0 ? `${minutes}分${seconds}秒` : `${seconds}秒`;
}

/**
 * 一轮的耗时。只有实时轮带 footer.durationMs；回放历史会话时协议没有记录这一项，
 * 因此返回 undefined，由调用方退化成只显示「已完成」。
 */
export function turnDurationMs(turn: Turn): number | undefined {
  return turn.footer?.durationMs;
}

/** 从工具参数里挑一个最能说明动作的字段。 */
function argumentSummary(raw: string): string {
  if (!raw) return "";
  try {
    const args = JSON.parse(raw) as Record<string, unknown>;
    for (const key of ["command", "cmd", "path", "file_path", "query", "pattern", "description"]) {
      const value = args[key];
      if (typeof value === "string" && value.trim()) return value;
    }
    const first = Object.values(args).find((value) => typeof value === "string");
    return typeof first === "string" ? first : "";
  } catch {
    return raw;
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
