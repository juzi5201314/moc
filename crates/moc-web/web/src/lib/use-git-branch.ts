// 工作目录的 git 分支：按 cwd 缓存，并按仓库根关联同仓库跨子目录刷新。
// 失效路径：invalidateGitBranch（工具/bash/子代理/提示结束、手动切换）、窗口聚焦、
// 可见状态下每 POLL_MS 轮询一次（core 侧直接读 .git/HEAD，零子进程开销），
// 确保 Agent 在后台切分支或外部终端切分支都能即时反映到 UI。

import { useEffect, useState } from "react";
import { rpc } from "@/protocol/client";
import type { WorkspaceGitBranchResult } from "@/protocol/types";

/** 可见页面轮询间隔与缓存新鲜期：core 侧零子进程读 .git/HEAD，3s 轮询开销可忽略。 */
const POLL_MS = 3_000;

export type GitBranchInfo = {
  /** 当前分支名；detached HEAD 时为 null。 */
  branch: string | null;
  /** 仓库根目录；用于同仓库跨目录联动失效。 */
  root: string | null;
  /** HEAD 短提交哈希；unborn 时为 null。 */
  headOid: string | null;
  detached: boolean;
  unborn: boolean;
};

/** 展示用分支标签：普通/unborn 显示分支名，detached 显示短提交哈希。 */
export function formatGitBranchLabel(info: GitBranchInfo): string {
  if (info.branch) return info.branch;
  if (info.detached) return info.headOid ? `detached · ${info.headOid}` : "HEAD";
  return "HEAD";
}

function normalizePath(path: string): string {
  return path.replace(/[\\/]+$/, "");
}

function isSameOrWithin(path: string, root: string): boolean {
  const a = normalizePath(path);
  const b = normalizePath(root);
  return a === b || a.startsWith(`${b}/`) || a.startsWith(`${b}\\`);
}

function toBranchInfo(result: WorkspaceGitBranchResult): GitBranchInfo | null {
  const branch = result.branch ?? null;
  const root = result.root ?? null;
  const detached = Boolean(result.detached);
  if (!branch && !detached && !root) return null;
  return {
    branch,
    root,
    headOid: result.head_oid ?? null,
    detached,
    unborn: Boolean(result.unborn),
  };
}

function sameBranchInfo(a: GitBranchInfo | null, b: GitBranchInfo | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.branch === b.branch &&
    a.root === b.root &&
    a.headOid === b.headOid &&
    a.detached === b.detached &&
    a.unborn === b.unborn
  );
}

type Entry = {
  at: number;
  hasValue: boolean;
  value: GitBranchInfo | null;
  inflight?: Promise<GitBranchInfo | null>;
  /** 在飞期间又收到失效信号时置位，当前请求结束后立刻补查一次，避免丢失连续切换。 */
  pending?: boolean;
};

const cache = new Map<string, Entry>();
const subscribers = new Map<string, Set<() => void>>();

function notifyCwd(cwd: string): void {
  for (const notify of subscribers.get(cwd) ?? []) notify();
}

function fetchBranch(cwd: string): Promise<GitBranchInfo | null> {
  const existing = cache.get(cwd);
  if (existing?.inflight) {
    existing.pending = true;
    return existing.inflight;
  }
  const entry: Entry = existing ?? { at: 0, hasValue: false, value: null };
  cache.set(cwd, entry);

  const promise = rpc
    .call<WorkspaceGitBranchResult>("workspace/git_branch", { cwd })
    .then(toBranchInfo)
    .catch(() => null)
    .then((next) => {
      const changed = !entry.hasValue || !sameBranchInfo(entry.value, next);
      entry.at = Date.now();
      entry.hasValue = true;
      if (changed) entry.value = next;
      entry.inflight = undefined;
      if (changed) notifyCwd(cwd);
      if (entry.pending) {
        entry.pending = false;
        if (subscribers.get(cwd)?.size) void fetchBranch(cwd);
      }
      return entry.value;
    });

  entry.inflight = promise;
  return promise;
}

