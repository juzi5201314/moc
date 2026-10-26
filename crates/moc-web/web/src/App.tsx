import { useEffect } from "react";
import { Composer } from "@/components/composer/composer";
import { CommandPalette } from "@/components/dialogs/command-palette";
import { ContextContentDialog } from "@/components/dialogs/context-content";
import { FolderPicker } from "@/components/dialogs/folder-picker";
import { NewSessionDialog } from "@/components/dialogs/new-session";
import { PlanReviewDialog } from "@/components/dialogs/plan-review";
import { PluginsDialog } from "@/components/dialogs/plugins-dialog";
import { SettingsDialog } from "@/components/dialogs/settings-dialog";
import { ShortcutHelp } from "@/components/dialogs/shortcut-help";
import { UsageDialog } from "@/components/dialogs/usage-dialog";
import { AppShell } from "@/components/layout/app-shell";
import { TranscriptView } from "@/components/transcript/transcript-view";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useGlobalShortcuts } from "@/lib/shortcuts";
import { useThemeSync } from "@/lib/theme";
import { rpc } from "@/protocol/client";
import "@/components/layout/right-panes";
import "@/store/connection";
import "@/store/dispatch";
import "@/store/lifecycle";

export function App() {
  useThemeSync();
  useGlobalShortcuts();
  useEffect(() => {
    // StrictMode 会挂载两次：清理时关掉上一条连接，避免残留一条已握手的 WS。
    rpc.connect();
    return () => rpc.dispose();
  }, []);

  return (
    <TooltipProvider delayDuration={300}>
      <AppShell
        main={(sessionId) =>
          sessionId ? (
            <div key={sessionId} className="flex min-h-0 flex-1 flex-col">
              <TranscriptView sessionId={sessionId} />
              <Composer sessionId={sessionId} />
            </div>
          ) : (
            <div className="flex flex-1 items-center justify-center text-muted-foreground text-sm">
              从左侧选择或新建一个会话
            </div>
          )
        }
      />
      <NewSessionDialog />
      <FolderPicker />
      <PlanReviewDialog />
      <UsageDialog />
      <PluginsDialog />
      <ContextContentDialog />
      <SettingsDialog />
      <CommandPalette />
      <ShortcutHelp />
      <Toaster />
    </TooltipProvider>
  );
}
