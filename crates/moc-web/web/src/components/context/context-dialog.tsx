// 「查看完整上下文」大模态：承载 ContextViewer（原检查面板上下文页）。

import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { useUi } from "@/store/ui";
import { ContextViewer } from "./context-viewer";

export function ContextDialog() {
  const sessionId = useUi((state) => state.contextDialog);
  return (
    <Dialog
      open={sessionId !== null}
      onOpenChange={(open) => !open && useUi.getState().set({ contextDialog: null })}
    >
      <DialogContent className="flex h-[85vh] w-[min(1100px,94vw)] max-w-none flex-col gap-0 overflow-hidden p-0">
        <DialogHeader className="px-6 pt-5 pb-3">
          <DialogTitle>上下文</DialogTitle>
        </DialogHeader>
        <div className="min-h-0 flex-1 overflow-y-auto">
          {sessionId && <ContextViewer key={sessionId} sessionId={sessionId} />}
        </div>
      </DialogContent>
    </Dialog>
  );
}
