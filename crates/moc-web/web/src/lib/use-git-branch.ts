// 工作目录的 git 分支：按 cwd 缓存，新鲜期内复用。
// 失效路径：invalidateGitBranch（会话工作区变更 / bash 任务结束）、窗口重新
// 聚焦、可见状态下每 STALE_MS 轮询一次——在外部终端切了分支也能收敛到正确值。

import { useEffect, useState } from "react";
import { rpc } from "@/protocol/client";
import type { WorkspaceGitBranchResult } from "@/protocol/types";

/** 缓存新鲜期：期内重复读取直接复用；同时兼作可见页面的轮询间隔。 */
const STALE_MS = 30_000;

type Entry = { at: number; promise: Promise<string | null> };
const cache = new Map<string, Entry>();
const subscribers = new Map<string, Set<() => void>>();

function fetchBranch(cwd: string): Promise<string | null> {
  const promise = rpc
    .call<WorkspaceGitBranchResult>("workspace/git_branch", { cwd })
    .then((result) => result.branch)
    .catch(() => null);
  cache.set(cwd, { at: Date.now(), promise });
  void promise.then(() => {
    for (const notify of subscribers.get(cwd) ?? []) notify();
  });
  return promise;
}

/** 分支可能已变化：无订阅者直接丢缓存等下次挂载重查，有订阅者立即重查并通知。 */
export function invalidateGitBranch(cwd: string | undefined): void {
  if (!cwd) return;
  if (subscribers.get(cwd)?.size) void fetchBranch(cwd);
  else cache.delete(cwd);
}

function current(cwd: string): Promise<string | null> {
  const entry = cache.get(cwd);
  return entry && Date.now() - entry.at < STALE_MS ? entry.promise : fetchBranch(cwd);
}

export function useGitBranch(cwd: string | undefined): string | null {
  const [branch, setBranch] = useState<string | null>(null);
  useEffect(() => {
    setBranch(null);
    if (!cwd) return;
    let alive = true;
    const reload = () => {
      void current(cwd).then((value) => {
        if (alive) setBranch(value);
      });
    };
    reload();
    let subs = subscribers.get(cwd);
    if (!subs) {
      subs = new Set();
      subscribers.set(cwd, subs);
    }
    subs.add(reload);
    const poll = () => {
      if (document.visibilityState === "visible") void fetchBranch(cwd);
    };
    window.addEventListener("focus", poll);
    document.addEventListener("visibilitychange", poll);
    const timer = setInterval(poll, STALE_MS);
    return () => {
      alive = false;
      subs.delete(reload);
      if (subs.size === 0) subscribers.delete(cwd);
      window.removeEventListener("focus", poll);
      document.removeEventListener("visibilitychange", poll);
      clearInterval(timer);
    };
  }, [cwd]);
  return branch;
}
