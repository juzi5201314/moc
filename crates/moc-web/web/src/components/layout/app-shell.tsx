// 三栏布局：左侧栏（工作区 → 会话）+ 中间（标签栏 / 会话头 / 转写 / composer）+ 可开关的右侧面板。
// 侧栏与右面板宽度以像素持久化到 prefs；拖动结束后才写入，避免每帧写 localStorage。

import { useEffect } from "react";
import {
  Group,
  Panel,
  type PanelImperativeHandle,
  Separator,
  usePanelRef,
} from "react-resizable-panels";
import { usePrefs } from "@/store/prefs";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { RightPanel } from "./right-panel";
import { SessionDialogs } from "./session-actions";
import { SessionHeader } from "./session-header";
import { Sidebar } from "./sidebar";
import { TabBar } from "./tab-bar";

const SIDEBAR_MIN = 220;
const SIDEBAR_MAX = 420;
const RIGHT_MIN = 280;
const RIGHT_MAX = 640;

function ResizeHandle({ label }: { label: string }) {
  return (
    <Separator
      aria-label={label}
      className="w-px bg-border transition-colors hover:bg-brand/60 data-[separator=active]:bg-brand"
    />
  );
}

/** 让折叠状态跟随 prefs.sidebarCollapsed（快捷键 Alt+S 只改 prefs）。 */
function useCollapseSync(panel: React.RefObject<PanelImperativeHandle | null>, collapsed: boolean) {
  useEffect(() => {
    const handle = panel.current;
    if (!handle) return;
    if (collapsed && !handle.isCollapsed()) handle.collapse();
    if (!collapsed && handle.isCollapsed()) handle.expand();
  }, [panel, collapsed]);
}

export function AppShell({ main }: { main: (sessionId: string | null) => React.ReactNode }) {
  const sidebarWidth = usePrefs((state) => state.sidebarWidth);
  const sidebarCollapsed = usePrefs((state) => state.sidebarCollapsed);
  const right = usePrefs((state) => state.rightPanel);
  const setPrefs = usePrefs((state) => state.set);
  const setRightPanel = usePrefs((state) => state.setRightPanel);
  const active = useTabs((state) => state.active);
  const loaded = useTranscript((state) => (active ? Boolean(state.bySession[active]) : false));
  const sidebarRef = usePanelRef();
  const rightRef = usePanelRef();

  useCollapseSync(sidebarRef, sidebarCollapsed);

  return (
    <div className="flex h-dvh min-h-0 bg-background text-foreground">
      <Group
        orientation="horizontal"
        className="min-h-0 flex-1"
        onLayoutChanged={(_layout, meta) => {
          if (!meta.isUserInteraction) return;
          const sidebar = sidebarRef.current;
          if (sidebar) {
            const collapsed = sidebar.isCollapsed();
            const width = Math.round(sidebar.getSize().inPixels);
            setPrefs(
              collapsed
                ? { sidebarCollapsed: true }
                : { sidebarCollapsed: false, sidebarWidth: width },
            );
          }
          const rightPanel = rightRef.current;
          if (rightPanel) setRightPanel({ width: Math.round(rightPanel.getSize().inPixels) });
        }}
      >
        <Panel
          id="sidebar"
          panelRef={sidebarRef}
          defaultSize={sidebarCollapsed ? 0 : sidebarWidth}
          minSize={SIDEBAR_MIN}
          maxSize={SIDEBAR_MAX}
          collapsible
          collapsedSize={0}
          groupResizeBehavior="preserve-pixel-size"
        >
          <Sidebar />
        </Panel>
        <ResizeHandle label="调整侧栏宽度" />
        <Panel id="main" minSize={360}>
          <main className="flex h-full min-w-0 flex-col">
            <TabBar />
            {active && loaded ? <SessionHeader sessionId={active} /> : null}
            <div className="relative flex min-h-0 flex-1 flex-col">{main(active)}</div>
          </main>
        </Panel>
        {right.open ? (
          <>
            <ResizeHandle label="调整右侧面板宽度" />
            <Panel
              id="right"
              panelRef={rightRef}
              defaultSize={right.width}
              minSize={RIGHT_MIN}
              maxSize={RIGHT_MAX}
              groupResizeBehavior="preserve-pixel-size"
            >
              <RightPanel sessionId={active} />
            </Panel>
          </>
        ) : null}
      </Group>
      <SessionDialogs />
    </div>
  );
}
