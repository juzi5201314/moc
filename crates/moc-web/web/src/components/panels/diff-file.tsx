// 单文件 diff：工具详情、检查面板「变更」与计划版本对比共用，固定按 hunk 渲染统一视图。

import { ChevronRightIcon, FileCodeIcon } from "lucide-react";
import { useState } from "react";
import { cn } from "@/lib/utils";
import type { DiffLine, ToolFileDiff } from "@/protocol/types";

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

export function DiffStat({
  added,
  removed,
  className,
}: {
  added: number;
  removed: number;
  className?: string;
}) {
  return (
    <span className={cn("shrink-0 font-mono text-2xs tabular-nums", className)}>
      {added > 0 && <span className="text-diff-add-fg">+{added}</span>}
      {added > 0 && removed > 0 && " "}
      {removed > 0 && <span className="text-diff-del-fg">−{removed}</span>}
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

export function DiffBody({ file, maxHeight }: { file: ToolFileDiff; maxHeight?: number }) {
  return (
    <div
      className="overflow-auto bg-code py-1 font-mono text-xs leading-[20px]"
      style={maxHeight ? { maxHeight } : undefined}
    >
      {file.hunks.map((hunk, index) => (
        <div key={`${hunk.old_start}:${hunk.new_start}`}>
          {index > 0 && (
            <div className="my-1 border-y border-border-subtle bg-muted/50 px-3 py-0.5 text-2xs text-text-tertiary">
              @@ -{hunk.old_start} +{hunk.new_start} @@
            </div>
          )}
          <UnifiedHunk lines={hunk.lines} />
        </div>
      ))}
    </div>
  );
}

/** 会话目录下的路径转相对路径（兼容尾斜杠、根目录与 Windows 分隔符）。 */
function relativeToCwd(filePath: string, cwd: string | undefined): string {
  if (!cwd) return filePath;
  const trimmed = cwd.replace(/[\\/]+$/, "");
  if (!trimmed) {
    // 根目录 `/`
    return filePath.startsWith("/") ? filePath.slice(1) : filePath;
  }
  if (
    filePath.startsWith(trimmed) &&
    (filePath[trimmed.length] === "/" || filePath[trimmed.length] === "\\")
  ) {
    return filePath.slice(trimmed.length + 1);
  }
  return filePath;
}

/** 可折叠的单文件卡片：头部是路径与增删统计。 */
export function DiffFile({
  file,
  cwd,
  defaultOpen = true,
  maxHeight,
}: {
  file: ToolFileDiff;
  /** 会话目录：其下的文件显示相对路径，完整路径留在悬停提示。 */
  cwd?: string;
  defaultOpen?: boolean;
  maxHeight?: number;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const stats = diffStats(file);
  const path = relativeToCwd(file.path, cwd);
  const slash = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  const dir = slash >= 0 ? path.slice(0, slash + 1) : "";
  const name = slash >= 0 ? path.slice(slash + 1) : path;

  return (
    <section className="overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className="flex h-10 w-full items-center gap-2 px-3.5 text-left text-xs hover:bg-surface-hover"
        title={file.path}
      >
        <ChevronRightIcon
          className={cn(
            "size-3.5 shrink-0 text-text-tertiary transition-transform",
            open && "rotate-90",
          )}
        />
        <FileCodeIcon className="size-3.5 shrink-0 text-text-tertiary" />
        {/* 目录先截断、文件名完整保留：窄面板里末尾的文件名才是辨认依据。 */}
        <span className="flex min-w-0 flex-1 font-mono">
          <span className="min-w-0 truncate text-text-tertiary">{dir}</span>
          <span className="max-w-full shrink-0 truncate text-foreground">{name}</span>
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
