// 连接状态、握手结果与 `config/get` 缓存。

import { create } from "zustand";
import { clientListeners, rpc } from "@/protocol/client";
import type { RpcError } from "@/protocol/errors";
import type { ConnectionStatus } from "@/protocol/rpc";
import type { ConfigGetResult, InitializeResult } from "@/protocol/types";

export type ConnectionState = {
  status: ConnectionStatus;
  server: InitializeResult | null;
  config: ConfigGetResult | null;
  handshakeError: RpcError | null;
  /** 曾经成功握手过：用于区分「首次连不上」与「断线重连」。 */
  everOnline: boolean;
  refreshConfig: () => Promise<void>;
};

export const useConnection = create<ConnectionState>()((set) => ({
  status: "offline",
  server: null,
  config: null,
  handshakeError: null,
  everOnline: false,
  refreshConfig: async () => {
    const config = await rpc.call<ConfigGetResult>("config/get", {});
    set({ config });
  },
}));

clientListeners.status.add((status) => useConnection.setState({ status }));
clientListeners.ready.add((server) => {
  useConnection.setState({ server, handshakeError: null, everOnline: true });
  void useConnection.getState().refreshConfig();
});
clientListeners.handshakeError.add((handshakeError) => useConnection.setState({ handshakeError }));
