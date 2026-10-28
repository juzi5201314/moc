import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { createQuickSession } from "@/store/lifecycle";
import { useUi } from "@/store/ui";

export function NewSessionDialog() {
  const { newSession: state, set } = useUi();
  const [cwd, setCwd] = useState("");

  const handleOpenChange = (open: boolean) => {
    set({ newSession: { open } });
  };

  const handleCreate = () => {
    set({ newSession: { open: false } });
    createQuickSession(cwd.trim() || undefined);
    setCwd("");
  };

  return (
    <Dialog open={state.open} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>新建会话</DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-2">
          <div className="space-y-1.5">
            <label htmlFor="session-cwd" className="text-xs font-medium text-muted-foreground">
              工作目录 (CWD)
            </label>
            <Input
              id="session-cwd"
              value={cwd}
              onChange={(e) => setCwd(e.target.value)}
              placeholder="留空使用当前项目目录..."
            />
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => set({ newSession: { open: false } })}>
            取消
          </Button>
          <Button onClick={handleCreate}>创建并打开</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
