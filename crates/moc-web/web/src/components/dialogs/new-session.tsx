// 新建会话：工作区（已有工作区 + 「浏览…」）、模式、模型（默认 config.default_model）。

import { useEffect, useMemo, useState } from "react";
import { runAction } from "@/components/layout/session-actions";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { SessionMode } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { newSession } from "@/store/lifecycle";
import { allSessions, groupWorkspaces, useSessions, workspaceName } from "@/store/sessions";
import { useUi } from "@/store/ui";

const BROWSE = "__browse__";

const MODES: { value: SessionMode; label: string }[] = [
  { value: "normal", label: "标准" },
  { value: "ask", label: "问答" },
  { value: "plan", label: "计划" },
  { value: "ptc", label: "PTC" },
];

export function NewSessionDialog() {
  const state = useUi((s) => s.newSession);
  const set = useUi((s) => s.set);
  const config = useConnection((s) => s.config);
  const list = useSessions((s) => s.list);
  const extra = useSessions((s) => s.extra);
  const pinned = useSessions((s) => s.pinned);
  const pin = useSessions((s) => s.pin);

  const [cwd, setCwd] = useState("");
  const [mode, setMode] = useState<SessionMode>("normal");
  const [model, setModel] = useState("");
  const [busy, setBusy] = useState(false);

  const workspaces = useMemo(
    () => groupWorkspaces(allSessions({ list, extra }), pinned).map((group) => group.cwd),
    [list, extra, pinned],
  );

  // biome-ignore lint/correctness/useExhaustiveDependencies: 只在打开时取默认值，其余依赖变化不应覆盖用户输入
  useEffect(() => {
    if (!state.open) return;
    setCwd(state.cwd ?? workspaces[0] ?? config?.cwd ?? "");
    setMode("normal");
    setModel(config?.default_model ?? config?.models[0]?.id ?? "");
  }, [state.open]);

  const options = cwd && !workspaces.includes(cwd) ? [cwd, ...workspaces] : workspaces;
  const close = () => set({ newSession: { open: false } });

  const browse = () =>
    set({
      folderPicker: {
        open: true,
        start: cwd || config?.cwd,
        onPick: (path) => {
          pin(path);
          setCwd(path);
        },
      },
    });

  const submit = async () => {
    setBusy(true);
    await runAction(async () => {
      await newSession({ cwd: cwd || undefined, model: model || undefined, mode });
      close();
    });
    setBusy(false);
  };

  return (
    <Dialog open={state.open} onOpenChange={(open) => !open && close()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>新建会话</DialogTitle>
          <DialogDescription>选择工作区、模式和模型。</DialogDescription>
        </DialogHeader>
        <div className="grid gap-3 text-sm">
          <div className="grid gap-1.5">
            <span id="new-session-cwd" className="text-muted-foreground">
              工作区
            </span>
            <Select
              value={cwd}
              onValueChange={(value) => (value === BROWSE ? browse() : setCwd(value))}
            >
              <SelectTrigger aria-labelledby="new-session-cwd" className="w-full">
                <SelectValue placeholder="选择工作区" />
              </SelectTrigger>
              <SelectContent>
                {options.map((item) => (
                  <SelectItem key={item} value={item} title={item}>
                    {workspaceName(item)}
                    <span className="ml-2 truncate text-muted-foreground text-xs">{item}</span>
                  </SelectItem>
                ))}
                <SelectItem value={BROWSE}>浏览…</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="grid gap-1.5">
            <span id="new-session-mode" className="text-muted-foreground">
              模式
            </span>
            <Select value={mode} onValueChange={(value) => setMode(value as SessionMode)}>
              <SelectTrigger aria-labelledby="new-session-mode" className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {MODES.map((item) => (
                  <SelectItem key={item.value} value={item.value}>
                    {item.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="grid gap-1.5">
            <span id="new-session-model" className="text-muted-foreground">
              模型
            </span>
            <Select value={model} onValueChange={setModel}>
              <SelectTrigger aria-labelledby="new-session-model" className="w-full">
                <SelectValue placeholder="默认模型" />
              </SelectTrigger>
              <SelectContent>
                {(config?.models ?? []).map((item) => (
                  <SelectItem key={item.id} value={item.id}>
                    {item.display_name || item.id}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={close}>
            取消
          </Button>
          <Button onClick={() => void submit()} disabled={busy}>
            创建
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
