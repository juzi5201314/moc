import { useState } from "react";
import { toast } from "sonner";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { useT } from "@/lib/i18n";
import { deleteSessions } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { allSessions, useSessions, workspaceName } from "@/store/sessions";
import { useUi } from "@/store/ui";

export function DeleteWorkspaceDialog() {
  const t = useT();
  const target = useUi((state) => state.confirmDeleteWorkspace);
  const count = useSessions((state) =>
    target ? allSessions(state).filter((session) => session.cwd === target.cwd).length : 0,
  );
  const [busy, setBusy] = useState(false);
  const close = () => useUi.getState().set({ confirmDeleteWorkspace: null });

  const remove = async () => {
    if (!target || busy) return;
    setBusy(true);
    const ids = allSessions(useSessions.getState())
      .filter((session) => session.cwd === target.cwd)
      .map((session) => session.id);
    const failed = await deleteSessions(ids);
    const { workspaces, removeWorkspace } = usePrefs.getState();
    if (workspaces.includes(target.cwd)) removeWorkspace(target.cwd);
    setBusy(false);
    close();
    if (failed === 0) {
      toast.success(t("ws.deleted", { n: ids.length }));
    } else {
      toast.error(t("ws.delete_partial", { ok: ids.length - failed, n: failed }));
    }
  };

  return (
    <AlertDialog open={target !== null} onOpenChange={(open) => !open && !busy && close()}>
      <AlertDialogContent className="rounded-2xl border-0 shadow-[var(--elevation-dialog)]">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("sb.delete_ws")}</AlertDialogTitle>
          <AlertDialogDescription className="break-words">
            {t("ws.delete_description", {
              name: target ? workspaceName(target.cwd) : "",
              n: count,
            })}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={busy}>{t("common.cancel")}</AlertDialogCancel>
          <AlertDialogAction
            variant="destructive"
            disabled={busy}
            onClick={(event) => {
              event.preventDefault();
              void remove();
            }}
          >
            {t("common.delete")}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
