// 快照生命周期与分页：刷新才替换内容，旧请求不能写入新视图。
import { useEffect, useRef, useState } from "react";
import { rpc } from "@/protocol/client";
import type { ContextContentResult, ContextNode, ContextReadResult } from "@/protocol/types";
import { useConnection } from "@/store/connection";

export type ContextPage = {
  result?: ContextReadResult;
  cursor: number;
  previous: number[];
  loading?: boolean;
  error?: string;
};

export function useContextView(sessionId: string) {
  const [view, setView] = useState<ContextContentResult | null>(null);
  const [pages, setPages] = useState<Map<string, ContextPage>>(() => new Map());
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [stale, setStale] = useState(false);
  const [generation, setGeneration] = useState(0);
  const active = useRef<string | null>(null);
  const epoch = useRef(0);
  const pending = useRef(new Set<string>());

  useEffect(() => {
    const unsubscribe = rpc.on("session/update", (params) => {
      if (params.session_id === sessionId) setStale(true);
    });
    const unsubscribeConfig = useConnection.subscribe((current, previous) => {
      if (current.config !== previous.config) setStale(true);
    });
    return () => {
      unsubscribe();
      unsubscribeConfig();
    };
  }, [sessionId]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: generation 是用户主动刷新代次。
  useEffect(() => {
    if (sessionId === "draft-session") return;
    const request = ++epoch.current;
    let owned: string | null = null;
    const release = (id: string) => {
      void rpc
        .call("session/context_close", { session_id: sessionId, view_id: id })
        .catch(() => {});
    };
    setView(null);
    setPages(new Map());
    setError(null);
    setLoading(true);
    setStale(false);
    pending.current.clear();
    void rpc
      .call<ContextContentResult>("session/context_content", { session_id: sessionId })
      .then((result) => {
        if (epoch.current !== request) {
          release(result.view_id);
          return;
        }
        owned = result.view_id;
        active.current = owned;
        setView(result);
      })
      .catch((reason) => {
        if (epoch.current === request)
          setError(reason instanceof Error ? reason.message : String(reason));
      })
      .finally(() => {
        if (epoch.current === request) setLoading(false);
      });
    return () => {
      epoch.current++;
      active.current = null;
      if (owned) release(owned);
    };
  }, [sessionId, generation]);

  const load = async (node: ContextNode, cursor = 0, previous: number[] = []) => {
    const viewId = active.current;
    if (!viewId || node.hidden || pending.current.has(node.id)) return;
    const request = epoch.current;
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
      if (epoch.current === request)
        setPages((current) => new Map(current).set(node.id, { result, cursor, previous }));
    } catch (reason) {
      if (epoch.current === request)
        setPages((current) =>
          new Map(current).set(node.id, {
            cursor,
            previous,
            error: reason instanceof Error ? reason.message : String(reason),
          }),
        );
    } finally {
      if (epoch.current === request) pending.current.delete(node.id);
    }
  };

  const copy = async (node: ContextNode) => {
    const viewId = active.current;
    if (!viewId || node.hidden || node.kind === "category") return;
    let cursor: number | null = 0;
    const chunks: string[] = [];
    do {
      const result: ContextReadResult = await rpc.call("session/context_read", {
        session_id: sessionId,
        view_id: viewId,
        node_id: node.id,
        cursor,
      });
      if (active.current !== viewId) return;
      chunks.push(result.text ?? "");
      cursor = result.next_cursor ?? null;
    } while (cursor !== null);
    await navigator.clipboard.writeText(chunks.join(""));
  };

  return {
    view,
    pages,
    error,
    loading,
    stale,
    load,
    copy,
    refresh: () => setGeneration((value) => value + 1),
  };
}
