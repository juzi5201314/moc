import { Loader2Icon } from "lucide-react";
import { lazy, Suspense, useEffect } from "react";
import { ChatThread } from "@/components/chat/chat-thread";
import { ComposerBar } from "@/components/chat/composer-bar";
import { SessionHeader } from "@/components/chat/session-header";
import { WelcomeView } from "@/components/chat/welcome";
import { ContextDialog } from "@/components/context/context-dialog";
import { CommandPalette } from "@/components/dialogs/command-palette";
import { DeleteSessionDialog } from "@/components/dialogs/delete-session-dialog";
import { DeleteWorkspaceDialog } from "@/components/dialogs/delete-workspace-dialog";
import { DoctorDialog } from "@/components/dialogs/doctor-dialog";
import { FeaturesDialog } from "@/components/dialogs/features-dialog";
import { JobsDialog } from "@/components/dialogs/jobs-dialog";
import { MonitorDialog } from "@/components/dialogs/monitor-dialog";
import { ToolsDialog } from "@/components/dialogs/tools-dialog";
import { WorkspacePicker } from "@/components/dialogs/workspace-picker";
import { AppFrame } from "@/components/layout/AppFrame";
import { InspectorPanel } from "@/components/panels/inspector-panel";
import { PluginsPage } from "@/components/settings/plugins-page";
import { SettingsDialog } from "@/components/settings/settings-dialog";
import { WorkspaceSidebar } from "@/components/sidebar/workspace-sidebar";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { WorkflowApprovalDialog } from "@/components/workflow/workflow-approval-dialog";
import { WorkflowRunPanel } from "@/components/workflow/workflow-run-panel";
import { useRoute } from "@/lib/route";
import { useGlobalShortcuts } from "@/lib/shortcuts";
import { useThemeSync } from "@/lib/theme";
import { rpc } from "@/protocol/client";
import { createQuickSession } from "@/store/lifecycle";
import { useSessionMeta } from "@/store/session-meta";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { useWorkflow } from "@/store/workflow";
import "@/store/connection";
import "@/store/dispatch";

const ImagePlaygroundApp = lazy(() => import("@/components/image-playground/playground-app"));

export function App() {
  useThemeSync();
  useGlobalShortcuts();
  // files store 只读 cwd，避免 store 间循环依赖。

  const route = useRoute();
  const inPlayground = route === "image";

  const jobsSession = useUi((state) => state.jobsSession);
  const toolsSession = useUi((state) => state.toolsSession);
  const featuresOpen = useUi((state) => state.features);
  const doctorOpen = useUi((state) => state.doctor);
  const monitorOpen = useUi((state) => state.monitor);
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
    if (!active && !inPlayground) createQuickSession();
  }, [active, inPlayground]);

  useEffect(() => {
    if (route === "agent" && active && active !== "draft-session") {
      useSessionMeta.getState().markRead(active);
    }
  }, [route, active]);

  return (
    <TooltipProvider delayDuration={300}>
      {inPlayground ? (
        <Suspense
          fallback={
            <div className="flex h-dvh w-screen items-center justify-center bg-background">
              <Loader2Icon className="size-5 animate-spin text-text-tertiary" />
            </div>
          }
        >
          <ImagePlaygroundApp />
        </Suspense>
      ) : route === "plugins" ? (
        <AppFrame main={<PluginsPage />} rightbar={null} />
      ) : (
        <>
          <AppFrame
            sidebar={<WorkspaceSidebar />}
            location={sessionId}
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
          <CommandPalette />
          <DeleteSessionDialog />
          <DeleteWorkspaceDialog />
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
          {featuresOpen && <FeaturesDialog />}
          {doctorOpen && <DoctorDialog />}
          {monitorOpen && <MonitorDialog />}
          <ContextDialog />
        </>
      )}
      <SettingsDialog />
      <Toaster />
    </TooltipProvider>
  );
}
