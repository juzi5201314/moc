// 三栏骨架：侧栏贴底色，主舞台与检查面板作为一张内嵌卡片浮在侧栏底色上。
// 窄窗口下侧栏与检查面板都改成浮层，主舞台保持完整宽度。

import { type PointerEvent, type ReactNode, useEffect, useRef, useState } from "react";
import { useNarrow } from "@/lib/use-narrow";
import { cn } from "@/lib/utils";
import { useLayout } from "@/store/layout";

type AppFrameProps = {
  sidebar: ReactNode;
  main: ReactNode;
  rightbar: ReactNode;
};

/** 列宽拖拽柄：只在指针按下期间跟踪，松开即提交。 */
function ResizeHandle({
  side,
  width,
  onResize,
}: {
  side: "left" | "right";
  width: number;
  onResize: (width: number) => void;
}) {
  const start = useRef<{ x: number; width: number } | null>(null);
  const [dragging, setDragging] = useState(false);

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    start.current = { x: event.clientX, width };
    setDragging(true);
  };
  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    if (!start.current) return;
    const delta = event.clientX - start.current.x;
    onResize(start.current.width + (side === "left" ? delta : -delta));
  };
  const onPointerUp = () => {
    start.current = null;
    setDragging(false);
  };

  return (
    // biome-ignore lint/a11y/useSemanticElements: 拖拽手柄需要承载指示线与拖动遮罩子元素，<hr> 不能有子节点
    <div
      role="separator"
      aria-orientation="vertical"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      className={cn(
        "group absolute inset-y-0 z-30 w-2 cursor-col-resize",
        side === "left" ? "-right-1" : "-left-1",
      )}
    >
      <div
        className={cn(
          "mx-auto h-full w-px transition-colors",
          dragging ? "bg-brand/60" : "bg-transparent group-hover:bg-border-strong",
        )}
      />
      {dragging && <div className="fixed inset-0 z-50 cursor-col-resize" />}
    </div>
  );
}

export function AppFrame({ sidebar, main, rightbar }: AppFrameProps) {
  const narrow = useNarrow();
  const sidebarCollapsed = useLayout((state) => state.sidebarCollapsed);
  const sidebarOverlay = useLayout((state) => state.sidebarOverlay);
  const sidebarWidth = useLayout((state) => state.sidebarWidth);
  const rightbarOpen = useLayout((state) => state.rightbarOpen);
  const rightbarWidth = useLayout((state) => state.rightbarWidth);
  const { setSidebarWidth, setRightbarWidth, setSidebarOverlay, toggleRightbar } =
    useLayout.getState();

  const sidebarVisible = narrow ? sidebarOverlay : !sidebarCollapsed;

  // 从窄窗口拉宽时收起浮层，避免回到宽布局后仍盖着一层遮罩。
  useEffect(() => {
    if (!narrow) setSidebarOverlay(false);
  }, [narrow, setSidebarOverlay]);

  return (
    <div className="flex h-dvh w-screen overflow-hidden bg-sidebar text-foreground">
      {narrow ? (
        sidebarVisible && (
          <>
            <button
              type="button"
              aria-label="关闭侧栏"
              className="fixed inset-0 z-40 bg-black/30 backdrop-blur-[1px] animate-in fade-in-0"
              onClick={() => setSidebarOverlay(false)}
            />
            <aside className="fixed inset-y-0 left-0 z-50 w-[280px] border-r border-border-subtle bg-sidebar shadow-[var(--elevation-popover)] animate-in slide-in-from-left-4 duration-200">
              {sidebar}
            </aside>
          </>
        )
      ) : (
        <aside
          style={{ width: sidebarVisible ? sidebarWidth : 0 }}
          className="relative h-full shrink-0 overflow-visible transition-[width] duration-200 ease-out"
        >
          <div
            style={{ width: sidebarWidth }}
            className={cn(
              "h-full overflow-hidden transition-opacity duration-150",
              sidebarVisible ? "opacity-100" : "pointer-events-none opacity-0",
            )}
          >
            {sidebar}
          </div>
          {sidebarVisible && (
            <ResizeHandle side="left" width={sidebarWidth} onResize={setSidebarWidth} />
          )}
        </aside>
      )}

      <div
        className={cn(
          "relative flex min-w-0 flex-1 overflow-hidden bg-background",
          !narrow && "my-2 mr-2 rounded-xl shadow-[var(--elevation-ring)]",
          !narrow && !sidebarVisible && "ml-2",
        )}
      >
        <main className="relative flex min-w-0 flex-1 flex-col overflow-hidden">{main}</main>

        {rightbarOpen &&
          (narrow ? (
            <>
              <button
                type="button"
                aria-label="关闭检查面板"
                className="absolute inset-0 z-30 bg-black/20 animate-in fade-in-0"
                onClick={() => toggleRightbar(false)}
              />
              <aside className="absolute inset-y-0 right-0 z-40 w-[min(420px,92%)] border-l border-border-subtle bg-background shadow-[var(--elevation-popover)] animate-in slide-in-from-right-4 duration-200">
                {rightbar}
              </aside>
            </>
          ) : (
            <aside
              style={{ width: rightbarWidth }}
              className="relative h-full shrink-0 border-l border-border-subtle bg-background"
            >
              <ResizeHandle side="right" width={rightbarWidth} onResize={setRightbarWidth} />
              {rightbar}
            </aside>
          ))}
      </div>
    </div>
  );
}
