// 当前上下文只读视图：消息按真实协议顺序，目录虚拟化，正文按页读取。
import { useVirtualizer } from "@tanstack/react-virtual";
import { ChevronRightIcon, CopyIcon, RefreshCwIcon } from "lucide-react";
import { memo, useEffect, useMemo, useRef, useState } from "react";
import Markdown from "react-markdown";
import { Button } from "@/components/ui/button";
import { compactTokens } from "@/lib/models";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type { ContextContentResult, ContextNode, ContextReadResult } from "@/protocol/types";

const errorText = (error: unknown) => (error instanceof Error ? error.message : String(error));
type Page = {
  result?: ContextReadResult;
  cursor: number;
  previous: number[];
  error?: string;
  loading?: boolean;
};
type Row = { node: ContextNode; depth: number; body?: boolean };

export function ContextTab({ sessionId }: { sessionId: string }) {
  const [view, setView] = useState<ContextContentResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const [pages, setPages] = useState<Map<string, Page>>(() => new Map());
  const generation = useRef(0);
  const active = useRef<string | null>(null);
  const pending = useRef(new Set<string>());
  const scroll = useRef<HTMLDivElement>(null);

  // biome-ignore lint/correctness/useExhaustiveDependencies: refresh 是显式刷新代次，触发释放旧视图并重读。
  useEffect(() => {
    if (sessionId === "draft-session") return;
    const request = ++generation.current;
    let owned: string | null = null;
    const release = (viewId: string) => {
      void rpc
        .call("session/context_close", { session_id: sessionId, view_id: viewId })
        .catch(() => {});
    };
    setLoading(true);
    setError(null);
    setView(null);
    setOpen(new Set());
    setPages(new Map());
    pending.current.clear();
    void rpc
      .call<ContextContentResult>("session/context_content", { session_id: sessionId })
      .then((result) => {
        if (generation.current !== request) {
          release(result.view_id);
          return;
        }
        owned = result.view_id;
        active.current = owned;
        setView(result);
      })
      .catch((error) => {
        if (generation.current === request) setError(errorText(error));
      })
      .finally(() => {
        if (generation.current === request) setLoading(false);
      });
    return () => {
      generation.current++;
      active.current = null;
      if (owned) release(owned);
    };
  }, [sessionId, refresh]);

  const load = async (node: ContextNode, cursor = 0, previous: number[] = []) => {
    const viewId = active.current;
    if (!viewId || pending.current.has(node.id)) return;
    const request = generation.current;
    pending.current.add(node.id);
    setPages((current) =>
      new Map(current).set(node.id, { ...current.get(node.id), cursor, previous, loading: true }),
    );
    try {
      const result = await rpc.call<ContextReadResult>("session/context_read", {
        session_id: sessionId,
        view_id: viewId,
        node_id: node.id,
        cursor,
      });
      if (generation.current === request && active.current === viewId) {
        setPages((current) => new Map(current).set(node.id, { result, cursor, previous }));
      }
    } catch (error) {
      if (generation.current === request)
        setPages((current) =>
          new Map(current).set(node.id, { cursor, previous, error: errorText(error) }),
        );
    } finally {
      if (generation.current === request) pending.current.delete(node.id);
    }
  };
  const toggle = (node: ContextNode) => {
    const wasOpen = open.has(node.id);
    setOpen((current) => {
      const next = new Set(current);
      if (wasOpen) next.delete(node.id);
      else next.add(node.id);
      return next;
    });
    if (!wasOpen && !pages.has(node.id)) void load(node);
  };
  const more = async () => {
    if (!view || view.next_cursor === null || loading) return;
    const request = generation.current;
    setLoading(true);
    try {
      const result = await rpc.call<ContextContentResult>("session/context_content", {
        session_id: sessionId,
        view_id: view.view_id,
        cursor: view.next_cursor,
      });
      if (generation.current === request)
        setView(
          (current) =>
            current && {
              ...current,
              sections: [...current.sections, ...result.sections],
              next_cursor: result.next_cursor,
            },
        );
    } catch (error) {
      if (generation.current === request) setError(errorText(error));
    } finally {
      if (generation.current === request) setLoading(false);
    }
  };
  const rows = useMemo(() => {
    const result: Row[] = [];
    const visit = (nodes: ContextNode[], depth: number) => {
      for (const node of nodes) {
        result.push({ node, depth });
        if (!open.has(node.id)) continue;
        const page = pages.get(node.id);
        // 每个展开节点有一个状态/正文行；子节点维持服务端给出的原顺序。
        result.push({ node, depth, body: true });
        if (page?.result) visit(page.result.children, depth + 1);
      }
    };
    visit(view?.sections ?? [], 0);
    return result;
  }, [view, open, pages]);
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroll.current,
    estimateSize: (index) => (rows[index].body ? 96 : 54),
    overscan: 8,
    getItemKey: (index) => `${rows[index].node.id}:${rows[index].body ? "body" : "title"}`,
  });
  if (sessionId === "draft-session")
    return <p className="p-6 text-sm text-muted-foreground">发送第一条消息后可查看上下文</p>;
  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="shrink-0 border-b border-border-subtle px-4 py-3">
        <div className="flex items-center justify-between gap-2">
          <h3 className="text-sm font-medium">当前上下文</h3>
          <div className="flex gap-1">
            <Button size="xs" variant="ghost" onClick={() => setOpen(new Set())}>
              全部收起
            </Button>
            <Button
              size="xs"
              variant="ghost"
              aria-label="刷新上下文"
              onClick={() => setRefresh((value) => value + 1)}
            >
              <RefreshCwIcon className={cn("size-3", loading && "animate-spin")} />
            </Button>
          </div>
        </div>
        {view && (
          <>
            <p className="mt-2 truncate text-xs text-muted-foreground">
              {view.model} · {view.protocol}
            </p>
            <p className="mt-1 font-mono text-xs">
              约 {compactTokens(view.total_tokens)} / {compactTokens(view.context_window)} tokens
            </p>
          </>
        )}
        <p className="mt-2 text-2xs leading-4 text-text-tertiary">
          按当前配置与会话构造。消息保持实际顺序；工具与请求设置是独立字段。
        </p>
      </div>
      {(error || view?.error) && (
        <div
          role="alert"
          className="m-3 rounded border border-destructive/30 p-3 text-xs text-destructive"
        >
          {error || view?.error}
        </div>
      )}
      {!view && loading && <p className="p-4 text-xs text-muted-foreground">正在读取上下文…</p>}
      <section ref={scroll} className="min-h-0 flex-1 overflow-auto" aria-label="上下文消息列表">
        <div style={{ height: virtual.getTotalSize(), position: "relative" }}>
          {virtual.getVirtualItems().map((item) => {
            const row = rows[item.index];
            const node = row.node;
            const page = pages.get(node.id);
            return (
              <div
                key={item.key}
                data-index={item.index}
                ref={virtual.measureElement}
                className="absolute top-0 left-0 w-full"
                style={{ transform: `translateY(${item.start}px)` }}
              >
                {row.body ? (
                  <div
                    className="border-l border-border-subtle px-3 pb-3"
                    style={{ marginLeft: Math.min(row.depth * 12 + 14, 110) }}
                  >
                    {page?.loading ? (
                      <p className="py-2 text-xs text-muted-foreground">正在读取…</p>
                    ) : page?.error ? (
                      <div className="text-xs text-destructive">
                        {page.error}
                        <Button
                          size="xs"
                          variant="ghost"
                          onClick={() => void load(node, page.cursor, page.previous)}
                        >
                          重试
                        </Button>
                      </div>
                    ) : page?.result ? (
                      <>
                        <p className="mb-2 break-all font-mono text-2xs text-text-tertiary">
                          {node.path}
                          {node.text_range &&
                            ` · 原文字节 ${node.text_range[0]}–${node.text_range[1]}`}
                        </p>
                        {page.result.text !== null && (
                          <ContextText node={node} page={page.result} cursor={page.cursor} />
                        )}
                        {(page.previous.length > 0 || page.result.next_cursor !== null) && (
                          <div className="mt-2 flex gap-2">
                            <Button
                              size="xs"
                              variant="outline"
                              disabled={!page.previous.length}
                              onClick={() =>
                                void load(
                                  node,
                                  page.previous.at(-1) ?? 0,
                                  page.previous.slice(0, -1),
                                )
                              }
                            >
                              上一页
                            </Button>
                            <Button
                              size="xs"
                              variant="outline"
                              disabled={page.result.next_cursor === null}
                              onClick={() =>
                                void load(node, page.result?.next_cursor ?? 0, [
                                  ...page.previous,
                                  page.cursor,
                                ])
                              }
                            >
                              下一页
                            </Button>
                            <span className="self-center text-2xs text-text-tertiary">
                              {node.children
                                ? `子项 ${page.cursor + 1} 起`
                                : `字节 ${page.cursor} 起`}
                            </span>
                          </div>
                        )}
                      </>
                    ) : null}
                  </div>
                ) : (
                  <button
                    type="button"
                    aria-expanded={open.has(node.id)}
                    onClick={() => toggle(node)}
                    className="flex min-h-[54px] w-full items-start gap-2 border-b border-border-subtle py-2 pr-3 text-left hover:bg-surface-hover"
                    style={{ paddingLeft: Math.min(row.depth * 12 + 12, 108) }}
                  >
                    <ChevronRightIcon
                      className={cn(
                        "mt-1 size-3 shrink-0 transition-transform",
                        open.has(node.id) && "rotate-90",
                      )}
                    />
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-2">
                        {node.role && (
                          <span
                            className={cn(
                              "font-mono text-xs",
                              node.role === "assistant" ? "text-text-tertiary" : "text-brand",
                            )}
                          >
                            {node.role}
                          </span>
                        )}
                        <span className="truncate text-xs font-medium">{node.name}</span>
                      </span>
                      <span className="mt-1 block truncate text-2xs text-text-tertiary">
                        {node.source ?? (node.children ? `${node.children} 项` : node.kind)}
                      </span>
                    </span>
                    <span className="shrink-0 font-mono text-2xs text-muted-foreground">
                      {node.bytes.toLocaleString()} B
                    </span>
                  </button>
                )}
              </div>
            );
          })}
        </div>
        {view?.next_cursor !== null && view && (
          <Button
            variant="ghost"
            className="my-2 w-full"
            disabled={loading}
            onClick={() => void more()}
          >
            继续读取消息目录
          </Button>
        )}
      </section>
    </div>
  );
}

