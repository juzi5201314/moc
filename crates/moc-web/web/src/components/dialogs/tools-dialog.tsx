import { useEffect, useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { rpc } from "@/protocol/client";
import type { ToolDefinition } from "@/protocol/types";

export function ToolsDialog({ sessionId, onClose }: { sessionId: string; onClose: () => void }) {
  const [tools, setTools] = useState<ToolDefinition[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    void rpc
      .call<ToolDefinition[]>("session/tools", { session_id: sessionId })
      .then((result) => {
        if (active) setTools(result);
      })
      .catch((failure: unknown) => {
        if (active) setError(failure instanceof Error ? failure.message : String(failure));
      });
    return () => {
      active = false;
    };
  }, [sessionId]);

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>可用工具{tools ? ` · ${tools.length}` : ""}</DialogTitle>
          <DialogDescription>按当前会话状态，下一轮请求将发送以下工具定义。</DialogDescription>
        </DialogHeader>
        <div className="min-h-0 overflow-y-auto">
          {error ? (
            <p role="alert" className="text-sm text-destructive">
              读取失败：{error}
            </p>
          ) : tools === null ? (
            <p role="status" className="text-sm text-muted-foreground">
              正在读取工具…
            </p>
          ) : tools.length === 0 ? (
            <p className="text-sm text-muted-foreground">当前请求不发送任何工具。</p>
          ) : (
            <dl className="flex flex-col gap-4">
              {tools.map((tool) => (
                <div key={tool.name} className="flex flex-col gap-1">
                  <dt className="break-all font-mono text-sm font-medium">{tool.name}</dt>
                  {tool.description ? (
                    <dd className="whitespace-pre-wrap break-words text-sm text-muted-foreground">
                      {tool.description}
                    </dd>
                  ) : null}
                </div>
              ))}
            </dl>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
