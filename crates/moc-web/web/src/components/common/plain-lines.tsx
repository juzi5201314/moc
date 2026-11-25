// 纯文本逐行渲染与 Markdown 硬换行原语：
// 让行内自动折行保持紧凑，显式换行（\n / <br>）拉开额外垂直间距以增强可读性。

import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

const LINE_BREAK = /\r\n|\r|\n/;

/** 把混合了纯文本与行内元素（如 RefChip）的节点序列按显式换行切成逐行二维数组。 */
export function splitInlineNodesByLine(nodes: readonly ReactNode[]): ReactNode[][] {
  const lines: ReactNode[][] = [[]];
  for (const node of nodes) {
    if (typeof node === "string") {
      const parts = node.split(LINE_BREAK);
      for (let index = 0; index < parts.length; index++) {
        if (index > 0) lines.push([]);
        if (parts[index].length > 0) lines[lines.length - 1].push(parts[index]);
      }
    } else {
      lines[lines.length - 1].push(node);
    }
  }
  // 与 CSS `white-space: pre-wrap` 一致：末尾单个换行只结束末行，不生出额外空白块。
  if (lines.length > 1 && lines[lines.length - 1].length === 0) {
    lines.pop();
  }
  return lines;
}

/** 逐行渲染切分好的行内节点；空行保留 `<br />` 以确保拖选复制还原出空行。 */
export function LineBlocks({
  lines,
  className,
}: {
  lines: readonly (readonly ReactNode[])[];
  className?: string;
}) {
  return (
    <div className={cn("moc-text-lines", className)}>
      {lines.map((line, index) => (
        <div
          // biome-ignore lint/suspicious/noArrayIndexKey: 逐行只读切分，顺序稳定
          key={index}
          className={line.length === 0 ? "moc-text-line moc-text-line-empty" : "moc-text-line"}
        >
          {line.length === 0 ? <br /> : line}
        </div>
      ))}
    </div>
  );
}

/** 纯文本消息逐行排版：行内自动折行紧、显式换行松。 */
export function PlainLines({ text, className }: { text: string; className?: string }) {
  if (!text) return null;
  return <LineBlocks lines={splitInlineNodesByLine([text])} className={className} />;
}

/** Markdown 硬换行（`<br>`）：零宽行内块置于 `<br>` 之前向下撑开行尾间距，避开 `<br>` 后的源码换行空白。 */
export function MarkdownBreak() {
  return (
    <>
      <span aria-hidden="true" className="moc-br-strut" />
      <br />
    </>
  );
}
