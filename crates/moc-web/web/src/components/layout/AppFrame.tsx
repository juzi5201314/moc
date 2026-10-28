import { PanelRightOpenIcon } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";
import { SIDEBAR_AUTO_COLLAPSE, SIDEBAR_COLLAPSED, useLayout } from "@/store/layout";

interface AppFrameProps {
  sidebar: ReactNode;
  main: ReactNode;
  rightbar: ReactNode;
}

export function AppFrame({ sidebar, main, rightbar }: AppFrameProps) {
  const {
    sidebarCollapsed,
    sidebarWidth,
    rightbarOpen,
    rightbarWidth,
    rightbarFullscreen,
    setSidebarWidth,
    setRightbarWidth,
    setSidebarCollapsed,
  } = useLayout();

  const [draggingSidebar, setDraggingSidebar] = useState(false);
  const [draggingRightbar, setDraggingRightbar] = useState(false);
  const [viewportWidth, setViewportWidth] = useState(() => window.innerWidth);
  const wasNarrow = useRef(false);
  const isNarrow = viewportWidth < SIDEBAR_AUTO_COLLAPSE;

  useEffect(() => {
    const handleResize = () => setViewportWidth(window.innerWidth);
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, []);

  useEffect(() => {
    if (isNarrow && !wasNarrow.current) useLayout.getState().toggleRightbar(false);
    wasNarrow.current = isNarrow;
  }, [isNarrow]);

  // 监听窗口尺寸自动折叠侧边栏
  useEffect(() => {
    let prevWidth = window.innerWidth;
    if (prevWidth < SIDEBAR_AUTO_COLLAPSE) setSidebarCollapsed(true);
    const handleResize = () => {
      const w = window.innerWidth;
      if (w < SIDEBAR_AUTO_COLLAPSE && prevWidth >= SIDEBAR_AUTO_COLLAPSE) {
        setSidebarCollapsed(true);
      }
      prevWidth = w;
    };
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, [setSidebarCollapsed]);

  // 左侧栏拖拽
  const sidebarStartPos = useRef(0);
  const sidebarStartWidth = useRef(0);

  const handleSidebarPointerDown = useCallback(
    (e: React.PointerEvent) => {
      if (sidebarCollapsed) return;
      e.preventDefault();
      e.currentTarget.setPointerCapture(e.pointerId);
      sidebarStartPos.current = e.clientX;
      sidebarStartWidth.current = sidebarWidth;
      setDraggingSidebar(true);
    },
    [sidebarCollapsed, sidebarWidth],
  );

  const handleSidebarPointerMove = useCallback(
    (e: React.PointerEvent) => {
      if (!draggingSidebar) return;
      const delta = e.clientX - sidebarStartPos.current;
      setSidebarWidth(sidebarStartWidth.current + delta);
    },
    [draggingSidebar, setSidebarWidth],
  );

  const handleSidebarPointerUp = useCallback(
    (e: React.PointerEvent) => {
      if (!draggingSidebar) return;
      try {
        e.currentTarget.releasePointerCapture(e.pointerId);
      } catch {
        // ignore
      }
      setDraggingSidebar(false);
    },
    [draggingSidebar],
  );

  // 右侧栏拖拽
  const rightbarStartPos = useRef(0);
  const rightbarStartWidth = useRef(0);

  const handleRightbarPointerDown = useCallback(
    (e: React.PointerEvent) => {
      if (!rightbarOpen || rightbarFullscreen) return;
      e.preventDefault();
      e.currentTarget.setPointerCapture(e.pointerId);
      rightbarStartPos.current = e.clientX;
      rightbarStartWidth.current = rightbarWidth;
      setDraggingRightbar(true);
    },
    [rightbarOpen, rightbarFullscreen, rightbarWidth],
  );

  const handleRightbarPointerMove = useCallback(
    (e: React.PointerEvent) => {
      if (!draggingRightbar) return;
      const delta = rightbarStartPos.current - e.clientX;
      setRightbarWidth(rightbarStartWidth.current + delta);
    },
    [draggingRightbar, setRightbarWidth],
  );

  const handleRightbarPointerUp = useCallback(
    (e: React.PointerEvent) => {
      if (!draggingRightbar) return;
      try {
        e.currentTarget.releasePointerCapture(e.pointerId);
      } catch {
        // ignore
      }
      setDraggingRightbar(false);
    },
    [draggingRightbar],
  );

  const effectiveSidebarWidth = sidebarCollapsed ? SIDEBAR_COLLAPSED : sidebarWidth;
  const layoutSidebarWidth = isNarrow ? SIDEBAR_COLLAPSED : effectiveSidebarWidth;

  return (
    <div
      className={`relative flex h-screen w-screen overflow-hidden bg-background text-foreground select-text ${
        draggingSidebar || draggingRightbar ? "select-none cursor-col-resize" : ""
      }`}
    >
      <div
        style={{ width: `${layoutSidebarWidth}px` }}
        className="relative z-20 h-full shrink-0 border-r border-border transition-[width] duration-200 ease-out"
      >
        <div
          className={
            isNarrow && !sidebarCollapsed
              ? "absolute inset-y-0 left-0 z-40 w-[280px] border-r border-border bg-sidebar shadow-xl"
              : "h-full w-full"
          }
        >
          {sidebar}
        </div>

        {!sidebarCollapsed && !isNarrow && (
          <div
            className={`absolute top-0 -right-1 bottom-0 z-30 w-2 cursor-col-resize transition-colors ${
              draggingSidebar ? "bg-primary/30" : "hover:bg-primary/20"
            }`}
            onPointerDown={handleSidebarPointerDown}
            onPointerMove={handleSidebarPointerMove}
            onPointerUp={handleSidebarPointerUp}
            onPointerCancel={handleSidebarPointerUp}
          />
        )}
      </div>

      <main
        className={`relative flex min-w-0 flex-1 flex-col overflow-hidden transition-all duration-200 ${
          rightbarFullscreen ? "hidden" : "flex"
        }`}
      >
        {main}
      </main>

      {isNarrow && !rightbarOpen && !rightbarFullscreen && (
        <button
          type="button"
          onClick={() => useLayout.getState().toggleRightbar(true)}
          aria-label="打开辅助面板"
          title="打开辅助面板"
          className="absolute right-2 top-2 z-10 flex size-8 items-center justify-center rounded-md border border-border bg-background text-muted-foreground shadow-sm hover:bg-muted hover:text-foreground"
        >
          <PanelRightOpenIcon className="size-4" />
        </button>
      )}

      {rightbarOpen && (
        <>
          {!rightbarFullscreen && !isNarrow && (
            <div
              className={`relative z-30 -mr-1 w-1 shrink-0 cursor-col-resize transition-colors ${
                draggingRightbar ? "bg-primary/30" : "hover:bg-primary/20"
              }`}
              onPointerDown={handleRightbarPointerDown}
              onPointerMove={handleRightbarPointerMove}
              onPointerUp={handleRightbarPointerUp}
              onPointerCancel={handleRightbarPointerUp}
            />
          )}

          <div
            style={
              rightbarFullscreen
                ? { width: "100%" }
                : isNarrow
                  ? { width: `${Math.max(0, viewportWidth - effectiveSidebarWidth)}px` }
                  : { width: `${rightbarWidth}px` }
            }
            className={`relative z-20 h-full shrink-0 border-l border-border bg-background transition-[width] duration-200 ease-out ${
              rightbarFullscreen
                ? "absolute inset-0 z-40 border-l-0"
                : isNarrow
                  ? "absolute inset-y-0 right-0 z-40 shadow-xl"
                  : ""
            }`}
          >
            {rightbar}
          </div>
        </>
      )}
    </div>
  );
}
