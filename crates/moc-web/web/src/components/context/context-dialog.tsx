// 上下文总览与正文共用一个模态；进入目录后再扩大阅读区域。

import { useCallback, useRef } from "react";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useT } from "@/lib/i18n";
import { useUi } from "@/store/ui";
import { ContextViewer } from "./context-viewer";

export function ContextDialog() {
  const t = useT();
  const sessionId = useUi((state) => state.contextDialog);
  const back = useRef<(() => void) | null>(null);
  const onBackChange = useCallback((handler: (() => void) | null) => {
    back.current = handler;
  }, []);
  return (
    <Dialog
      open={sessionId !== null}
      onOpenChange={(open) => !open && useUi.getState().set({ contextDialog: null })}
    >
      <DialogContent
        className="block w-[min(420px,94vw)] max-w-none overflow-hidden p-0 sm:max-w-none has-[[data-context-detail=true]]:w-[min(760px,94vw)]"
        aria-describedby={undefined}
        onEscapeKeyDown={(event) => {
          if (back.current) {
            event.preventDefault();
            back.current();
          }
        }}
      >
        <DialogTitle className="sr-only">{t("cv.title")}</DialogTitle>
        {sessionId && (
          <ContextViewer key={sessionId} sessionId={sessionId} onBackChange={onBackChange} />
        )}
      </DialogContent>
    </Dialog>
  );
}
