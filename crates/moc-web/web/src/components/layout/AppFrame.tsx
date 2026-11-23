// 三栏骨架：会话侧栏、主舞台与检查区拼成一张内嵌窗框。
// 窄窗口下侧栏与检查面板都改成浮层，主舞台保持完整宽度。

import { type PointerEvent, type ReactNode, useEffect, useRef, useState } from "react";
import { useT } from "@/lib/i18n";
import { useNarrow } from "@/lib/use-narrow";
import { cn } from "@/lib/utils";
import { SIDEBAR_DEFAULT, useLayout } from "@/store/layout";
import { AppRail } from "./app-rail";

type AppFrameProps = {
  /** 不传时只有主舞台（如插件页），标签栏不显示折叠按钮。 */
  sidebar?: ReactNode;
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
      data-resizing={dragging || undefined}
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
  const t = useT();
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
    <div className="flex h-dvh w-screen overflow-hidden bg-chrome text-foreground md:py-1.5 md:pr-1.5">
      <AppRail hasSidebar={Boolean(sidebar)} />
      {!sidebar ? null : narrow ? (
        sidebarVisible && (
          <>
            <button
              type="button"
              aria-label={t("layout.close_sidebar")}
              className="fixed inset-y-0 right-0 left-[52px] z-40 bg-black/30 backdrop-blur-[1px] motion-safe:animate-in motion-safe:fade-in-0 motion-safe:duration-160"
              onClick={() => setSidebarOverlay(false)}
            />
            <aside
              style={{ width: SIDEBAR_DEFAULT }}
              className="fixed inset-y-0 left-[52px] z-50 max-w-[calc(100%-76px)] overflow-hidden rounded-r-xl bg-sidebar shadow-[var(--elevation-popover)] motion-safe:animate-in motion-safe:fade-in-0 motion-safe:slide-in-from-left-2 motion-safe:duration-160"
            >
              {sidebar}
            </aside>
          </>
        )
      ) : (
        <aside
          style={{ width: sidebarVisible ? sidebarWidth : 0 }}
          className="relative h-full shrink-0 overflow-visible motion-safe:transition-[width] motion-safe:duration-160 motion-safe:ease-out has-[[data-resizing]]:transition-none"
        >
          <div
            style={{ width: sidebarWidth }}
            inert={!sidebarVisible}
            className={cn(
              "h-full overflow-hidden rounded-l-xl motion-safe:transition-opacity motion-safe:duration-150",
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
          "relative flex min-w-0 flex-1 overflow-hidden bg-background md:rounded-r-xl",
          sidebar && !narrow && sidebarVisible ? "border-l border-border" : "md:rounded-l-xl",
        )}
      >
        <main className="relative flex min-w-0 flex-1 flex-col overflow-hidden">{main}</main>

        {rightbar && rightbarOpen && narrow && (
          <>
            <button
              type="button"
              aria-label={t("layout.close_inspector")}
              className="absolute inset-0 z-30 bg-black/20 motion-safe:animate-in motion-safe:fade-in-0 motion-safe:duration-160"
              onClick={() => toggleRightbar(false)}
            />
            <aside className="absolute inset-y-0 right-0 z-40 w-[min(420px,92%)] overflow-hidden rounded-l-xl bg-panel shadow-[var(--elevation-popover)] motion-safe:animate-in motion-safe:fade-in-0 motion-safe:slide-in-from-right-2 motion-safe:duration-160">
              {rightbar}
            </aside>
          </>
        )}
      </div>

      {/* 宽屏检查面板是窗框里独立的一张卡片，与主舞台之间留 chrome 缝隙。 */}
      {rightbar && rightbarOpen && !narrow && (
        <aside
          style={{ width: rightbarWidth }}
          className="relative ml-1 h-full max-w-[max(320px,calc(100%-560px))] shrink-0 rounded-xl bg-background shadow-[var(--elevation-ring)]"
        >
          <ResizeHandle side="right" width={rightbarWidth} onResize={setRightbarWidth} />
          <div className="h-full overflow-hidden rounded-xl">{rightbar}</div>
        </aside>
      )}
    </div>
  );
}
