// 单文件 diff：工具详情、检查面板「变更」与计划版本对比共用。
// 统一视图按 hunk 排列；并排视图把相邻的删/增行配对到左右两栏。

import { ChevronRightIcon, FileCodeIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { toSplitRows } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { DiffLine, ToolFileDiff } from "@/protocol/types";
import { type DiffMode, usePrefs } from "@/store/prefs";

export function diffStats(file: ToolFileDiff): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const hunk of file.hunks) {
    for (const line of hunk.lines) {
      if (line.kind === "insert") added++;
      else if (line.kind === "delete") removed++;
    }
  }
  return { added, removed };
}

export function DiffStat({ added, removed }: { added: number; removed: number }) {
  return (
    <span className="shrink-0 font-mono text-[11px] tabular-nums">
      {added > 0 && <span className="text-success">+{added}</span>}
      {added > 0 && removed > 0 && " "}
      {removed > 0 && <span className="text-destructive">-{removed}</span>}
    </span>
  );
}

const LINE_TONE: Record<DiffLine["kind"], string> = {
  insert: "bg-diff-insert",
  delete: "bg-diff-delete",
  equal: "",
};

const SIGN: Record<DiffLine["kind"], string> = { insert: "+", delete: "-", equal: " " };

// core 保证：delete/equal 必有 old_no，insert/equal 必有 new_no，同一 hunk 内二者组合唯一。
const lineKey = (line?: DiffLine) => `${line?.old_no ?? ""}:${line?.new_no ?? ""}`;

function SplitCell({ cell }: { cell?: DiffLine }) {
  return (
    <div
      className={cn("min-w-0 overflow-hidden whitespace-pre px-3", cell && LINE_TONE[cell.kind])}
    >
      {cell?.text || " "}
    </div>
  );
}

function UnifiedHunk({ lines }: { lines: DiffLine[] }) {
  return (
    <>
      {lines.map((line) => (
        <div
          key={lineKey(line)}
          className={cn(
            "grid min-w-max grid-cols-[2.75rem_2.75rem_1.25rem_1fr]",
            LINE_TONE[line.kind],
          )}
        >
          <span className="select-none pr-2 text-right text-text-tertiary/70">
            {line.old_no ?? ""}
          </span>
          <span className="select-none pr-2 text-right text-text-tertiary/70">
            {line.new_no ?? ""}
          </span>
          <span
            className={cn(
              "select-none text-center",
              line.kind === "insert"
                ? "text-success"
                : line.kind === "delete"
                  ? "text-destructive"
                  : "text-text-tertiary/50",
            )}
          >
            {SIGN[line.kind]}
          </span>
          <span className="whitespace-pre pr-4">{line.text || " "}</span>
        </div>
      ))}
    </>
  );
}

function SplitHunk({ lines }: { lines: DiffLine[] }) {
  const rows = useMemo(() => toSplitRows(lines), [lines]);
  return (
    <>
      {rows.map((row) => (
        <div
          key={`${lineKey(row.left)}|${lineKey(row.right)}`}
          className="grid grid-cols-2 divide-x divide-border-subtle"
        >
          <SplitCell cell={row.left} />
          <SplitCell cell={row.right} />
        </div>
      ))}
    </>
  );
}

export function DiffBody({
  file,
  mode,
  maxHeight,
}: {
  file: ToolFileDiff;
  mode?: DiffMode;
  maxHeight?: number;
}) {
  const preferred = usePrefs((state) => state.diffMode);
  const effective = mode ?? preferred;
  return (
    <div
      className="overflow-auto bg-code py-1 font-mono text-[12.5px] leading-[20px]"
      style={maxHeight ? { maxHeight } : undefined}
    >
      {file.hunks.map((hunk, index) => (
        <div key={`${hunk.old_start}:${hunk.new_start}`}>
          {index > 0 && (
            <div className="my-1 border-y border-border-subtle bg-muted/50 px-3 py-0.5 text-[11px] text-text-tertiary">
              @@ -{hunk.old_start} +{hunk.new_start} @@
            </div>
          )}
          {effective === "split" ? (
            <SplitHunk lines={hunk.lines} />
          ) : (
            <UnifiedHunk lines={hunk.lines} />
          )}
        </div>
      ))}
    </div>
  );
}

/** 可折叠的单文件卡片：头部是路径与增删统计。 */
export function DiffFile({
  file,
  defaultOpen = true,
  maxHeight,
}: {
  file: ToolFileDiff;
  defaultOpen?: boolean;
  maxHeight?: number;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const stats = diffStats(file);
  const slash = file.path.lastIndexOf("/");
  const dir = slash >= 0 ? file.path.slice(0, slash + 1) : "";
  const name = slash >= 0 ? file.path.slice(slash + 1) : file.path;

  return (
    <section className="overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className="flex h-10 w-full items-center gap-2 px-3.5 text-left text-[12.5px] hover:bg-surface-hover"
        title={file.path}
      >
        <ChevronRightIcon
          className={cn(
            "size-3.5 shrink-0 text-text-tertiary transition-transform",
            open && "rotate-90",
          )}
        />
        <FileCodeIcon className="size-3.5 shrink-0 text-text-tertiary" />
        <span className="min-w-0 flex-1 truncate font-mono">
          <span className="text-text-tertiary">{dir}</span>
          <span className="text-foreground">{name}</span>
        </span>
        <DiffStat {...stats} />
      </button>
      {open && (
        <div className="border-t border-border-subtle">
          <DiffBody file={file} maxHeight={maxHeight} />
        </div>
      )}
    </section>
  );
}
