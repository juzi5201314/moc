// 工作目录的 git 分支：按 cwd 缓存一次查询，切会话不重复请求。

import { useEffect, useState } from "react";
import { rpc } from "@/protocol/client";
import type { WorkspaceGitBranchResult } from "@/protocol/types";

const cache = new Map<string, Promise<string | null>>();

export function useGitBranch(cwd: string | undefined): string | null {
  const [branch, setBranch] = useState<string | null>(null);
  useEffect(() => {
    setBranch(null);
    if (!cwd) return;
    let alive = true;
    let pending = cache.get(cwd);
    if (!pending) {
      pending = rpc
        .call<WorkspaceGitBranchResult>("workspace/git_branch", { cwd })
        .then((result) => result.branch)
        .catch(() => null);
      cache.set(cwd, pending);
    }
    void pending.then((value) => {
      if (alive) setBranch(value);
    });
    return () => {
      alive = false;
    };
  }, [cwd]);
  return branch;
}
