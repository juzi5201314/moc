// 全应用唯一的 RPC 连接与访问令牌。
//
// 令牌只在首次打开时从地址栏读取，随即存进 sessionStorage 并从 URL 去掉，
// 避免出现在历史记录、截图与 Referer 里。

import type { RpcError } from "./errors";
import { type ConnectionStatus, RpcClient } from "./rpc";
import type { InitializeResult } from "./types";

const TOKEN_KEY = "moc-token";

function takeToken(): string | null {
  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get("token");
  if (fromUrl) {
    sessionStorage.setItem(TOKEN_KEY, fromUrl);
    url.searchParams.delete("token");
    window.history.replaceState(null, "", `${url.pathname}${url.search}${url.hash}`);
    return fromUrl;
  }
  return sessionStorage.getItem(TOKEN_KEY);
}

export const accessToken = takeToken();

const wsProtocol = window.location.protocol === "https:" ? "wss" : "ws";

/** 连接生命周期监听；store 在模块加载时注册，晚于 client 构造。 */
export type ClientListeners = {
  ready: Set<(result: InitializeResult) => void>;
  status: Set<(status: ConnectionStatus) => void>;
  handshakeError: Set<(error: RpcError) => void>;
};

export const clientListeners: ClientListeners = {
  ready: new Set(),
  status: new Set(),
  handshakeError: new Set(),
};

export const rpc = new RpcClient({
  url: `${wsProtocol}://${window.location.host}/ws?token=${encodeURIComponent(accessToken ?? "")}`,
  clientVersion: "0.1.0",
  onReady: (result) => {
    for (const listener of clientListeners.ready) listener(result);
  },
  onStatus: (status) => {
    for (const listener of clientListeners.status) listener(status);
  },
  onHandshakeError: (error) => {
    for (const listener of clientListeners.handshakeError) listener(error);
  },
});
