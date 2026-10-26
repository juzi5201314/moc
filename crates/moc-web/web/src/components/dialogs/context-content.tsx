// 上下文正文：`session/context_content`，逐块折叠显示；截断时标注被丢弃的字节数。

import { ChevronRight } from "lucide-react";
import { useEffect, useState } from "react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { formatTokens } from "@/lib/format";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type { ContextContentResult } from "@/protocol/types";
import { useUi } from "@/store/ui";

const MAX_SECTION_BYTES = 65536;

export function ContextContentDialog() {
  const sessionId = useUi((state) => state.contextContent);
  const set = useUi((state) => state.set);
  const [result, setResult] = useState<ContextContentResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!sessionId) return;
    let alive = true;
    setResult(null);
    setError(null);
    rpc
      .call<ContextContentResult>("session/context_content", {
        session_id: sessionId,
        max_section_bytes: MAX_SECTION_BYTES,
      })
      .then((value) => alive && setResult(value))
      .catch((err: unknown) => {
        if (alive) setError(err instanceof RpcError ? err.message : String(err));
      });
    return () => {
      alive = false;
    };
  }, [sessionId]);

  return (
    <Dialog
      open={sessionId !== null}
      onOpenChange={(open) => !open && set({ contextContent: null })}
    >
      <DialogContent className="max-h-[85vh] overflow-hidden sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>上下文正文</DialogTitle>
          <DialogDescription>
            {result
              ? `${formatTokens(result.total_tokens)} / ${formatTokens(result.context_window)} tokens`
              : "正在读取…"}
          </DialogDescription>
        </DialogHeader>
        {error ? <p className="text-destructive text-sm">{error}</p> : null}
        <div className="max-h-[65vh] space-y-2 overflow-y-auto pr-1">
          {result?.sections.map((section) => (
            <Collapsible key={section.name} className="rounded-lg border">
              <CollapsibleTrigger className="group flex w-full items-center gap-2 px-3 py-2 text-left text-sm">
                <ChevronRight
                  className="size-4 transition-transform group-data-[state=open]:rotate-90"
                  aria-hidden
                />
                <span className="font-medium">{section.name}</span>
                <span className="ml-auto text-muted-foreground text-xs">
                  {formatTokens(section.tokens)} tokens
                  {section.truncated_bytes > 0 ? ` · 截断 ${section.truncated_bytes} 字节` : ""}
                </span>
              </CollapsibleTrigger>
              <CollapsibleContent>
                <pre className="max-h-96 overflow-auto whitespace-pre-wrap break-words border-t px-3 py-2 font-mono text-xs">
                  {section.text}
                </pre>
              </CollapsibleContent>
            </Collapsible>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}