/** 找出与给定 cwd / repoRoot 属于同一仓库的已缓存或订阅目录集合。 */
function matchingCwds(cwd: string, repoRoot?: string | null): Set<string> {
  const root = repoRoot ?? cache.get(cwd)?.value?.root ?? null;
  const targets = new Set<string>([cwd]);
  const allKeys = new Set<string>([...cache.keys(), ...subscribers.keys()]);
  for (const key of allKeys) {
    if (key === cwd) continue;
    const knownRoot = cache.get(key)?.value?.root ?? null;
    if (root && knownRoot && normalizePath(root) === normalizePath(knownRoot)) {
      targets.add(key);
    } else if (root && isSameOrWithin(key, root)) {
      targets.add(key);
    } else if (knownRoot && isSameOrWithin(cwd, knownRoot)) {
      targets.add(key);
    }
  }
  return targets;
}

/** 分支可能已变化：同仓库有订阅者立即重查并通知，无订阅者清缓存等下次挂载重查。 */
export function invalidateGitBranch(cwd: string | undefined, repoRoot?: string | null): void {
  if (!cwd) return;
  for (const target of matchingCwds(cwd, repoRoot)) {
    if (subscribers.get(target)?.size) void fetchBranch(target);
    else cache.delete(target);
  }
}

/** 手动切换分支成功后立即同步更新同仓库缓存（零延迟切 UI），再后台校准 OID。 */
export function applySwitchedBranch(
  cwd: string,
  repoRoot: string | null | undefined,
  branch: string,
): void {
  const now = Date.now();
  for (const target of matchingCwds(cwd, repoRoot)) {
    const entry = cache.get(target);
    const next: GitBranchInfo = {
      branch,
      root: repoRoot ?? entry?.value?.root ?? null,
      headOid: entry?.value?.headOid ?? null,
      detached: false,
      unborn: entry?.value?.unborn ?? false,
    };
    if (entry) {
      entry.at = now;
      entry.hasValue = true;
      entry.value = next;
    } else {
      cache.set(target, { at: now, hasValue: true, value: next });
    }
    notifyCwd(target);
  }
  invalidateGitBranch(cwd, repoRoot);
}

function readOrFetch(cwd: string): GitBranchInfo | null {
  const entry = cache.get(cwd);
  if (!entry || Date.now() - entry.at >= POLL_MS) {
    void fetchBranch(cwd);
  }
  return entry?.hasValue ? entry.value : null;
}

export function useGitBranch(cwd: string | undefined): GitBranchInfo | null {
  const [info, setInfo] = useState<GitBranchInfo | null>(() =>
    cwd ? (cache.get(cwd)?.hasValue ? (cache.get(cwd)?.value ?? null) : null) : null,
  );

  useEffect(() => {
    if (!cwd) {
      setInfo(null);
      return;
    }
    const sync = () => {
      const entry = cache.get(cwd);
      const next = entry?.hasValue ? entry.value : null;
      setInfo((prev) => (sameBranchInfo(prev, next) ? prev : next));
    };
    setInfo(readOrFetch(cwd));
    let subs = subscribers.get(cwd);
    if (!subs) {
      subs = new Set();
      subscribers.set(cwd, subs);
    }
    subs.add(sync);
    const poll = () => {
      if (document.visibilityState === "visible") void fetchBranch(cwd);
    };
    window.addEventListener("focus", poll);
    document.addEventListener("visibilitychange", poll);
    const timer = setInterval(poll, POLL_MS);
    return () => {
      subs.delete(sync);
      if (subs.size === 0) subscribers.delete(cwd);
      window.removeEventListener("focus", poll);
      document.removeEventListener("visibilitychange", poll);
      clearInterval(timer);
    };
  }, [cwd]);

  return info;
}

/** 测试辅助：重置分支缓存。 */
export function resetGitBranchCache(): void {
  cache.clear();
}