/** 默认精确保留原文；Markdown 作为可选阅读方式，复制始终复制原文字节对应的文本。 */
const ContextText = memo(function ContextText({
  node,
  page,
  cursor,
}: {
  node: ContextNode;
  page: ContextReadResult;
  cursor: number;
}) {
  const [rendered, setRendered] = useState(false);
  const [copyState, setCopyState] = useState("");
  const text = page.text ?? "";
  const opaque = /\/(signature|encrypted_content|data)$/.test(node.path);
  const image = text.startsWith("data:image/") && page.next_cursor === null && cursor === 0;
  const segments = useMemo(() => {
    const bytes = new TextEncoder().encode(text);
    const decoder = new TextDecoder();
    const parts: { text: string; source?: string; start: number }[] = [];
    let offset = 0;
    for (const source of page.sources) {
      const start = Math.max(offset, source.start - cursor, 0);
      const end = Math.min(bytes.length, source.end - cursor);
      if (start >= end) continue;
      if (start > offset)
        parts.push({ text: decoder.decode(bytes.subarray(offset, start)), start: offset });
      parts.push({
        text: decoder.decode(bytes.subarray(start, end)),
        source: source.source,
        start,
      });
      offset = end;
    }
    if (offset < bytes.length)
      parts.push({ text: decoder.decode(bytes.subarray(offset)), start: offset });
    return parts;
  }, [text, page.sources, cursor]);
  return (
    <div>
      {page.sources.map((source) => (
        <div
          key={`${source.source}:${source.start}`}
          className="mb-1 border-l-2 border-brand/60 pl-2 text-2xs text-muted-foreground"
        >
          {source.source} · 字节 {source.start}–{source.end}
        </div>
      ))}
      <div className="mb-2 flex flex-wrap items-center gap-2">
        {!opaque && (
          <Button size="xs" variant="ghost" onClick={() => setRendered((value) => !value)}>
            {rendered ? "原文" : "阅读视图"}
          </Button>
        )}
        <Button
          size="xs"
          variant="ghost"
          onClick={() => {
            void navigator.clipboard.writeText(text).then(
              () => setCopyState("已复制当前页"),
              () => setCopyState("复制失败"),
            );
          }}
        >
          <CopyIcon className="size-3" />
          复制当前页
        </Button>
        <span role="status" className="text-2xs text-muted-foreground">
          {copyState}
        </span>
      </div>
      {opaque && (
        <p className="mb-2 text-xs text-muted-foreground">
          不透明载荷 · 原文按页保留，不解释其含义
        </p>
      )}
      {image && (
        <img
          src={text}
          alt="当前请求中的图片"
          className="mb-2 max-h-64 max-w-full object-contain"
        />
      )}
      <div className="max-h-96 overflow-auto rounded bg-code p-3 text-xs leading-5">
        {rendered ? (
          <Markdown skipHtml={false}>{text}</Markdown>
        ) : (
          <pre className="break-words whitespace-pre-wrap font-mono">
            {text
              ? segments.map((segment) => (
                  <span
                    key={segment.start}
                    title={segment.source}
                    className={cn(
                      segment.source?.startsWith("运行时注入") && "bg-brand/10 text-foreground",
                      segment.source?.startsWith("系统提示") &&
                        "decoration-brand/30 underline decoration-dotted underline-offset-4",
                    )}
                  >
                    {segment.text}
                  </span>
                ))
              : "（空文本）"}
          </pre>
        )}
      </div>
      {page.next_cursor !== null && (
        <p className="mt-1 text-2xs text-muted-foreground">正文还有后续页；未截断源内容。</p>
      )}
    </div>
  );
});
