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
import { deleteSession } from "@/store/lifecycle";
import { useUi } from "@/store/ui";

export function DeleteSessionDialog() {
  const t = useT();
  const target = useUi((state) => state.confirmDelete);
  const [busy, setBusy] = useState(false);
  const close = () => useUi.getState().set({ confirmDelete: null });

  const remove = async () => {
    if (!target || busy) return;
    setBusy(true);
    try {
      await deleteSession(target.id);
      close();
      toast.success(t("session.deleted"));
    } catch (error) {
      toast.error(
        t("session.delete_failed", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <AlertDialog open={target !== null} onOpenChange={(open) => !open && !busy && close()}>
      <AlertDialogContent className="rounded-2xl border-0 shadow-[var(--elevation-dialog)]">
        <AlertDialogHeader>
          <AlertDialogTitle>{t("hd.delete")}</AlertDialogTitle>
          <AlertDialogDescription className="break-words">
            {t("session.delete_description", { title: target?.title || t("session.new_title") })}
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
