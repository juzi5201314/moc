// 会话级动作（重命名、删除、关闭、释放）与对应的确认对话框。
// 侧栏、标签栏、会话头共用同一套入口，对话框只在 AppShell 里挂一份。

import { useEffect, useState } from "react";
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
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import type { SessionInfo } from "@/protocol/types";
import { closeSession, deleteSession, renameSession } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 执行一个异步动作；成功时可选 toast，失败统一 toast 错误。 */
export async function runAction(task: () => Promise<unknown>, success?: string): Promise<void> {
  try {
    await task();
    if (success) toast.success(success);
  } catch (error) {
    toast.error(errorMessage(error));
  }
}

export type SessionActions = {
  rename: (session: SessionInfo) => void;
  remove: (session: SessionInfo) => void;
  /** 关闭标签：运行中先弹确认。 */
  close: (sessionId: string) => void;
  run: typeof runAction;
};

export function useSessionActions(): SessionActions {
  const set = useUi((state) => state.set);
  return {
    rename: (session) => set({ rename: session }),
    remove: (session) => set({ confirmDelete: session }),
    close: (sessionId) => {
      const view = useTranscript.getState().bySession[sessionId];
      if (view?.running && !view.readOnly) set({ confirmClose: sessionId });
      else void runAction(() => closeSession(sessionId));
    },
    run: runAction,
  };
}

function RenameDialog() {
  const target = useUi((state) => state.rename);
  const set = useUi((state) => state.set);
  const [title, setTitle] = useState("");

  useEffect(() => {
    if (target) setTitle(target.title);
  }, [target]);

  const close = () => set({ rename: null });
  const submit = () => {
    if (!target || !title.trim()) return;
    const id = target.id;
    close();
    void runAction(() => renameSession(id, title));
  };

  return (
    <Dialog open={target !== null} onOpenChange={(open) => !open && close()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>重命名会话</DialogTitle>
          <DialogDescription className="truncate">{target?.cwd}</DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
        >
          <Input
            aria-label="会话标题"
            autoFocus
            value={title}
            onChange={(event) => setTitle(event.target.value)}
          />
          <DialogFooter className="mt-4">
            <Button type="button" variant="ghost" onClick={close}>
              取消
            </Button>
            <Button type="submit" disabled={!title.trim()}>
              保存
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function DeleteDialog() {
  const target = useUi((state) => state.confirmDelete);
  const set = useUi((state) => state.set);
  const close = () => set({ confirmDelete: null });
  return (
    <AlertDialog open={target !== null} onOpenChange={(open) => !open && close()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>删除会话？</AlertDialogTitle>
          <AlertDialogDescription>
            「{target?.title || "未命名会话"}」的转写将被永久删除，无法恢复。
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction
            className="bg-destructive text-white hover:bg-destructive/90"
            onClick={() => {
              if (!target) return;
              const id = target.id;
              void runAction(() => deleteSession(id), "会话已删除");
            }}
          >
            删除
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function CloseRunningDialog() {
  const target = useUi((state) => state.confirmClose);
  const set = useUi((state) => state.set);
  const close = () => set({ confirmClose: null });
  return (
    <AlertDialog open={target !== null} onOpenChange={(open) => !open && close()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>会话正在运行</AlertDialogTitle>
          <AlertDialogDescription>关闭前会先停止当前生成。</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction
            onClick={() => {
              if (!target) return;
              void runAction(() => closeSession(target));
            }}
          >
            停止并关闭
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/** 会话级对话框，挂在 AppShell 里一份。 */
export function SessionDialogs() {
  return (
    <>
      <RenameDialog />
      <DeleteDialog />
      <CloseRunningDialog />
    </>
  );
}
