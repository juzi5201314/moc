// 统一上下文面板：总览 → 来源目录 → 正文；返回恢复位置，不展示协议树。
import { ArrowLeftIcon, ChevronRightIcon, CopyIcon, RefreshCwIcon } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { PlanMarkdown } from "@/components/plan/plan-markdown";
import { Button } from "@/components/ui/button";
import { compactTokens } from "@/lib/models";
import { cn } from "@/lib/utils";
import type { ContextNode } from "@/protocol/types";
import { compactSession } from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { useContextView } from "./use-context-view";

type Location = { node: ContextNode | null; scroll: number; focus?: string };

export function ContextViewer({
  sessionId,
  onBackChange,
}: {
  sessionId: string;
  onBackChange?: (back: (() => void) | null) => void;
}) {
  const snapshot = useContextView(sessionId);
  const compactDisabled = useTranscript(
    (state) => !!state.bySession[sessionId]?.running || !!state.bySession[sessionId]?.readOnly,
  );
  const [navigation, setNavigation] = useState<{ current: Location; parents: Location[] }>({
    current: { node: null, scroll: 0 },
    parents: [],
  });
  const [raw, setRaw] = useState(false);
  const [copying, setCopying] = useState(false);
  const scroll = useRef<HTMLDivElement>(null);
  const location = navigation.current;
  const node = location.node;
  const page = node ? snapshot.pages.get(node.id) : undefined;
  const body = node !== null && node.kind !== "category";
  const rows = node ? (page?.result?.children ?? []) : (snapshot.view?.sections ?? []);
  const total = snapshot.view?.total_tokens ?? 0;
  const window = snapshot.view?.context_window ?? 0;

  useLayoutEffect(() => {
    if (!scroll.current) return;
    scroll.current.scrollTop = location.scroll;
    if (location.focus)
      scroll.current
        .querySelector<HTMLElement>(`[data-context-node="${CSS.escape(location.focus)}"]`)
        ?.focus({ preventScroll: true });
  }, [location]);

  const back = useCallback(() => {
    setNavigation((current) => {
      const parent = current.parents.at(-1);
      return parent ? { current: parent, parents: current.parents.slice(0, -1) } : current;
    });
  }, []);
  useEffect(() => {
    onBackChange?.(node ? back : null);
    return () => onBackChange?.(null);
  }, [node, back, onBackChange]);
  const enter = (next: ContextNode) => {
    if (next.hidden || (next.kind === "category" && next.children === 0)) return;
    setNavigation((current) => ({
      parents: [
        ...current.parents,
        { ...current.current, scroll: scroll.current?.scrollTop ?? 0, focus: next.id },
      ],
      current: { node: next, scroll: 0 },
    }));
    setRaw(next.kind === "json");
    if (!snapshot.pages.has(next.id)) void snapshot.load(next);
  };
  const refresh = () => {
    setNavigation({ current: { node: null, scroll: 0 }, parents: [] });
    snapshot.refresh();
  };
  const paginate = (cursor: number, previous: number[]) => {
    if (!node) return;
    if (scroll.current) scroll.current.scrollTop = 0;
    void snapshot.load(node, cursor, previous);
  };

  return (
    <div
      data-context-detail={node !== null}
      className={cn("flex max-h-[85dvh] flex-col", node ? "h-[78dvh]" : "")}
    >
      <div className="flex min-h-14 items-center gap-2 px-5 pt-3 pb-2 pr-12">
        {node && (
          <Button variant="ghost" size="icon-sm" aria-label="返回上一级" onClick={back}>
            <ArrowLeftIcon className="size-4" />
          </Button>
        )}
        <h2 className="min-w-0 flex-1 truncate text-sm font-medium">{node?.name ?? "上下文"}</h2>
        <Button
          variant="ghost"
          size="sm"
          disabled={snapshot.loading}
          onClick={refresh}
          aria-label={snapshot.stale ? "有更新，刷新上下文" : "刷新上下文"}
        >
          <RefreshCwIcon className={cn("size-3.5", snapshot.loading && "animate-spin")} />
          {snapshot.stale && <span className="text-xs">有更新</span>}
        </Button>
      </div>
      {!node && snapshot.view && (
        <div className="px-5 pb-3">
          <div className="flex items-baseline gap-2 font-mono tabular-nums">
            <span className="text-lg font-semibold">{compactTokens(total)}</span>
            <span className="text-sm text-muted-foreground">/ {compactTokens(window)}</span>
            <span className="ml-auto text-xs text-muted-foreground">
              {window > 0 ? `${((total / window) * 100).toFixed(1)}%` : "—"}
            </span>
          </div>
          <div className="mt-3 flex h-1 overflow-hidden rounded-full bg-muted" aria-hidden="true">
            {snapshot.view.sections.map((section, index) => (
              <div
                key={section.id}
                className={section.name === "messages" ? "bg-brand/60" : "bg-foreground/25"}
                style={{
                  width: `${window > 0 ? (section.tokens / window) * 100 : 0}%`,
                  opacity: section.name === "messages" ? 1 : 1 - index * 0.1,
                }}
              />
            ))}
          </div>
        </div>
      )}
      {body && node && (
        <div className="flex flex-wrap items-center gap-2 border-b px-5 pb-3">
          <span className="min-w-0 flex-1 break-all text-xs text-muted-foreground">
            {node.source ?? compactTokens(node.tokens)}
          </span>
          <Button size="sm" variant="ghost" onClick={() => setRaw((value) => !value)}>
            {raw ? "阅读" : "原文"}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={copying || !page?.result?.text}
            onClick={async () => {
              setCopying(true);
              try {
                await snapshot.copy(node);
                toast.success("已复制完整原文");
              } catch (error) {
                toast.error(error instanceof Error ? error.message : String(error));
              } finally {
                setCopying(false);
              }
            }}
          >
            <CopyIcon className="size-3.5" />
            复制原文
          </Button>
        </div>
      )}
      <div
        ref={scroll}
        className={cn("min-h-0 overflow-y-auto overscroll-contain px-5 pb-5", node && "flex-1")}
      >
        {snapshot.loading && <p className="py-5 text-sm text-muted-foreground">正在读取上下文…</p>}
        {(snapshot.error || snapshot.view?.error) && (
          <p role="alert" className="py-3 text-sm text-destructive">
            {snapshot.error ?? snapshot.view?.error}
          </p>
        )}
        {page?.error && (
          <div role="alert" className="py-3 text-sm text-destructive">
            {page.error}
            <Button
              variant="ghost"
              size="sm"
              onClick={() => node && void snapshot.load(node, page.cursor, page.previous)}
            >
              重试
            </Button>
          </div>
        )}
        {node && (!page || page.loading) && (
          <p className="py-4 text-sm text-muted-foreground">正在读取…</p>
        )}
        {!body &&
          rows.map((row) => {
            const disabled = !!row.hidden || (row.kind === "category" && row.children === 0);
            return (
              <button
                key={row.id}
                type="button"
                disabled={disabled}
                data-context-node={row.id}
                className="flex w-full items-start gap-3 rounded-md py-3 text-left transition-colors enabled:hover:bg-muted/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-default"
                onClick={() => enter(row)}
              >
                {!node && (
                  <span
                    className={cn(
                      "mt-1.5 size-1.5 shrink-0 rounded-full",
                      row.name === "messages" ? "bg-brand/60" : "bg-foreground/25",
                    )}
                  />
                )}
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm">{row.name}</span>
                  {(row.hidden || (node && row.source)) && (
                    <span
                      className="mt-1 block truncate text-xs text-muted-foreground"
                      title={row.hidden ?? row.source ?? undefined}
                    >
                      {row.hidden ?? row.source}
                    </span>
                  )}
                </span>
                <span className="pt-0.5 font-mono text-xs text-muted-foreground tabular-nums">
                  {compactTokens(row.tokens) || "0"}
                </span>
                {!node && (
                  <span className="w-10 pt-0.5 text-right font-mono text-xs text-muted-foreground tabular-nums">
                    {total > 0 ? `${Math.round((row.tokens / total) * 100)}%` : "0%"}
                  </span>
                )}
                <ChevronRightIcon
                  className={cn(
                    "mt-0.5 size-3.5 shrink-0 text-muted-foreground",
                    disabled && "invisible",
                  )}
                />
              </button>
            );
          })}
        {node && !body && page?.result && rows.length === 0 && (
          <p className="py-6 text-sm text-muted-foreground">暂无内容</p>
        )}
        {body &&
          page?.result?.text &&
          (raw ? (
            <pre className="whitespace-pre-wrap break-words py-4 font-mono text-xs leading-6">
              {page.result.text}
            </pre>
          ) : (
            <PlanMarkdown
              className="py-4"
              markdown={page.result.text.replace(/^([ \t]*<[^>\n]+>)[ \t]*$/gm, "`$1`")}
            />
          ))}
      </div>
      {!node && snapshot.view && (
        <div className="px-5 pb-4">
          <Button
            variant="outline"
            size="xs"
            disabled={compactDisabled}
            onClick={() => {
              void compactSession(sessionId)
                .then(() => toast.success("上下文已压缩，点击刷新查看新快照"))
                .catch((error) =>
                  toast.error(error instanceof Error ? error.message : String(error)),
                );
            }}
          >
            压缩
          </Button>
        </div>
      )}
      {node && page && (page.previous.length > 0 || page.result?.next_cursor != null) && (
        <div className="flex justify-between border-t px-5 py-2">
          <Button
            size="sm"
            variant="ghost"
            disabled={!page.previous.length || page.loading}
            onClick={() => {
              const cursor = page.previous.at(-1);
              if (cursor !== undefined) paginate(cursor, page.previous.slice(0, -1));
            }}
          >
            上一页
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={page.result?.next_cursor == null || page.loading}
            onClick={() => {
              if (page.result?.next_cursor != null)
                paginate(page.result.next_cursor, [...page.previous, page.cursor]);
            }}
          >
            下一页
          </Button>
        </div>
      )}
    </div>
  );
}
