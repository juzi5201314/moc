import { BotIcon, FolderOpenIcon, SparklesIcon } from "lucide-react";
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
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden bg-background">
      <div className="border-b border-border/70 px-6 py-4">
        <div className="text-sm font-semibold text-foreground">执行轨迹</div>
        <div className="mt-1 text-xs text-muted-foreground">
          按会话顺序查看提示、思考、工具和结果。
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-6 py-5">
        {blocks.length === 0 ? (
          <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
            发送第一条提示后，这里会显示真实执行事件。
          </div>
        ) : (
          <ol className="mx-auto max-w-3xl space-y-3">
            {blocks.map((block) => (
              <li
                key={block.id}
                className="relative rounded-lg border border-border/70 bg-card px-4 py-3"
              >
                <div className="flex items-center justify-between gap-3 text-xs">
                  <span className="font-medium text-foreground">{trajectoryLabel(block)}</span>
                  {"atMs" in block ? (
                    <time className="font-mono text-[10px] text-muted-foreground">
                      {clockTime(block.atMs)}
                    </time>
                  ) : null}
                </div>
                <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-5 text-muted-foreground">
                  {trajectoryBody(block)}
                </pre>
              </li>
            ))}
          </ol>
        )}
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
          hasConversation ? (
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
              <div className="flex min-h-0 flex-1 items-center justify-center overflow-y-auto px-6 py-8">
                <section className="flex w-full max-w-2xl flex-col items-center text-center">
                  <div className="moc-empty-mark mb-6 flex size-12 items-center justify-center rounded-2xl">
                    <BotIcon className="size-6" strokeWidth={1.7} />
                  </div>
                  <div className="mb-7 max-w-xl space-y-2">
                    <div className="flex items-center justify-center gap-1.5 text-[11px] font-semibold uppercase text-primary">
                      <SparklesIcon className="size-3.5" />
                      <span>moc 智能工作台</span>
                    </div>
                    <h1 className="text-2xl font-semibold leading-tight text-foreground sm:text-[28px]">
                      你好，今天想一起完成什么？
                    </h1>
                    <p className="text-sm leading-6 text-muted-foreground">
                      描述目标或贴入问题，从这里开始你的工作。
                    </p>
                  </div>
                  <div
                    className="moc-workspace-chip flex max-w-full items-center gap-2.5 rounded-lg px-3 py-2"
                    title={workspacePath}
                  >
                    <FolderOpenIcon className="size-4 shrink-0 text-primary" />
                    <span className="text-xs text-muted-foreground">工作区</span>
                    <span className="max-w-[min(42vw,320px)] truncate text-xs font-medium text-foreground">
                      {workspaceName(workspacePath)}
                    </span>
                    <span className="size-1.5 shrink-0 rounded-full bg-emerald-500" />
                  </div>
                </section>
              </div>
              <ComposerBar sessionId={composerSessionId} />
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
