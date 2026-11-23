// 右侧检查面板：文件 / 终端 / 变更 / 旁路。
// 计数徽标只保留 changes；需要远端数据的分页只在打开时拉取。

import {
  Columns2Icon,
  FolderIcon,
  GitCompareArrowsIcon,
  type LucideIcon,
  MessagesSquareIcon,
  PanelRightIcon,
  PlusIcon,
  RowsIcon,
  SquareTerminalIcon,
} from "lucide-react";
import { lazy, Suspense, useMemo } from "react";
import { IconButton } from "@/components/common/icon-button";
import { FilesTab } from "@/components/files/files-tab";
import { DiffFile } from "@/components/panels/diff-file";
import { type MsgKey, useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import type { ToolFileDiff } from "@/protocol/types";
import { type InspectorTab, useLayout } from "@/store/layout";
import { usePrefs } from "@/store/prefs";
import { useTranscript } from "@/store/transcript";
import { BtwTab, PanelEmpty } from "./inspector-tabs";
import { LauncherTab } from "./launcher-tab";

// 终端整页懒加载：xterm 依赖大，不进主路径。
const TerminalTab = lazy(() =>
  import("@/components/terminal/terminal-tab").then((module) => ({ default: module.TerminalTab })),
);

const TABS: { id: Exclude<InspectorTab, "launcher">; label: MsgKey; icon: LucideIcon }[] = [
  { id: "files", label: "insp.files", icon: FolderIcon },
  { id: "terminal", label: "insp.terminal", icon: SquareTerminalIcon },
  { id: "changes", label: "insp.changes", icon: GitCompareArrowsIcon },
  { id: "btw", label: "insp.btw", icon: MessagesSquareIcon },
];

const TAB_CLASS =
  "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-ui transition-colors [&_svg]:size-3.5";

export function InspectorPanel({ sessionId }: { sessionId: string }) {
  const t = useT();
  const tab = useLayout((state) => state.inspectorTab);
  const view = useTranscript((state) => state.bySession[sessionId]);

  const counts: Partial<Record<InspectorTab, number>> = {
    changes: new Set(view?.diffs.flatMap((record) => record.diff.files.map((file) => file.path)))
      .size,
  };

  return (
    <div className="flex h-full w-full min-w-0 flex-col bg-background text-foreground">
      <div className="flex h-11 shrink-0 items-center gap-1 px-2">
        <div className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none]">
          {TABS.map((item) => {
            const count = counts[item.id] ?? 0;
            const Icon = item.icon;
            return (
              <button
                key={item.id}
                type="button"
                onClick={() => useLayout.getState().openInspector(item.id)}
                className={cn(
                  TAB_CLASS,
                  tab === item.id
                    ? "bg-accent text-foreground"
                    : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                )}
              >
                <Icon />
                {t(item.label)}
                {count > 0 && (
                  <span className="min-w-4 rounded-full bg-foreground/10 px-1 text-center text-2xs leading-4 tabular-nums text-muted-foreground">
                    {count}
                  </span>
                )}
              </button>
            );
          })}
          {tab === "launcher" && (
            <span className={cn(TAB_CLASS, "bg-accent text-foreground")}>
              <PlusIcon />
              {t("nt.title")}
            </span>
          )}
          <IconButton
            label={t("nt.open")}
            size="sm"
            className="size-7 shrink-0"
            onClick={() => useLayout.getState().openInspector("launcher")}
          >
            <PlusIcon />
          </IconButton>
        </div>
        <IconButton
          label={t("insp.close")}
          shortcut="Alt R"
          size="sm"
          onClick={() => useLayout.getState().toggleRightbar(false)}
        >
          <PanelRightIcon />
        </IconButton>
      </div>

      {/* files/terminal/btw 各自管理内部滚动，只有 changes 用外层滚动 */}
      <div
        className={cn("min-h-0 flex-1", tab === "changes" ? "overflow-y-auto" : "flex flex-col")}
      >
        {tab === "files" && <FilesTab key={sessionId} sessionId={sessionId} />}
        {tab === "terminal" && (
          <Suspense
            fallback={<p className="px-4 py-6 text-xs text-text-tertiary">{t("insp.term_load")}</p>}
          >
            <TerminalTab key={sessionId} sessionId={sessionId} />
          </Suspense>
        )}
        {tab === "changes" && <ChangesTab sessionId={sessionId} />}
        {tab === "btw" && <BtwTab key={sessionId} sessionId={sessionId} />}
        {tab === "launcher" && <LauncherTab key={sessionId} sessionId={sessionId} />}
      </div>
    </div>
  );
}

// ---------- 变更 ----------

/** 同一文件的多次编辑合成一张卡片，hunk 按编辑先后排列。 */
function ChangesTab({ sessionId }: { sessionId: string }) {
  const t = useT();
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
    return <PanelEmpty title={t("insp.no_changes")} hint={t("insp.no_changes_hint")} />;
  }
  return (
    <div className="flex flex-col">
      <div className="flex h-10 items-center justify-between px-4">
        <span className="text-xs font-medium text-muted-foreground">
          {t("cp.files_n", { n: files.length })}
        </span>
        <div className="flex items-center gap-0.5">
          {(["unified", "split"] as const).map((mode) => (
            <IconButton
              key={mode}
              label={mode === "unified" ? t("insp.unified") : t("insp.split")}
              size="xs"
              active={diffMode === mode}
              onClick={() => usePrefs.getState().set({ diffMode: mode })}
              className="size-6"
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
