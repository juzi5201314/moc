import { Loader2Icon } from "lucide-react";
import { useEffect } from "react";
import { ChatThread } from "@/components/chat/chat-thread";
import { ComposerBar } from "@/components/chat/composer-bar";
import { SessionHeader } from "@/components/chat/session-header";
import { WelcomeView } from "@/components/chat/welcome";
import { CommandPalette } from "@/components/dialogs/command-palette";
import { JobsDialog } from "@/components/dialogs/jobs-dialog";
import { ToolsDialog } from "@/components/dialogs/tools-dialog";
import { WorkspacePicker } from "@/components/dialogs/workspace-picker";
import { AppFrame } from "@/components/layout/AppFrame";
import { InspectorPanel } from "@/components/panels/inspector-panel";
import { SettingsDialog } from "@/components/settings/settings-dialog";
import { WorkspaceSidebar } from "@/components/sidebar/workspace-sidebar";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { WorkflowApprovalDialog } from "@/components/workflow/workflow-approval-dialog";
import { WorkflowRunPanel } from "@/components/workflow/workflow-run-panel";
import { useGlobalShortcuts } from "@/lib/shortcuts";
import { useThemeSync } from "@/lib/theme";
import { rpc } from "@/protocol/client";
import { createQuickSession } from "@/store/lifecycle";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { useWorkflow } from "@/store/workflow";
import "@/store/connection";
import "@/store/dispatch";

export function App() {
  useThemeSync();
  useGlobalShortcuts();

  const jobsSession = useUi((state) => state.jobsSession);
  const toolsSession = useUi((state) => state.toolsSession);
  const wfApproval = useWorkflow((s) => s.approvals[0] ?? null);
  const wfApprovalOpen = useWorkflow((s) => s.approvalOpen);

  const active = useTabs((state) => state.active);
  const sessionId = active ?? "draft-session";
  // 草稿与还没有任何消息的会话走欢迎页；真实会话还在加载时显示占位。
  const stage = useTranscript((state) => {
    const view = state.bySession[sessionId];
    if (!view) return sessionId === "draft-session" ? "welcome" : "loading";
    return view.blocks.length === 0 && !view.running && !view.hasMore ? "welcome" : "chat";
  });

  useEffect(() => {
    rpc.connect();
    return () => rpc.dispose();
  }, []);

  useEffect(() => {
    if (!active) createQuickSession();
  }, [active]);

  return (
    <TooltipProvider delayDuration={300}>
      <AppFrame
        sidebar={<WorkspaceSidebar />}
        main={
          stage === "welcome" ? (
            <WelcomeView sessionId={sessionId} />
          ) : stage === "loading" ? (
            <div className="flex h-full flex-1 items-center justify-center">
              <Loader2Icon className="size-5 animate-spin text-text-tertiary" />
            </div>
          ) : (
            <div className="flex h-full min-h-0 flex-1 flex-col">
              <SessionHeader sessionId={sessionId} />
              <ChatThread sessionId={sessionId} />
              <ComposerBar sessionId={sessionId} />
            </div>
          )
        }
        rightbar={<InspectorPanel sessionId={sessionId} />}
      />
      <WorkflowApprovalDialog
        sessionId={sessionId}
        approval={wfApproval}
        open={wfApprovalOpen && wfApproval !== null}
        onOpenChange={(open) => useWorkflow.setState({ approvalOpen: open })}
      />
      <WorkflowRunPanel sessionId={sessionId} />
      <WorkspacePicker />
      <SettingsDialog />
      <CommandPalette />
      {jobsSession && (
        <JobsDialog
          sessionId={jobsSession}
          open
          onOpenChange={(open) => {
            if (!open) useUi.getState().set({ jobsSession: null });
          }}
        />
      )}
      {toolsSession ? (
        <ToolsDialog
          key={toolsSession}
          sessionId={toolsSession}
          onClose={() => useUi.getState().set({ toolsSession: null })}
        />
      ) : null}
      <Toaster />
    </TooltipProvider>
  );
}
