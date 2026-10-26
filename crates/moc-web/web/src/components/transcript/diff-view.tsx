// 结构化 diff 渲染：数据来自 ToolDiff.files[].hunks[].lines。
// unified：单列，gutter 显示 old_no / new_no；split：成对的 delete/insert 放同一行左右两侧。
// `maxLines` 用于 brief 档位：超出部分折叠成「⋯ 还有 N 行」。

import { Copy } from "lucide-react";
import { toast } from "sonner";
import { cn } from "@/lib/utils";
import type { DiffLine, ToolDiff, ToolFileDiff } from "@/protocol/types";
import type { DiffMode } from "@/store/prefs";

export function diffStats(diff: ToolDiff): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const file of diff.files) {
    for (const hunk of file.hunks) {
      for (const line of hunk.lines) {
        if (line.kind === "insert") added++;
        else if (line.kind === "delete") removed++;
      }
    }
  }
  return { added, removed };
}

type SplitRow = { left?: DiffLine; right?: DiffLine };

/** 把一段连续的 delete 与紧随其后的 insert 两两配对。 */
export function toSplitRows(lines: DiffLine[]): SplitRow[] {
  const rows: SplitRow[] = [];
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.kind === "equal") {
      rows.push({ left: line, right: line });
      index++;
      continue;
    }
    const deletes: DiffLine[] = [];
    const inserts: DiffLine[] = [];
    while (index < lines.length && lines[index].kind === "delete") deletes.push(lines[index++]);
    while (index < lines.length && lines[index].kind === "insert") inserts.push(lines[index++]);
    const count = Math.max(deletes.length, inserts.length);
    for (let pair = 0; pair < count; pair++)
      rows.push({ left: deletes[pair], right: inserts[pair] });
  }
  return rows;
}

const LINE_BG: Record<DiffLine["kind"], string> = {
  equal: "",
  insert: "bg-diff-insert",
  delete: "bg-diff-delete",
};

const LINE_SIGN: Record<DiffLine["kind"], string> = { equal: " ", insert: "+", delete: "-" };

function Gutter({ value }: { value?: number }) {
  return (
    <span className="w-10 shrink-0 select-none pr-2 text-right text-muted-foreground/70">
      {value ?? ""}
    </span>
  );
}

function UnifiedLine({ line }: { line: DiffLine }) {
  return (
    <div className={cn("flex min-w-max", LINE_BG[line.kind])}>
      <Gutter value={line.old_no} />
      <Gutter value={line.new_no} />
      <span className="w-4 shrink-0 select-none text-muted-foreground">{LINE_SIGN[line.kind]}</span>
      <span className="whitespace-pre pr-3">{line.text}</span>
    </div>
  );
}

function SplitCell({ line }: { line?: DiffLine }) {
  if (!line) return <div className="min-w-0 flex-1 bg-muted/40" />;
  const number = line.kind === "insert" ? line.new_no : line.old_no;
  return (
    <div className={cn("flex min-w-0 flex-1 overflow-hidden", LINE_BG[line.kind])}>
      <Gutter value={line.kind === "equal" ? line.old_no : number} />
      <span className="w-4 shrink-0 select-none text-muted-foreground">{LINE_SIGN[line.kind]}</span>
      <span className="truncate whitespace-pre pr-2">{line.text}</span>
    </div>
  );
}

function FileHeader({ path }: { path: string }) {
  return (
    <div className="flex items-center gap-2 border-b bg-muted/50 px-3 py-1 font-sans text-xs">
      <span className="min-w-0 flex-1 truncate" title={path}>
        {path}
      </span>
      <button
        type="button"
        aria-label={`复制路径 ${path}`}
        className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
        onClick={() => {
          void navigator.clipboard.writeText(path).then(() => toast.success("已复制路径"));
        }}
      >
        <Copy className="size-3" aria-hidden />
      </button>
    </div>
  );
}

function FileBody({
  file,
  mode,
  budget,
}: {
  file: ToolFileDiff;
  mode: DiffMode;
  budget: { left: number };
}) {
  const lines = file.hunks.flatMap((hunk, hunkIndex) => [
    ...(hunkIndex > 0 ? [null] : []),
    ...hunk.lines,
  ]);
  const visible: (DiffLine | null)[] = [];
  for (const line of lines) {
    if (budget.left <= 0) break;
    visible.push(line);
    if (line) budget.left--;
  }
  if (mode === "split") {
    const segments: DiffLine[][] = [[]];
    for (const line of visible) {
      if (line) segments.at(-1)?.push(line);
      else segments.push([]);
    }
    return (
      <div>
        {segments.map((segment, segmentIndex) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: 段落顺序固定，只按位置区分
          <div key={segmentIndex} className={cn(segmentIndex > 0 && "border-t border-dashed")}>
            {toSplitRows(segment).map((row, rowIndex) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: diff 行没有稳定 id
              <div key={rowIndex} className="flex divide-x">
                <SplitCell line={row.left} />
                <SplitCell line={row.right} />
              </div>
            ))}
          </div>
        ))}
      </div>
    );
  }
  return (
    <div className="overflow-x-auto">
      {visible.map((line, index) =>
        line ? (
          // biome-ignore lint/suspicious/noArrayIndexKey: diff 行没有稳定 id
          <UnifiedLine key={index} line={line} />
        ) : (
          // biome-ignore lint/suspicious/noArrayIndexKey: 同上
          <div key={index} className="select-none px-3 text-muted-foreground">
            ⋯
          </div>
        ),
      )}
    </div>
  );
}

export function DiffView({
  diff,
  mode,
  maxLines,
  className,
}: {
  diff: ToolDiff;
  mode: DiffMode;
  maxLines?: number;
  className?: string;
}) {
  const total = diff.files.reduce(
    (sum, file) => sum + file.hunks.reduce((inner, hunk) => inner + hunk.lines.length, 0),
    0,
  );
  const budget = { left: maxLines ?? Number.POSITIVE_INFINITY };
  const hidden = maxLines !== undefined ? Math.max(0, total - maxLines) : 0;
  return (
    <div className={cn("overflow-hidden rounded-lg border font-mono text-xs leading-5", className)}>
      {diff.files.map((file) =>
        budget.left > 0 ? (
          <div key={file.path} className="border-b last:border-b-0">
            <FileHeader path={file.path} />
            <FileBody file={file} mode={mode} budget={budget} />
          </div>
        ) : null,
      )}
      {hidden > 0 ? (
        <div className="border-t px-3 py-1 font-sans text-muted-foreground">⋯ 还有 {hidden} 行</div>
      ) : null}
      {diff.truncated ? (
        <div className="border-t px-3 py-1 font-sans text-muted-foreground">
          其余改动超出 diff 上限，未显示
        </div>
      ) : null}
    </div>
  );
}
