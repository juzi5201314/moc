// `session/search` 的防抖封装：query 满 2 字后延迟发 RPC，
// 期间的新输入作废旧计时与旧响应，结果只属于当前 query。

import { useEffect, useRef, useState } from "react";
import { rpc } from "@/protocol/client";
import type { SessionSearchHit, SessionSearchResult } from "@/protocol/types";

/** 低于这个长度不发正文搜索：单字匹配面太大且没有信息量。 */
export const SEARCH_MIN_CHARS = 2;
const DEBOUNCE_MS = 250;

export interface SessionSearchState {
  /** 当前 query 的内容命中；query 变更期间返回空（旧结果不沿用）。 */
  hits: SessionSearchHit[];
  /** 有未返回的搜索请求。 */
  pending: boolean;
}

export function useSessionSearch(query: string, limit: number): SessionSearchState {
  const needle = query.trim();
  const [result, setResult] = useState<{ needle: string; hits: SessionSearchHit[] } | null>(null);
  const [pending, setPending] = useState(false);
  // generation 作废在途响应：防抖计时与 RPC 完成都按它判定归属。
  const generation = useRef(0);

  useEffect(() => {
    const mine = ++generation.current;
    if (needle.length < SEARCH_MIN_CHARS) {
      setResult(null);
      setPending(false);
      return;
    }
    setPending(true);
    const timer = setTimeout(() => {
      rpc
        .call<SessionSearchResult>("session/search", { query: needle, limit })
        .then((res) => {
          if (generation.current === mine) setResult({ needle, hits: res.matches });
        })
        .catch(() => {
          if (generation.current === mine) setResult({ needle, hits: [] });
        })
        .finally(() => {
          if (generation.current === mine) setPending(false);
        });
    }, DEBOUNCE_MS);
    return () => {
      generation.current += 1;
      clearTimeout(timer);
    };
  }, [needle, limit]);

  return { hits: result?.needle === needle ? result.hits : [], pending };
}
