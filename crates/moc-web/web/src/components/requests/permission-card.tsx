// 权限卡片：停在 composer 上方（不用模态）。一个会话的多个请求排队，一次只显示队首。
// 快捷键：1..n 选对应项；Esc 选 reject_once（没有就选第一项）。默认焦点在 allow_once。

import { ShieldAlert } from "lucide-react";
import { useEffect, useRef } from "react";
import { DiffView } from "@/components/transcript/diff-view";
import { Button } from "@/components/ui/button";
import type { PermissionOption, PermissionRequestParams } from "@/protocol/types";
import { usePrefs } from "@/store/prefs";
import { resolvePermission, useTranscript } from "@/store/transcript";
import { requestComposerFocus } from "@/store/ui";

const EMPTY: PermissionRequestParams[] = [];

const VARIANT: Record<PermissionOption["kind"], "default" | "secondary" | "outline"> = {
  allow_once: "default",
  allow_session: "secondary",
  reject_once: "outline",
};

export function PermissionCard({ sessionId }: { sessionId: string }) {
  const queue = useTranscript((state) => state.permissions[sessionId] ?? EMPTY);
  const request = queue[0];
  if (!request) return null;
  return <PermissionCardInner key={request.request_id} request={request} more={queue.length - 1} />;
}

function PermissionCardInner({
  request,
  more,
}: {
  request: PermissionRequestParams;
  more: number;
}) {
  const diffMode = usePrefs((state) => state.diffMode);
  const primary = useRef<HTMLButtonElement>(null);
  const card = useRef<HTMLDivElement>(null);

  const choose = (option: PermissionOption | undefined) => {
    if (!option) return;
    resolvePermission(request.session_id, request.request_id, option.id);
    requestComposerFocus();
  };

  useEffect(() => {
    primary.current?.focus();
  }, []);

  const onKeyDown = (event: React.KeyboardEvent) => {
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    // 输入框里打字时不抢数字键
    if (event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      choose(request.options.find((item) => item.kind === "reject_once") ?? request.options[0]);
      return;
    }
    const index = Number(event.key) - 1;
    if (Number.isInteger(index) && index >= 0 && index < request.options.length) {
      event.preventDefault();
      choose(request.options[index]);
    }
  };

  const defaultIndex = Math.max(
    0,
    request.options.findIndex((item) => item.kind === "allow_once"),
  );

  return (
    <section
      ref={card}
      aria-label="需要授权"
      onKeyDown={onKeyDown}
      className="mx-auto mb-2 w-full max-w-[780px] rounded-xl border border-warn/50 bg-card p-3 shadow-sm"
    >
      <header className="mb-2 flex items-center gap-2 text-sm">
        <ShieldAlert className="size-4 text-warn" aria-hidden />
        <span className="font-medium">{request.title}</span>
        <span className="text-muted-foreground text-xs">{request.tool_name}</span>
        {more > 0 ? (
          <span className="ml-auto text-muted-foreground text-xs">还有 {more} 个待授权</span>
        ) : null}
      </header>
      {request.body ? (
        <pre className="mb-2 max-h-48 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-muted p-2 font-mono text-xs">
          {request.body}
        </pre>
      ) : null}
      {request.diff ? (
        <DiffView diff={request.diff} mode={diffMode} className="mb-2 max-h-72 overflow-y-auto" />
      ) : null}
      <div className="flex flex-wrap gap-2">
        {request.options.map((option, index) => (
          <Button
            key={option.id}
            ref={index === defaultIndex ? primary : undefined}
            size="sm"
            variant={VARIANT[option.kind]}
            className={
              option.kind === "reject_once"
                ? "border-destructive/60 text-destructive hover:bg-destructive/10"
                : undefined
            }
            onClick={() => choose(option)}
          >
            <kbd className="rounded border px-1 font-mono text-[10px] opacity-70">{index + 1}</kbd>
            {option.label}
          </Button>
        ))}
      </div>
    </section>
  );
}
