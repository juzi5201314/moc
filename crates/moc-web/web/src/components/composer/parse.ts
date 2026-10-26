// Composer 输入解析：纯函数，便于单测。规则对齐 Rust 侧：
// - `!` / `!!`：`moc-protocol` 的 `BashCommand::parse`
// - 资源前缀：`moc-tui` 的 `current_token_range` / `resource_prefix`
// - 模型 selector：`moc-tui` 的 `cycle_thinking_level`（`{id}:{level}`，按最后一个 `:` 拆）

import type { BashCommand } from "@/protocol/types";

/** 只识别开头的感叹号；先消费 `!!`，其余原样交给 bash。 */
export function parseBash(input: string): BashCommand | null {
  if (!input.startsWith("!")) return null;
  const rest = input.slice(1);
  if (rest.startsWith("!")) return { command: rest.slice(1), exclude_from_context: true };
  return { command: rest, exclude_from_context: false };
}

export type SlashCommand = { name: string; args: string };

/** `/name args…`；`//` 开头是资源 URI，不算命令。 */
export function parseSlash(input: string): SlashCommand | null {
  if (!input.startsWith("/") || input.startsWith("//")) return null;
  const match = /^\/(\S+)(?:\s+([\s\S]*))?$/.exec(input);
  if (!match?.[1]) return null;
  return { name: match[1], args: (match[2] ?? "").trim() };
}

export type TokenRange = { start: number; end: number };

/** 光标所在的空白分隔 token（不跨行）。 */
export function currentTokenRange(text: string, cursor: number): TokenRange {
  const before = text.slice(0, cursor);
  const lineStart = before.lastIndexOf("\n") + 1;
  const linePart = before.slice(lineStart);
  const lastSpace = Math.max(...[" ", "\t", "\n", "\r"].map((char) => linePart.lastIndexOf(char)));
  const start = lastSpace >= 0 ? lineStart + lastSpace + 1 : lineStart;
  const afterMatch = /\s/.exec(text.slice(cursor));
  const end = afterMatch ? cursor + afterMatch.index : text.length;
  return { start, end };
}

/** 光标处 token 是 `//`、以 `:/` 结尾或含 `://` 时返回前缀与替换范围。 */
export function resourcePrefix(
  text: string,
  cursor: number,
): { prefix: string; range: TokenRange } | null {
  const range = currentTokenRange(text, cursor);
  const prefix = text.slice(range.start, cursor);
  const isUri = prefix === "//" || prefix.endsWith(":/") || prefix.includes("://");
  return isUri ? { prefix, range } : null;
}

/** 用候选替换 token；`appendSpace` 时补一个空格。返回新文本与光标位置。 */
export function applyCompletion(
  text: string,
  range: TokenRange,
  value: string,
  appendSpace: boolean,
): { text: string; cursor: number } {
  const insert = appendSpace ? `${value} ` : value;
  return {
    text: text.slice(0, range.start) + insert + text.slice(range.end),
    cursor: range.start + insert.length,
  };
}

/**
 * 当前会话 model 字符串 → `{id, level}`。只有拆出来的前半段是已知模型、
 * 后半段是它的思考等级时才算带等级；否则整串当模型 id。
 */
export function parseSelector(
  model: string,
  levelsOf: (id: string) => string[] | undefined,
): { id: string; level: string | null } {
  const index = model.lastIndexOf(":");
  if (index > 0) {
    const id = model.slice(0, index);
    const level = model.slice(index + 1);
    if (levelsOf(id)?.includes(level)) return { id, level };
  }
  return { id: model, level: null };
}

export function buildSelector(id: string, level: string | null): string {
  return level ? `${id}:${level}` : id;
}
