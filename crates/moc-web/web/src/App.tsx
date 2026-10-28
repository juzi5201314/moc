import { BotIcon, FolderOpenIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { ChatThread } from "@/components/chat/chat-thread";
import { ComposerBar } from "@/components/chat/composer-bar";
import { SessionHeader } from "@/components/chat/session-header";
import { CommandPalette } from "@/components/dialogs/command-palette";
import { NewSessionDialog } from "@/components/dialogs/new-session-dialog";
import { SettingsDialog } from "@/components/dialogs/settings-dialog";
import { WorkspacePicker } from "@/components/dialogs/workspace-picker";
import { AppFrame } from "@/components/layout/AppFrame";
import { InspectorPanel } from "@/components/panels/inspector-panel";
import { WorkspaceSidebar } from "@/components/sidebar/workspace-sidebar";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { clockTime } from "@/lib/format";
import { useGlobalShortcuts } from "@/lib/shortcuts";
import { useThemeSync } from "@/lib/theme";
import type { Block } from "@/lib/transcript-model";
import { rpc } from "@/protocol/client";
import { createQuickSession } from "@/store/lifecycle";
import { useSessions, workspaceName } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import "@/store/connection";
import "@/store/dispatch";
import "@/store/lifecycle";

function trajectoryLabel(block: Block): string {
  switch (block.kind) {
    case "user":
      return "用户提示";
    case "text":
      return "助手回复";
    case "reasoning":
      return "思考过程";
    case "tool":
      return `工具调用 · ${block.name}`;
    case "bash":
      return `终端命令 · ${block.command}`;
    case "agent-message":
      return `子代理消息 · ${block.from}`;
    case "plan":
      return `计划 · ${block.title}`;
    case "status":
      return "状态更新";
    case "footer":
      return "轮次完成";
  }
}

function trajectoryBody(block: Block): string {
  switch (block.kind) {
    case "user":
      return block.text;
    case "text":
    case "reasoning":
      return block.text;
    case "tool":
      return (block.result ?? block.arguments) || "等待工具结果";
    case "bash":
      return block.output ?? `$ ${block.command}`;
    case "agent-message":
      return block.body;
    case "plan":
      return block.path;
    case "status":
      return block.text;
    case "footer":
      return block.finishReason ? `结束原因：${String(block.finishReason)}` : "";
  }
}

function TrajectoryView({ sessionId }: { sessionId: string }) {
  const blocks = useTranscript((state) => state.bySession[sessionId]?.blocks ?? []);
  return (
    <div className="moc-chat-pane flex min-h-0 flex-1 flex-col overflow-hidden bg-background">
      <div className="min-h-0 flex-1 overflow-y-auto py-6">
        <div className="moc-chat-column flex flex-col">
          {blocks.length === 0 ? (
            <p className="py-16 text-center text-sm text-[var(--text-tertiary)]">
              发送第一条提示后，这里会显示真实执行事件。
            </p>
          ) : (
            blocks.map((block) => (
              <div key={block.id} className="flex flex-col gap-1 border-b border-border/60 py-2.5">
                <div className="flex items-center justify-between gap-3 text-[13px]">
                  <span className="font-medium text-foreground">{trajectoryLabel(block)}</span>
                  {"atMs" in block ? (
                    <time className="font-mono text-[11px] text-[var(--text-tertiary)]">
                      {clockTime(block.atMs)}
                    </time>
                  ) : null}
                </div>
                <pre className="max-h-60 overflow-auto whitespace-pre-wrap break-words font-mono text-xs leading-5 text-[var(--text-tertiary)]">
                  {trajectoryBody(block)}
                </pre>
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

export function App() {
  useThemeSync();
  useGlobalShortcuts();

  const activeSessionId = useTabs((state) => state.active);
  const activeCwd = useTranscript((state) => state.bySession[activeSessionId ?? ""]?.info.cwd);
  const workspacePath = useSessions(
    (state) =>
      activeCwd ??
      state.draftSession?.cwd ??
      state.workspaces[0] ??
      state.list[0]?.cwd ??
      "/home/soeur/project/moc",
  );
  const [sessionTab, setSessionTab] = useState<"chat" | "trajectory">("chat");
  const hasConversation = Boolean(activeSessionId && activeSessionId !== "draft-session");
  // 还没有任何消息的会话同样走欢迎态：DSH 的新会话就是「英雄区 + 内嵌输入框」
  const blockCount = useTranscript((state) =>
    activeSessionId ? (state.bySession[activeSessionId]?.blocks.length ?? 0) : 0,
  );
  const showConversation = hasConversation && blockCount > 0;
  const composerSessionId = activeSessionId ?? "draft-session";

  useEffect(() => {
    rpc.connect();
    return () => rpc.dispose();
  }, []);

  useEffect(() => {
    if (!activeSessionId && !useTabs.getState().active) createQuickSession();
  }, [activeSessionId]);

  return (
    <TooltipProvider delayDuration={250}>
      <AppFrame
        sidebar={<WorkspaceSidebar />}
        main={
          showConversation ? (
            <div className="flex h-full min-h-0 flex-1 flex-col overflow-hidden">
              <SessionHeader
                sessionId={composerSessionId}
                activeTab={sessionTab}
                onTabChange={setSessionTab}
              />
              <div className="relative flex min-h-0 flex-1 flex-col overflow-hidden">
                {sessionTab === "chat" ? (
                  <>
                    <ChatThread sessionId={composerSessionId} />
                    <ComposerBar sessionId={composerSessionId} />
                  </>
                ) : (
                  <TrajectoryView sessionId={composerSessionId} />
                )}
              </div>
            </div>
          ) : (
            <div className="flex h-full min-h-0 flex-1 flex-col overflow-hidden bg-background">
              <div className="flex min-h-0 flex-1 flex-col items-center justify-center overflow-y-auto px-6 py-8">
                <div className="flex w-full max-w-[816px] flex-col">
                  <div className="mb-7 flex flex-col items-center gap-3 text-center">
                    <div className="moc-empty-mark flex size-14 items-center justify-center rounded-2xl">
                      <BotIcon className="size-7" strokeWidth={1.7} />
                    </div>
                    <h1 className="text-[28px] font-semibold leading-tight text-foreground">
                      你好，今天想一起完成什么？
                    </h1>
                    <p className="text-sm text-[var(--text-tertiary)]">
                      描述目标或贴入问题，从这里开始你的工作。
                    </p>
                  </div>

                  <div className="mb-3 flex items-center gap-2">
                    <div
                      className="flex h-7 max-w-full items-center gap-1.5 rounded-md px-2 text-[13px] font-medium text-foreground"
                      title={workspacePath}
                    >
                      <FolderOpenIcon className="size-3.5 shrink-0 text-[var(--text-tertiary)]" />
                      <span className="max-w-[min(42vw,320px)] truncate">
                        {workspaceName(workspacePath)}
                      </span>
                    </div>
                  </div>

                  <ComposerBar sessionId={composerSessionId} inline />
                </div>
              </div>
            </div>
          )
        }
        rightbar={<InspectorPanel sessionId={composerSessionId} />}
      />

      <NewSessionDialog />
      <WorkspacePicker />
      <SettingsDialog />
      <CommandPalette />
      <Toaster />
    </TooltipProvider>
  );
}
