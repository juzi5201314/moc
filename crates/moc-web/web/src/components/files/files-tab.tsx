// 文件分页：左树右编辑区（宽 ≥640px），窄时上下层叠、打开文件后树被替换。
// 树按需加载；工作区可写，会话空间整体只读。

import { useEffect, useRef, useState } from "react";
import { PanelEmpty } from "@/components/panels/inspector-tabs";
import { cn } from "@/lib/utils";
import { useFiles } from "@/store/files";
import { useLayout } from "@/store/layout";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { EditorArea } from "./editor-area";
import { FileTree } from "./file-tree";
import { FilesToolbar } from "./files-toolbar";

const WIDE_LAYOUT = 640;

export function FilesTab({ sessionId }: { sessionId: string }) {
  const isDraft = sessionId === "draft-session";
  const files = useFiles((state) => state.bySession[sessionId]);
  const root = files?.root ?? "workspace";
  const hasTabs = (files?.tabs.length ?? 0) > 0;
  const stackedView = files?.stackedView ?? "tree";
  const draft = useSessions((state) => state.draftSession);
  const cwd = useTranscript((state) =>
    isDraft ? draft?.cwd : state.bySession[sessionId]?.info.cwd,
  );

  const containerRef = useRef<HTMLDivElement>(null);
  const [wide, setWide] = useState(true);
  const treeWidth = useLayout((state) => state.filesTreeWidth);

  // 布局随容器宽度切换：≥640 左树右编辑区，更窄上下层叠。
  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    const observer = new ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width ?? 0;
      setWide(width >= WIDE_LAYOUT);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const workspaceRoot = cwd ? { kind: "workspace" as const, cwd } : null;
  // 层叠模式：返回 tree 保留全部标签与草稿，开文件才切 editor。
  const showEditor = !wide && hasTabs && stackedView === "editor";

  return (
    <div ref={containerRef} className="flex h-full min-h-0 w-full flex-col">
      <FilesToolbar sessionId={sessionId} workspaceRoot={workspaceRoot} />
      {isDraft && root === "session" ? (
        <PanelEmpty title="发送第一条消息后可浏览会话空间" />
      ) : !cwd && root === "workspace" ? (
        <PanelEmpty title="工作区路径还没就绪" />
      ) : wide ? (
        <div className="flex min-h-0 flex-1">
          <div className="min-w-0 shrink-0 overflow-y-auto" style={{ width: treeWidth }}>
            <FileTree sessionId={sessionId} workspaceRoot={workspaceRoot} />
          </div>
          <TreeResizer />
          <div className={cn("min-w-0 flex-1", !hasTabs && "flex items-start justify-center")}>
            {hasTabs ? (
              <EditorArea sessionId={sessionId} workspaceRoot={workspaceRoot} stacked={false} />
            ) : (
              <PanelEmpty title="还没有打开的文件" hint="从左侧树里挑一个文件。" />
            )}
          </div>
        </div>
      ) : showEditor ? (
        <EditorArea sessionId={sessionId} workspaceRoot={workspaceRoot} stacked />
      ) : (
        <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
          <FileTree sessionId={sessionId} workspaceRoot={workspaceRoot} />
        </div>
      )}
    </div>
  );
}

/** 与 AppFrame 的 ResizeHandle 一致：悬停发丝线，拖动 brand。 */
function TreeResizer() {
  const [dragging, setDragging] = useState(false);
  const drag = useRef<number | null>(null);
  return (
    // biome-ignore lint/a11y/useSemanticElements: 需要 block 热区尺寸，<hr> 语义等价但默认样式要额外清
    <div
      role="separator"
      aria-orientation="vertical"
      className="group relative w-2 shrink-0 cursor-col-resize"
      onPointerDown={(event) => {
        drag.current = event.clientX;
        setDragging(true);
        event.currentTarget.setPointerCapture(event.pointerId);
      }}
      onPointerMove={(event) => {
        if (drag.current === null) return;
        const delta = event.clientX - drag.current;
        drag.current = event.clientX;
        useLayout.getState().setFilesTreeWidth(useLayout.getState().filesTreeWidth + delta);
      }}
      onPointerUp={() => {
        drag.current = null;
        setDragging(false);
      }}
    >
      <div
        className={cn(
          "absolute inset-y-0 left-1/2 w-px -translate-x-1/2",
          dragging ? "bg-brand/60" : "bg-transparent group-hover:bg-border-strong",
        )}
      />
    </div>
  );
}
