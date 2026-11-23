// 按工作目录缓存 MCP 服务器运行态；监听 `mcp/status_updated` 实时同步，支持手动查询与重连。

import { create } from "zustand";
import { clientListeners, rpc } from "@/protocol/client";
import type { McpStatusSnapshot } from "@/protocol/types";
import { useConnection } from "./connection";

export type McpTarget = {
  sessionId?: string;
  cwd?: string;
};

type McpState = {
  byCwd: Record<string, McpStatusSnapshot>;
  /** 正在发起 `mcp/reconnect` 的键（`${cwd}::${server ?? "*"}`）。 */
  reconnecting: Record<string, boolean>;
  adopt: (snapshot: McpStatusSnapshot, aliasCwd?: string) => void;
  refresh: (target?: McpTarget) => Promise<McpStatusSnapshot>;
  reconnect: (target: McpTarget & { server?: string }) => Promise<McpStatusSnapshot>;
};

function reconnectKey(cwd: string, server?: string): string {
  return `${cwd}::${server ?? "*"}`;
}

export const useMcp = create<McpState>()((set, get) => ({
  byCwd: {},
  reconnecting: {},
  adopt: (snapshot, aliasCwd) =>
    set((state) => {
      const next = { ...state.byCwd, [snapshot.cwd]: snapshot };
      if (aliasCwd && aliasCwd !== snapshot.cwd) {
        next[aliasCwd] = snapshot;
      }
      return { byCwd: next };
    }),
  refresh: async (target) => {
    const params: { session_id?: string; cwd?: string } = {};
    if (target?.sessionId && target.sessionId !== "draft-session") {
      params.session_id = target.sessionId;
    } else if (target?.cwd) {
      params.cwd = target.cwd;
    }
    const snapshot = await rpc.call<McpStatusSnapshot>("mcp/list", params);
    get().adopt(snapshot, target?.cwd);
    return snapshot;
  },
  reconnect: async (target) => {
    const keyCwd = target.cwd ?? "";
    const key = reconnectKey(keyCwd, target.server);
    set((state) => ({ reconnecting: { ...state.reconnecting, [key]: true } }));
    try {
      const params: { session_id?: string; cwd?: string; server?: string } = {};
      if (target.sessionId && target.sessionId !== "draft-session") {
        params.session_id = target.sessionId;
      } else if (target.cwd) {
        params.cwd = target.cwd;
      }
      if (target.server) {
        params.server = target.server;
      }
      const snapshot = await rpc.call<McpStatusSnapshot>("mcp/reconnect", params);
      get().adopt(snapshot, target.cwd);
      return snapshot;
    } finally {
      set((state) => {
        const next = { ...state.reconnecting };
        delete next[key];
        return { reconnecting: next };
      });
    }
  },
}));

export function mcpReconnectPending(
  reconnecting: Record<string, boolean>,
  cwd: string,
  server?: string,
): boolean {
  return Boolean(reconnecting[reconnectKey(cwd, server)] || reconnecting[reconnectKey(cwd)]);
}

rpc.on("mcp/status_updated", (snapshot) => {
  useMcp.getState().adopt(snapshot);
});

clientListeners.ready.add(() => {
  const defaultCwd = useConnection.getState().config?.cwd;
  void useMcp
    .getState()
    .refresh(defaultCwd ? { cwd: defaultCwd } : undefined)
    .catch(() => undefined);
});
