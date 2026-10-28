// 单文件 diff 视图：被工具卡片与计划版本对比共用。

import type { ToolFileDiff } from "@/protocol/types";

export function DiffFile({ file }: { file: ToolFileDiff }) {
  const additions = file.hunks
    .flatMap((hunk) => hunk.lines)
    .filter((line) => line.kind === "insert").length;
  const deletions = file.hunks
    .flatMap((hunk) => hunk.lines)
    .filter((line) => line.kind === "delete").length;

  return (
    <section className="min-w-0 border-b border-border last:border-b-0">
      <header className="sticky top-0 z-10 flex items-center justify-between gap-3 border-b border-border/60 bg-background px-3 py-2 text-xs">
        <span className="truncate font-mono" title={file.path}>
          {file.path}
        </span>
        <span className="shrink-0 font-mono text-[10px]">
          <span className="text-emerald-600">+{additions}</span>{" "}
          <span className="text-rose-500">-{deletions}</span>
        </span>
      </header>
      <div className="overflow-x-auto py-1 font-mono text-[11px] leading-5">
        {file.hunks.map((hunk) => (
          <div key={`${hunk.old_start}:${hunk.new_start}`}>
            <div className="bg-muted/50 px-3 py-1 text-[10px] text-muted-foreground">
              @@ -{hunk.old_start} +{hunk.new_start} @@
            </div>
            {hunk.lines.map((line) => (
              <div
                key={`${line.old_no ?? "-"}:${line.new_no ?? "-"}:${line.kind}:${line.text}`}
                className={`grid min-w-max grid-cols-[3rem_3rem_1rem_minmax(0,1fr)] px-2 ${
                  line.kind === "insert"
                    ? "bg-emerald-500/10 text-emerald-800 dark:text-emerald-300"
                    : line.kind === "delete"
                      ? "bg-rose-500/10 text-rose-800 dark:text-rose-300"
                      : "text-foreground"
                }`}
              >
                <span className="select-none text-right text-muted-foreground/60">
                  {line.old_no ?? ""}
                </span>
                <span className="select-none text-right text-muted-foreground/60">
                  {line.new_no ?? ""}
                </span>
                <span className="select-none text-center text-muted-foreground/60">
                  {line.kind === "insert" ? "+" : line.kind === "delete" ? "-" : " "}
                </span>
                <span className="whitespace-pre pr-4">{line.text || " "}</span>
              </div>
            ))}
          </div>
        ))}
      </div>
    </section>
  );
}
