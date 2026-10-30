// 计划正文渲染：给每个块标上源码行范围，供「悬停加批注」与选区换算行号使用。
//
// 这里直接用 react-markdown 而不是流式渲染器：批注锚点依赖 hast 的
// `node.position`，块级的起止行刚好就是计划源码的 1-based 行。

import type { ElementType, MouseEvent } from "react";
import { createContext, type ReactNode, useContext, useMemo } from "react";
import Markdown, { type Components, type ExtraProps } from "react-markdown";
import remarkGfm from "remark-gfm";

import { type PlanLineRange, rangeFromSelection } from "@/lib/plan-annotation";
import { cn } from "@/lib/utils";
import type { PlanAnnotation } from "@/protocol/types";

type Props = {
  markdown: string;
  annotations?: PlanAnnotation[];
  /** 点击某个块左侧的「＋」时给出该块的源码行范围。 */
  onAnnotate?: (range: PlanLineRange) => void;
  /** 在正文里拖选后松手时给出选区对应的源码行范围。 */
  onSelectText?: (range: PlanLineRange) => void;
  className?: string;
};

type BlockProps = ExtraProps & { children?: ReactNode; className?: string };

type AnnotateContextValue = {
  covered: number[];
  onAnnotate?: (range: PlanLineRange) => void;
};

const AnnotateContext = createContext<AnnotateContextValue>({ covered: [] });

function sourceRange(node: ExtraProps["node"]): PlanLineRange | null {
  const start = node?.position?.start.line;
  const end = node?.position?.end.line;
  if (start === undefined || end === undefined) return null;
  return { startLine: start, endLine: end };
}

/** 块级渲染：保留原标签与属性，附带行范围、批注标记与悬停「＋」。 */
function Annotated({
  tag,
  node,
  children,
  className,
  ...props
}: BlockProps & { tag: ElementType }) {
  const { covered, onAnnotate } = useContext(AnnotateContext);
  const range = sourceRange(node);
  const annotated =
    range !== null && covered.some((line) => line >= range.startLine && line <= range.endLine);
  const Tag = tag;
  return (
    <Tag
      {...props}
      className={cn(
        "group/plan relative",
        className,
        annotated && "border-l-2 border-amber-500/70 pl-2",
      )}
      data-line-start={range?.startLine}
      data-line-end={range?.endLine}
    >
      {onAnnotate && range && (
        <button
          type="button"
          aria-label={`在第 ${range.startLine}-${range.endLine} 行批注`}
          title={`批注第 ${range.startLine}-${range.endLine} 行`}
          onClick={(event: MouseEvent<HTMLButtonElement>) => {
            event.preventDefault();
            event.stopPropagation();
            onAnnotate(range);
          }}
          className="absolute top-0 -left-6 hidden size-5 items-center justify-center rounded-full border border-border bg-background text-[11px] leading-none text-muted-foreground shadow-sm group-hover/plan:flex hover:text-foreground"
        >
          ＋
        </button>
      )}
      {children}
    </Tag>
  );
}

const components = {
  h1: (props: BlockProps) => <Annotated tag="h1" {...props} />,
  h2: (props: BlockProps) => <Annotated tag="h2" {...props} />,
  h3: (props: BlockProps) => <Annotated tag="h3" {...props} />,
  h4: (props: BlockProps) => <Annotated tag="h4" {...props} />,
  h5: (props: BlockProps) => <Annotated tag="h5" {...props} />,
  h6: (props: BlockProps) => <Annotated tag="h6" {...props} />,
  p: (props: BlockProps) => <Annotated tag="p" {...props} />,
  li: (props: BlockProps) => <Annotated tag="li" {...props} />,
  blockquote: (props: BlockProps) => <Annotated tag="blockquote" {...props} />,
  pre: (props: BlockProps) => <Annotated tag="pre" {...props} />,
  table: (props: BlockProps) => <Annotated tag="table" {...props} />,
  tr: (props: BlockProps) => <Annotated tag="tr" {...props} />,
} as unknown as Components;

export function PlanMarkdown({
  markdown,
  annotations = [],
  onAnnotate,
  onSelectText,
  className,
}: Props) {
  const covered = useMemo(() => {
    const lines: number[] = [];
    for (const annotation of annotations) {
      for (
        let line = annotation.start_line;
        line <= annotation.end_line && lines.length < 4096;
        line += 1
      ) {
        lines.push(line);
      }
    }
    return lines;
  }, [annotations]);

  const context = useMemo<AnnotateContextValue>(
    () => ({ covered, onAnnotate }),
    [covered, onAnnotate],
  );

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: 只把文字选区换算成源码行
    <div
      className={cn(
        "plan-markdown text-sm leading-relaxed select-text",
        "[&_h1]:mt-4 [&_h1]:mb-2 [&_h1]:text-lg [&_h1]:font-semibold",
        "[&_h2]:mt-4 [&_h2]:mb-2 [&_h2]:text-base [&_h2]:font-semibold",
        "[&_h3]:mt-3 [&_h3]:mb-1.5 [&_h3]:text-sm [&_h3]:font-semibold",
        "[&_p]:my-2 [&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-5",
        "[&_li]:my-0.5 [&_blockquote]:my-2 [&_blockquote]:border-l-2 [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground",
        "[&_pre]:my-2 [&_pre]:overflow-x-auto [&_pre]:rounded-md [&_pre]:bg-muted [&_pre]:p-3 [&_pre]:text-[11px]",
        "[&_code]:rounded [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-[11px]",
        "[&_table]:my-2 [&_table]:w-full [&_table]:border-collapse [&_td]:border-b [&_td]:border-border [&_td]:px-3 [&_td]:py-1.5 [&_th]:border-b [&_th]:border-border-strong [&_th]:px-3 [&_th]:py-1.5 [&_th]:text-left [&_th]:font-semibold [&_td:first-child]:pl-0 [&_th:first-child]:pl-0 [&_td:last-child]:pr-0 [&_th:last-child]:pr-0",
        className,
      )}
      onMouseUp={() => {
        if (!onSelectText) return;
        const range = rangeFromSelection(window.getSelection());
        if (range) onSelectText(range);
      }}
    >
      <AnnotateContext.Provider value={context}>
        <Markdown remarkPlugins={[remarkGfm]} components={components}>
          {markdown}
        </Markdown>
      </AnnotateContext.Provider>
    </div>
  );
}
