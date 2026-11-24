// `resource/inspect` 的会话级缓存：输入框与用户消息共用，同一 token 30 秒内只查一次，
// 同一轮渲染里新出现的 token 合并成一次批量请求。

import { useEffect, useSyncExternalStore } from "react";
import { rpc } from "@/protocol/client";
import type { ResourceInspectResult, ResourceRefInfo } from "@/protocol/types";

type Entry = { info: ResourceRefInfo | null; at: number };

const TTL_MS = 30_000;
const cache = new Map<string, Map<string, Entry>>();
const pending = new Map<string, Set<string>>();
const inflight = new Set<string>();
const listeners = new Set<() => void>();
let version = 0;

function notify() {
  version += 1;
  for (const listener of listeners) listener();
}

function flush(sessionId: string) {
  const tokens = [...(pending.get(sessionId) ?? [])];
  pending.delete(sessionId);
  if (tokens.length === 0) return;
  for (const token of tokens) inflight.add(`${sessionId}\n${token}`);
  const done = (infos: (ResourceRefInfo | null)[]) => {
    const entries = cache.get(sessionId) ?? new Map<string, Entry>();
    cache.set(sessionId, entries);
    const at = Date.now();
    tokens.forEach((token, index) => {
      inflight.delete(`${sessionId}\n${token}`);
      entries.set(token, { info: infos[index] ?? null, at });
    });
    notify();
  };
  rpc
    .call<ResourceInspectResult>("resource/inspect", { session_id: sessionId, refs: tokens })
    .then((result) => done(result.refs))
    .catch(() => done([]));
}

function request(sessionId: string, tokens: readonly string[]) {
  const entries = cache.get(sessionId);
  const now = Date.now();
  const missing = tokens.filter((token) => {
    const entry = entries?.get(token);
    return (!entry || now - entry.at > TTL_MS) && !inflight.has(`${sessionId}\n${token}`);
  });
  if (missing.length === 0) return;
  const queued = pending.get(sessionId);
  if (queued) {
    for (const token of missing) queued.add(token);
    return;
  }
  pending.set(sessionId, new Set(missing));
  queueMicrotask(() => flush(sessionId));
}

/** 查询资源引用：`undefined` 表示还没有结果，`null` 表示无效。草稿会话（`null`）一律无效。 */
export function useRefInfo(
  sessionId: string | null,
  tokens: readonly string[],
): (token: string) => ResourceRefInfo | null | undefined {
  useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    () => version,
  );
  const key = tokens.join("\n");
  // biome-ignore lint/correctness/useExhaustiveDependencies: key 是 tokens 的内容指纹
  useEffect(() => {
    if (sessionId && tokens.length > 0) request(sessionId, tokens);
  }, [sessionId, key]);
  return (token) => (sessionId ? cache.get(sessionId)?.get(token)?.info : null);
}
