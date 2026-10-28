// 计划批注的纯函数：建议围栏解析、选区→源码行映射、批注覆盖判断。
//
// 行号一律是计划源码的 1-based 行；与 TUI 共用同一套语义：协议不解析批注正文，
// `suggestion` 围栏只是前端给出「替换文本」的约定（GitHub PR 的写法）。

import type { PlanAnnotation } from "@/protocol/types";

/** 计划源码行；与 Rust `str::lines()` 对齐（末尾换行不多出一行）。 */
export function sourceLines(markdown: string): string[] {
  if (markdown.length === 0) return [];
  const body = markdown.endsWith("\n") ? markdown.slice(0, -1) : markdown;
  return body.split("\n");
}

/** 选区锚定的源码行范围。 */
export type PlanLineRange = { startLine: number; endLine: number };

/**
 * 从批注正文里拆出 `suggestion` 围栏：`note` 是栅栏外的意见，`suggestion` 是建议文本。
 * 没有围栏时返回 `null`。
 */
export function splitSuggestion(body: string): { note: string; suggestion: string } | null {
  const match = body.match(/```suggestion[^\n]*\n([\s\S]*?)\n?```/);
  if (!match) return null;
  const note = body.replace(match[0], "").trim();
  return { note, suggestion: match[1] };
}

/** 把建议改法写进 `suggestion` 围栏；已有围栏时原地替换。 */
export function appendSuggestion(body: string, replacement: string): string {
  const fence = `\`\`\`suggestion\n${replacement}\n\`\`\``;
  const existing = splitSuggestion(body);
  if (existing) {
    return body.replace(/```suggestion[^\n]*\n[\s\S]*?\n?```/, fence);
  }
  const note = body.trimEnd();
  return note.length > 0 ? `${note}\n\n${fence}` : fence;
}

/** 这一行（1-based）是否落在某条批注里。 */
export function annotationAt(
  annotations: PlanAnnotation[],
  line: number,
): PlanAnnotation | undefined {
  return annotations.find(
    (annotation) => annotation.start_line <= line && line <= annotation.end_line,
  );
}

/** 元素上记录的行范围（由 `data-line-start` / `data-line-end` 提供）。 */
export function blockRangeFrom(element: Element | null): PlanLineRange | null {
  const block = element?.closest<HTMLElement>("[data-line-start][data-line-end]");
  if (!block) return null;
  const startLine = Number(block.dataset.lineStart);
  const endLine = Number(block.dataset.lineEnd);
  if (!Number.isFinite(startLine) || !Number.isFinite(endLine)) return null;
  return { startLine, endLine };
}

/**
 * 把浏览器选区换算成计划源码行：取选区两端最近的锚点块，覆盖两块的整个范围。
 *
 * 渲染后的 markdown 无法逐字还原源码列位，块级范围是稳定且可核对的粒度。
 */
export function rangeFromSelection(selection: Selection | null): PlanLineRange | null {
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return null;
  const quote = selection.toString().trim();
  if (quote.length === 0) return null;
  const range = selection.getRangeAt(0);
  const start = blockRangeFrom(
    range.startContainer.nodeType === Node.ELEMENT_NODE
      ? (range.startContainer as Element)
      : range.startContainer.parentElement,
  );
  const end = blockRangeFrom(
    range.endContainer.nodeType === Node.ELEMENT_NODE
      ? (range.endContainer as Element)
      : range.endContainer.parentElement,
  );
  if (!start && !end) return null;
  if (!start || !end) {
    const only = (start ?? end) as PlanLineRange;
    return { startLine: only.startLine, endLine: only.endLine };
  }
  const startLine = Math.min(start.startLine, end.startLine);
  const endLine = Math.max(start.endLine, end.endLine);
  return { startLine, endLine };
}

/** 选区文本，供批注卡片回显原文。 */
export function selectionQuote(selection: Selection | null): string {
  return selection?.toString().trim() ?? "";
}
