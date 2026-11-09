// 右侧检查面板：文件 / 终端 / 变更 / 旁路。
// 计数徽标只保留 changes；需要远端数据的分页只在打开时拉取。

import { Columns2Icon, RowsIcon, XIcon } from "lucide-react";
import { lazy, Suspense, useMemo } from "react";
import { IconButton } from "@/components/common/icon-button";
import { FilesTab } from "@/components/files/files-tab";
import { DiffFile } from "@/components/panels/diff-file";
import { cn } from "@/lib/utils";
import type { ToolFileDiff } from "@/protocol/types";
import { type InspectorTab, useLayout } from "@/store/layout";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { BtwTab, PanelEmpty } from "./inspector-tabs";

// 终端整页懒加载：xterm 依赖大，不进主路径。
const TerminalTab = lazy(() =>
  import("@/components/terminal/terminal-tab").then((module) => ({ default: module.TerminalTab })),
);

const TABS: { id: InspectorTab; label: string }[] = [
  { id: "files", label: "文件" },
  { id: "terminal", label: "终端" },
  { id: "changes", label: "变更" },
  { id: "btw", label: "旁路" },
];

export function InspectorPanel({ sessionId }: { sessionId: string }) {
  const tab = useLayout((state) => state.inspectorTab);
  const view = useTranscript((state) => state.bySession[sessionId]);

  const counts: Partial<Record<InspectorTab, number>> = {
    changes: new Set(view?.diffs.flatMap((record) => record.diff.files.map((file) => file.path)))
      .size,
  };

  return (
    <div className="flex h-full w-full min-w-0 flex-col bg-background text-foreground">
      <div className="flex h-12 shrink-0 items-center gap-1 border-b border-border-subtle pr-2 pl-2">
        <div className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none]">
          {TABS.map((item) => {
            const count = counts[item.id] ?? 0;
            return (
              <button
                key={item.id}
                type="button"
                onClick={() => useLayout.getState().openInspector(item.id)}
                className={cn(
                  "inline-flex h-8 shrink-0 items-center gap-1.5 rounded-lg px-2.5 text-sm transition-colors",
                  tab === item.id
                    ? "bg-accent font-medium text-foreground"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                {item.label}
                {count > 0 && (
                  <span className="min-w-4 rounded-full bg-foreground/10 px-1 text-center text-2xs leading-4 tabular-nums text-muted-foreground">
                    {count}
                  </span>
                )}
              </button>
            );
          })}
        </div>
        <IconButton
          label="关闭面板"
          shortcut="Alt R"
          size="sm"
          onClick={() => useLayout.getState().toggleRightbar(false)}
        >
          <XIcon />
        </IconButton>
      </div>

      {/* files/terminal/btw 各自管理内部滚动，只有 changes 用外层滚动 */}
      <div
        className={cn("min-h-0 flex-1", tab === "changes" ? "overflow-y-auto" : "flex flex-col")}
      >
        {tab === "files" && <FilesTab key={sessionId} sessionId={sessionId} />}
        {tab === "terminal" && (
          <Suspense fallback={<p className="px-4 py-6 text-xs text-text-tertiary">加载终端…</p>}>
            <TerminalTab key={sessionId} sessionId={sessionId} />
          </Suspense>
        )}
        {tab === "changes" && <ChangesTab sessionId={sessionId} />}
        {tab === "btw" && <BtwTab key={sessionId} sessionId={sessionId} />}
      </div>
    </div>
  );
}

// ---------- 变更 ----------

/** 同一文件的多次编辑合成一张卡片，hunk 按编辑先后排列。 */
function ChangesTab({ sessionId }: { sessionId: string }) {
  const diffs = useTranscript((state) => state.bySession[sessionId]?.diffs);
  const diffMode = usePrefs((state) => state.diffMode);
  const files = useMemo(() => {
    const byPath = new Map<string, ToolFileDiff>();
    for (const record of diffs ?? []) {
      for (const file of record.diff.files) {
        const existing = byPath.get(file.path);
        if (existing) existing.hunks = [...existing.hunks, ...file.hunks];
        else byPath.set(file.path, { path: file.path, hunks: [...file.hunks] });
      }
    }
    return [...byPath.values()];
  }, [diffs]);

  if (files.length === 0) {
    return (
      <PanelEmpty title="还没有文件变更" hint="模型通过编辑工具修改的文件会按文件汇总在这里。" />
    );
  }
  return (
    <div className="flex flex-col">
      <div className="flex h-10 items-center justify-between px-4">
        <span className="text-xs font-medium text-muted-foreground">{files.length} 个文件</span>
        <div className="flex items-center rounded-md bg-muted p-0.5">
          {(["unified", "split"] as const).map((mode) => (
            <IconButton
              key={mode}
              label={mode === "unified" ? "统一视图" : "并排视图"}
              size="xs"
              active={diffMode === mode}
              onClick={() => usePrefs.getState().set({ diffMode: mode })}
              className={cn("size-5", diffMode === mode && "bg-background shadow-xs")}
            >
              {mode === "unified" ? <RowsIcon /> : <Columns2Icon />}
            </IconButton>
          ))}
        </div>
      </div>
      <div className="space-y-2 px-3 pb-4">
        {files.map((file) => (
          <DiffFile key={file.path} file={file} defaultOpen={files.length <= 3} maxHeight={480} />
        ))}
      </div>
    </div>
  );
}
