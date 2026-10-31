// 计划正文渲染：给每个块标上源码行范围，供「悬停加批注」与选区换算行号使用。
//
// 这里直接用 react-markdown 而不是流式渲染器：批注锚点依赖 hast 的
// `node.position`，块级的起止行刚好就是计划源码的 1-based 行。
// 排版复用对话正文的 `.moc-prose`：各元素打上与 Streamdown 相同的 `data-streamdown`
// 标记，标题/列表/表格/代码的样式就只有一份。

import type { CSSProperties, ElementType, MouseEvent } from "react";
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
        annotated && "-ml-2.5 border-l-2 border-brand/60 pl-2",
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
          className="absolute top-0.5 -left-6 hidden size-5 items-center justify-center rounded-full bg-popover text-2xs leading-none text-muted-foreground shadow-[var(--elevation-popover)] group-hover/plan:flex hover:text-foreground"
        >
          ＋
        </button>
      )}
      {children}
    </Tag>
  );
}

/** 块级标签 → 对应的 Streamdown 标记；没有块级批注需求的行内元素只打标记。 */
const block = (tag: ElementType, name?: string) => (props: BlockProps) => (
  <Annotated tag={tag} data-streamdown={name} {...props} />
);
const mark =
  (Tag: ElementType, name: string) =>
  ({ node: _node, ...props }: BlockProps) => <Tag data-streamdown={name} {...props} />;

// pre 内的 code 是代码块正文，不能套行内代码的灰底
const InPre = createContext(false);

function Code({ node: _node, ...props }: BlockProps) {
  const inPre = useContext(InPre);
  return <code data-streamdown={inPre ? undefined : "inline-code"} {...props} />;
}

// 代码块自带 overflow:hidden，批注外壳要套在外面，否则悬停「＋」会被裁掉；
// 外壳已经吃掉块间距，内层清掉 .moc-prose 给代码块的外边距。
function Pre({ children, ...props }: BlockProps) {
  return (
    <Annotated tag="div" {...props}>
      <div data-streamdown="code-block" style={{ margin: 0 }}>
        <pre data-streamdown="code-block-body" className="overflow-x-auto">
          <InPre.Provider value={true}>{children}</InPre.Provider>
        </pre>
      </div>
    </Annotated>
  );
}

const components = {
  h1: block("h1", "heading-1"),
  h2: block("h2", "heading-2"),
  h3: block("h3", "heading-3"),
  h4: block("h4", "heading-4"),
  h5: block("h5", "heading-5"),
  h6: block("h6", "heading-6"),
  p: block("p"),
  ul: mark("ul", "unordered-list"),
  ol: mark("ol", "ordered-list"),
  li: block("li", "list-item"),
  blockquote: block("blockquote", "blockquote"),
  pre: Pre,
  code: Code,
  a: mark("a", "link"),
  strong: mark("strong", "strong"),
  hr: mark("hr", "horizontal-rule"),
  table: block("table", "table"),
  thead: mark("thead", "table-header"),
  th: mark("th", "table-header-cell"),
  td: mark("td", "table-cell"),
  tr: block("tr"),
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
      // 侧栏比对话列窄，正文降一级字号
      style={{ "--moc-prose-size": "13.5px" } as CSSProperties}
      className={cn("plan-markdown moc-prose select-text", className)}
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
