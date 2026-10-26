// 会话头：标题（双击原地编辑）、工作区胶囊、模式/权限徽章、只读横幅、上下文占用环、更多菜单。

import { FolderOpen, Lock, MoreHorizontal, PlugZap } from "lucide-react";
import { useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatTokens } from "@/lib/format";
import { rpc } from "@/protocol/client";
import type { SessionMode, SessionPermission } from "@/protocol/types";
import { reacquire, refreshSideData, releaseSession, renameSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { workspaceName } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { runAction, useSessionActions } from "./session-actions";

export const MODE_LABEL: Record<SessionMode, string> = {
  normal: "标准",
  ask: "问答",
  plan: "计划",
  ptc: "PTC",
  clm_experimental: "CLM",
};

export const PERMISSION_LABEL: Record<SessionPermission, string> = {
  full: "完全权限",
  workspace: "工作区沙盒",
  workspace_only: "仅工作区",
};

export function SessionHeader({ sessionId }: { sessionId: string }) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const set = useUi((state) => state.set);
  const setRightPanel = usePrefs((state) => state.setRightPanel);
  const actions = useSessionActions();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");

  if (!view) return null;
  const { info } = view;

  const commitTitle = () => {
    setEditing(false);
    if (draft.trim() && draft.trim() !== info.title) {
      void runAction(() => renameSession(sessionId, draft));
    }
  };

  const reloadPlugins = () =>
    runAction(async () => {
      await rpc.call("plugin/reload", { session_id: sessionId });
      await refreshSideData(sessionId);
    }, "插件已重载");

  return (
    <div className="border-b">
      <header className="flex h-11 items-center gap-2 px-4">
        {editing ? (
          <Input
            autoFocus
            aria-label="会话标题"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onBlur={commitTitle}
            onKeyDown={(event) => {
              if (event.key === "Enter") commitTitle();
              if (event.key === "Escape") setEditing(false);
            }}
            className="h-7 max-w-80"
          />
        ) : (
          <button
            type="button"
            className="min-w-0 truncate text-sm font-medium"
            title="双击重命名"
            disabled={view.readOnly}
            onDoubleClick={() => {
              setDraft(info.title);
              setEditing(true);
            }}
          >
            {info.title || "新会话"}
          </button>
        )}

        <Tooltip>
          <TooltipTrigger asChild>
            <span className="inline-flex max-w-48 items-center gap-1 truncate rounded-full border px-2 py-0.5 text-xs text-muted-foreground">
              <FolderOpen className="size-3 shrink-0" aria-hidden />
              <span className="truncate">{workspaceName(info.cwd)}</span>
            </span>
          </TooltipTrigger>
          <TooltipContent>{info.cwd}</TooltipContent>
        </Tooltip>
        <Badge variant="secondary">{MODE_LABEL[info.mode]}</Badge>
        <Badge variant="outline">{PERMISSION_LABEL[info.permission]}</Badge>
        {view.readOnly ? (
          <Badge variant="outline" className="gap-1">
            <Lock className="size-3" aria-hidden />
            只读
          </Badge>
        ) : null}

        <div className="ml-auto flex items-center gap-1">
          {view.pluginNotice ? (
            <Button
              variant="outline"
              size="xs"
              className="text-warn"
              title={`${view.pluginNotice.plugin}：${view.pluginNotice.message}`}
              onClick={() => void reloadPlugins()}
            >
              <PlugZap aria-hidden />
              重载插件
            </Button>
          ) : null}
          <ContextRing
            used={view.context?.total_tokens}
            total={view.context?.context_window}
            onClick={() => setRightPanel({ open: true, tab: "context" })}
          />
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <IconButton label="更多">
                <MoreHorizontal />
              </IconButton>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={() => set({ usage: sessionId })}>用量</DropdownMenuItem>
              <DropdownMenuItem onSelect={() => set({ contextContent: sessionId })}>
                上下文正文
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => set({ plugins: sessionId })}>插件</DropdownMenuItem>
              <DropdownMenuItem
                disabled={view.readOnly || view.running}
                onSelect={() =>
                  void runAction(
                    () => rpc.call("session/compact", { session_id: sessionId }),
                    "上下文已压缩",
                  )
                }
              >
                压缩上下文
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                disabled={view.readOnly}
                onSelect={() => void runAction(() => releaseSession(sessionId), "已释放写权限")}
              >
                释放写权限
              </DropdownMenuItem>
              <DropdownMenuItem variant="destructive" onSelect={() => actions.remove(info)}>
                删除会话
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </header>

      {view.readOnly && !info.parent_session_id ? (
        <div className="flex items-center gap-2 border-t bg-panel px-4 py-1.5 text-xs text-muted-foreground">
          <Lock className="size-3" aria-hidden />
          <span>
            只读
            {view.holder ? ` · ${view.holder.frontend} · pid ${view.holder.pid}` : ""}
          </span>
          <Button
            variant="link"
            size="xs"
            className="ml-auto h-auto p-0"
            onClick={() => void runAction(() => reacquire(sessionId))}
          >
            重新获取写权限
          </Button>
        </div>
      ) : null}
    </div>
  );
}

/** 上下文占用环：SVG 圆环 + tooltip 显示 used / total。 */
function ContextRing({
  used,
  total,
  onClick,
}: {
  used?: number;
  total?: number;
  onClick: () => void;
}) {
  if (!used || !total) return null;
  const ratio = Math.min(1, used / total);
  const radius = 7;
  const circumference = 2 * Math.PI * radius;
  const tone = ratio > 0.85 ? "text-destructive" : ratio > 0.65 ? "text-warn" : "text-brand";
  const label = `上下文 ${formatTokens(used)} / ${formatTokens(total)}（${Math.round(ratio * 100)}%）`;
  return (
    <IconButton label={label} onClick={onClick}>
      <svg viewBox="0 0 18 18" className="size-4.5 -rotate-90" aria-hidden>
        <circle cx="9" cy="9" r={radius} fill="none" strokeWidth="2" className="stroke-border" />
        <circle
          cx="9"
          cy="9"
          r={radius}
          fill="none"
          strokeWidth="2"
          strokeLinecap="round"
          strokeDasharray={circumference}
          strokeDashoffset={circumference * (1 - ratio)}
          className={`stroke-current ${tone}`}
        />
      </svg>
    </IconButton>
  );
}
