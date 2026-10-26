// 顶部会话标签栏：每个打开的会话一个标签；原生 HTML5 拖动排序，中键关闭。

import { PanelRight, X } from "lucide-react";
import { type DragEvent, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { SessionStatusIcon } from "@/components/common/session-status";
import { cn } from "@/lib/utils";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useSessionActions } from "./session-actions";

const DRAG_TYPE = "application/x-moc-tab";

export function TabBar() {
  const open = useTabs((state) => state.open);
  const active = useTabs((state) => state.active);
  const rightOpen = usePrefs((state) => state.rightPanel.open);
  const setRightPanel = usePrefs((state) => state.setRightPanel);

  return (
    <div className="flex h-10 shrink-0 items-stretch border-b bg-panel">
      <div
        role="tablist"
        aria-label="打开的会话"
        className="flex min-w-0 flex-1 items-stretch overflow-x-auto [scrollbar-width:none]"
      >
        {open.map((id, index) => (
          <Tab key={id} sessionId={id} index={index} active={id === active} />
        ))}
      </div>
      <div className="flex items-center px-1.5">
        <IconButton
          label={rightOpen ? "关闭右侧面板" : "打开右侧面板"}
          shortcut="Alt+R"
          aria-pressed={rightOpen}
          onClick={() => setRightPanel({ open: !rightOpen })}
        >
          <PanelRight />
        </IconButton>
      </div>
    </div>
  );
}

function useTitle(sessionId: string): string {
  const fromView = useTranscript((state) => state.bySession[sessionId]?.info.title);
  const fromList = useSessions(
    (state) => allSessions(state).find((session) => session.id === sessionId)?.title,
  );
  return fromView || fromList || "新会话";
}

function Tab({ sessionId, index, active }: { sessionId: string; index: number; active: boolean }) {
  const title = useTitle(sessionId);
  const activate = useTabs((state) => state.activate);
  const move = useTabs((state) => state.move);
  const actions = useSessionActions();
  const [dropHint, setDropHint] = useState(false);

  const onDragStart = (event: DragEvent) => {
    event.dataTransfer.setData(DRAG_TYPE, String(index));
    event.dataTransfer.effectAllowed = "move";
  };
  const onDragOver = (event: DragEvent) => {
    if (!event.dataTransfer.types.includes(DRAG_TYPE)) return;
    event.preventDefault();
    setDropHint(true);
  };
  const onDrop = (event: DragEvent) => {
    setDropHint(false);
    const from = Number(event.dataTransfer.getData(DRAG_TYPE));
    if (Number.isInteger(from) && from !== index) move(from, index);
  };

  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: 拖动排序与中键关闭只是鼠标增强；激活与关闭由内部两个 button 提供键盘可达
    <div
      draggable
      onDragStart={onDragStart}
      onDragOver={onDragOver}
      onDragLeave={() => setDropHint(false)}
      onDrop={onDrop}
      onAuxClick={(event) => {
        if (event.button === 1) actions.close(sessionId);
      }}
      className={cn(
        "group relative flex max-w-56 min-w-28 items-center border-r text-sm transition-colors",
        active
          ? "bg-background text-foreground"
          : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
        dropHint && "ring-1 ring-brand ring-inset",
      )}
    >
      {active ? <span className="absolute inset-x-0 top-0 h-0.5 bg-brand" aria-hidden /> : null}
      <button
        type="button"
        role="tab"
        aria-selected={active}
        title={title}
        onClick={() => activate(sessionId)}
        className="flex min-w-0 flex-1 items-center gap-1.5 py-2 pr-1 pl-3 text-left outline-none focus-visible:underline"
      >
        <SessionStatusIcon sessionId={sessionId} />
        <span className="truncate">{title}</span>
      </button>
      <button
        type="button"
        aria-label={`关闭 ${title}`}
        onClick={() => actions.close(sessionId)}
        className={cn(
          "mr-1.5 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground",
          active ? "opacity-100" : "opacity-0 group-hover:opacity-100 focus-visible:opacity-100",
        )}
      >
        <X className="size-3.5" aria-hidden />
      </button>
    </div>
  );
}
