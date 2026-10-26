// 会话状态标记：运行中 / 待处理 / 只读。状态同时配图标与文字（sr-only），不只靠颜色。

import { CircleAlert, Loader2, Lock } from "lucide-react";
import { cn } from "@/lib/utils";
import { attentionCount, useTranscript } from "@/store/transcript";

export type SessionStatusKind = "running" | "attention" | "readonly" | "idle";

export function useSessionStatus(sessionId: string): SessionStatusKind {
  return useTranscript((state) => {
    if (attentionCount(state, sessionId) > 0) return "attention";
    if (state.activity[sessionId]?.running || state.bySession[sessionId]?.running) return "running";
    if (state.bySession[sessionId]?.readOnly) return "readonly";
    return "idle";
  });
}

export function SessionStatusIcon({
  sessionId,
  className,
}: {
  sessionId: string;
  className?: string;
}) {
  const status = useSessionStatus(sessionId);
  if (status === "running") {
    return (
      <span className={cn("inline-flex text-brand", className)}>
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        <span className="sr-only">运行中</span>
      </span>
    );
  }
  if (status === "attention") {
    return (
      <span className={cn("inline-flex text-warn", className)}>
        <CircleAlert className="size-3.5" aria-hidden />
        <span className="sr-only">需要处理</span>
      </span>
    );
  }
  if (status === "readonly") {
    return (
      <span className={cn("inline-flex text-muted-foreground", className)}>
        <Lock className="size-3" aria-hidden />
        <span className="sr-only">只读</span>
      </span>
    );
  }
  return null;
}
