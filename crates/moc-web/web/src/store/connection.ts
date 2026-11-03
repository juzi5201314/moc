// 连接状态、握手结果与 `config/get` 缓存。

import { create } from "zustand";
import { clientListeners, rpc } from "@/protocol/client";
import type { RpcError } from "@/protocol/errors";
import type { ConnectionStatus } from "@/protocol/rpc";
import type { CommandMeta, ConfigGetResult, InitializeResult } from "@/protocol/types";

export type ConnectionState = {
  status: ConnectionStatus;
  server: InitializeResult | null;
  config: ConfigGetResult | null;
  /** 内置斜杠命令元数据，来自后端命令注册表（`/commands.json`）。 */
  commands: CommandMeta[];
  handshakeError: RpcError | null;
  /** 曾经成功握手过：用于区分「首次连不上」与「断线重连」。 */
  everOnline: boolean;
  refreshConfig: () => Promise<void>;
  refreshCommands: () => Promise<void>;
};

export const useConnection = create<ConnectionState>()((set) => ({
  status: "offline",
  server: null,
  config: null,
  commands: [],
  handshakeError: null,
  everOnline: false,
  refreshConfig: async () => {
    const config = await rpc.call<ConfigGetResult>("config/get", {});
    set({ config });
  },
  refreshCommands: async () => {
    const response = await fetch("/commands.json");
    if (response.ok) set({ commands: await response.json() });
  },
}));

const NO_MODELS: ConfigGetResult["models"] = [];
const NO_COMMANDS: CommandMeta[] = [];

/** 模型目录选择器：config 未加载时返回同一个空数组，避免 `?? []` 每次产生新引用导致无限重渲染。 */
export const selectModels = (state: ConnectionState) => state.config?.models ?? NO_MODELS;
/** 内置命令元数据选择器；同 `selectModels` 的稳定空数组理由。 */
export const selectCommands = (state: ConnectionState) => state.commands ?? NO_COMMANDS;
/** 特性开关快照选择器：config 未加载时为 undefined，消费方按「全部开启」处理。 */
export const selectFeatures = (state: ConnectionState) => state.config?.features;

clientListeners.status.add((status) => useConnection.setState({ status }));
clientListeners.ready.add((server) => {
  useConnection.setState({ server, handshakeError: null, everOnline: true });
  void useConnection.getState().refreshConfig();
  void useConnection.getState().refreshCommands();
});
clientListeners.handshakeError.add((handshakeError) => useConnection.setState({ handshakeError }));
