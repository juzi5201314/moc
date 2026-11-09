// 终端分页：标签条 + xterm 附着视图。整页懒加载（不进面板主路径）。

import { PlusIcon, XIcon } from "lucide-react";
import { lazy, Suspense, useEffect } from "react";
import { IconButton } from "@/components/common/icon-button";
import { PanelEmpty } from "@/components/panels/inspector-tabs";
import { cn } from "@/lib/utils";
import { useSessions } from "@/store/sessions";
import {
  activateTerminal,
  closeTerminal,
  refreshTerminals,
  spawnTerminal,
  useTerminals,
} from "@/store/terminal";
import { useTranscript } from "@/store/transcript";

const XtermView = lazy(() => import("./xterm-view"));

export function TerminalTab({ sessionId }: { sessionId: string }) {
  const isDraft = sessionId === "draft-session";
  const draft = useSessions((state) => state.draftSession);
  const cwd = useTranscript((state) =>
    isDraft ? draft?.cwd : state.bySession[sessionId]?.info.cwd,
  );
  const state = useTerminals((state) => (cwd ? state.byCwd[cwd] : undefined));
  const items = state?.items ?? [];

  useEffect(() => {
    if (cwd) void refreshTerminals(cwd, true);
  }, [cwd]);

  if (!cwd) return <PanelEmpty title="工作区路径还没就绪" />;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-9 shrink-0 items-center gap-0.5 overflow-x-auto px-2 [scrollbar-width:none]">
        {items.map((item) => (
          <TerminalChip key={item.id} cwd={cwd} item={item} active={state?.active === item.id} />
        ))}
        <IconButton label="新建终端" size="xs" onClick={() => void spawnTerminal(cwd)}>
          <PlusIcon />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1">
        {items.length === 0 ? (
          <PanelEmpty title="还没有终端" hint="点上方 + 新建一个。" />
        ) : (
          items.map((item) => (
            <div
              key={item.id}
              className={cn("h-full", state?.active === item.id ? "block" : "hidden")}
            >
              {state?.active === item.id && (
                <Suspense
                  fallback={<p className="px-4 py-6 text-xs text-text-tertiary">加载终端…</p>}
                >
                  <XtermView terminal={item} />
                </Suspense>
              )}
            </div>
          ))
        )}
      </div>
    </div>
  );
}

function TerminalChip({
  cwd,
  item,
  active,
}: {
  cwd: string;
  item: import("@/lib/web-api").PtyInfo;
  active: boolean;
}) {
  const basename = item.shell.split("/").filter(Boolean).at(-1) ?? item.shell;
  const relative =
    item.cwd !== cwd && item.cwd.startsWith(cwd) ? ` · ${item.cwd.slice(cwd.length + 1)}` : "";
  return (
    // biome-ignore lint/a11y/noStaticElementInteractions: 标签 chip 与文件标签同款的 span+onClick 形态
    // biome-ignore lint/a11y/useKeyWithClickEvents: 键盘用户经 Ctrl+` 与标签顺序切换
    <span
      onClick={() => activateTerminal(cwd, item.id)}
      className={cn(
        "group/tchip flex h-7 shrink-0 cursor-default items-center gap-1.5 rounded-lg px-2.5 text-ui",
        active ? "bg-accent text-foreground" : "text-muted-foreground hover:text-foreground",
      )}
    >
      <span className={cn("truncate font-mono", item.exited && "text-text-tertiary")}>
        {basename}
        {relative}
      </span>
      {item.exited && (
        <span className="text-2xs text-text-tertiary">
          已退出{item.exit_code !== null ? ` (${item.exit_code})` : ""}
        </span>
      )}
      <button
        type="button"
        aria-label="关闭终端"
        onClick={(event) => {
          event.stopPropagation();
          void closeTerminal(cwd, item.id);
        }}
        className="flex size-4 items-center justify-center rounded text-text-tertiary opacity-0 transition-opacity group-hover/tchip:opacity-100 hover:text-foreground"
      >
        <XIcon className="size-3" />
      </button>
    </span>
  );
}
