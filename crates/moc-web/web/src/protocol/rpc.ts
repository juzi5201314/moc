// JSON-RPC 2.0 over WebSocket：一个标签页一条连接，core 侧一条独立 NDJSON 连接。
//
// - 前端发起的调用 id 从 1 递增；core 发起的反向请求 id 为负数，按原 id 回应。
// - 通知里不认识的 `type` 整条丢弃（协议兼容规则），不报错、不断连接。
// - 断线后指数退避重连；每次连上都重新 initialize。

import { ErrorCode, RpcError } from "./errors";
import {
  type AskParams,
  type AskResult,
  BTW_UPDATE_TYPES,
  type BtwUpdateParams,
  type InitializeResult,
  type PermissionRequestParams,
  type PermissionResult,
  PROTOCOL_VERSION,
  SESSION_UPDATE_TYPES,
  type SessionUpdateParams,
  type WebStateDocument,
} from "./types";

export type ConnectionStatus = "connecting" | "online" | "offline";

/** WebSocket 的最小子集，便于测试注入假实现。 */
export type SocketLike = {
  readonly readyState: number;
  send(data: string): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
};

const SOCKET_OPEN = 1;
const RECONNECT_INITIAL_MS = 600;
const RECONNECT_FACTOR = 1.7;
const RECONNECT_MAX_MS = 8000;

type NotificationMap = {
  "session/update": SessionUpdateParams;
  "session/btw_update": BtwUpdateParams;
  "web/state_updated": WebStateDocument;
};

type RequestMap = {
  "session/request_permission": [PermissionRequestParams, PermissionResult];
  "session/request_ask": [AskParams, AskResult];
};

type Pending = { resolve: (value: unknown) => void; reject: (error: RpcError) => void };

export type RpcClientOptions = {
  url: string;
  clientVersion: string;
  createSocket?: (url: string) => SocketLike;
  /** 握手完成（含重连后）调用。 */
  onReady?: (result: InitializeResult) => void;
  onStatus?: (status: ConnectionStatus) => void;
  /** 握手失败（版本不兼容等），不会自动重试同一连接。 */
  onHandshakeError?: (error: RpcError) => void;
  /** 关闭 setTimeout 驱动的重连（测试用）。 */
  reconnect?: boolean;
};

export class RpcClient {
  private socket: SocketLike | null = null;
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  private readonly notificationHandlers = new Map<string, Set<(params: never) => void>>();
  private readonly requestHandlers = new Map<string, (params: never) => Promise<unknown>>();
  private reconnectDelay = RECONNECT_INITIAL_MS;
  private reconnectTimer: number | undefined;
  private stopped = false;
  private ready = false;
  status: ConnectionStatus = "offline";

  private readonly options: RpcClientOptions;

  constructor(options: RpcClientOptions) {
    this.options = options;
  }

  connect(): void {
    this.stopped = false;
    this.open();
  }

  /** 停止并不再重连。 */
  dispose(): void {
    this.stopped = true;
    window.clearTimeout(this.reconnectTimer);
    this.socket?.close();
  }

  get isReady(): boolean {
    return this.ready;
  }

  call<T>(method: string, params: unknown = {}): Promise<T> {
    const socket = this.socket;
    if (!socket || socket.readyState !== SOCKET_OPEN) {
      return Promise.reject(new RpcError(ErrorCode.ConnectionClosed, "连接已关闭"));
    }
    const id = this.nextId++;
    const { promise, resolve, reject } = Promise.withResolvers<T>();
    this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
    try {
      socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    } catch (error) {
      this.pending.delete(id);
      reject(new RpcError(ErrorCode.ConnectionClosed, String(error)));
    }
    return promise;
  }

  on<M extends keyof NotificationMap>(
    method: M,
    handler: (params: NotificationMap[M]) => void,
  ): () => void {
    let handlers = this.notificationHandlers.get(method);
    if (!handlers) {
      handlers = new Set();
      this.notificationHandlers.set(method, handlers);
    }
    const entry = handler as (params: never) => void;
    handlers.add(entry);
    return () => handlers.delete(entry);
  }

  onRequest<M extends keyof RequestMap>(
    method: M,
    handler: (params: RequestMap[M][0]) => Promise<RequestMap[M][1]>,
  ): void {
    this.requestHandlers.set(method, handler as (params: never) => Promise<unknown>);
  }

  // ---------- 内部 ----------

  private setStatus(status: ConnectionStatus): void {
    this.status = status;
    this.options.onStatus?.(status);
  }

  private open(): void {
    this.setStatus("connecting");
    const create = this.options.createSocket ?? ((url: string) => new WebSocket(url) as SocketLike);
    const socket = create(this.options.url);
    this.socket = socket;

    socket.onopen = () => {
      void this.handshake();
    };
    socket.onmessage = (event) => {
      if (typeof event.data !== "string") return;
      let message: unknown;
      try {
        message = JSON.parse(event.data);
      } catch {
        return;
      }
      if (message && typeof message === "object") this.handleMessage(message as RawMessage);
    };
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.ready = false;
      this.failPending();
      this.setStatus("offline");
      this.scheduleReconnect();
    };
    socket.onerror = () => {
      socket.close();
    };
  }

  private async handshake(): Promise<void> {
    try {
      const result = await this.call<InitializeResult>("initialize", {
        protocol_version: PROTOCOL_VERSION,
        client: { name: "moc-web", version: this.options.clientVersion, frontend: "webui" },
        capabilities: { permission_requests: true, ask_requests: true },
      });
      this.reconnectDelay = RECONNECT_INITIAL_MS;
      this.ready = true;
      this.setStatus("online");
      this.options.onReady?.(result);
    } catch (error) {
      const rpcError =
        error instanceof RpcError ? error : new RpcError(ErrorCode.InternalError, String(error));
      if (rpcError.code !== ErrorCode.ConnectionClosed) this.options.onHandshakeError?.(rpcError);
    }
  }

  private failPending(): void {
    for (const pending of this.pending.values()) {
      pending.reject(new RpcError(ErrorCode.ConnectionClosed, "连接已关闭"));
    }
    this.pending.clear();
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.options.reconnect === false) return;
    const delay = this.reconnectDelay;
    this.reconnectDelay = Math.min(this.reconnectDelay * RECONNECT_FACTOR, RECONNECT_MAX_MS);
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = undefined;
      if (!this.stopped) this.open();
    }, delay);
  }

  private send(message: object): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== SOCKET_OPEN) return;
    socket.send(JSON.stringify(message));
  }

  private handleMessage(message: RawMessage): void {
    const hasId = message.id !== undefined && message.id !== null;
    if (hasId && typeof message.method === "string") {
      void this.handleServerRequest(message.id as number | string, message.method, message.params);
      return;
    }
    if (hasId) {
      const pending = this.pending.get(message.id as number);
      if (!pending) return;
      this.pending.delete(message.id as number);
      if (message.error) {
        pending.reject(
          new RpcError(message.error.code, message.error.message ?? "请求失败", message.error.data),
        );
      } else {
        pending.resolve(message.result);
      }
      return;
    }
    if (typeof message.method === "string")
      this.dispatchNotification(message.method, message.params);
  }

  private dispatchNotification(method: string, params: unknown): void {
    const handlers = this.notificationHandlers.get(method);
    if (!handlers || !params || typeof params !== "object") return;
    const update = "update" in params ? params.update : undefined;
    const type =
      update && typeof update === "object" && "type" in update && typeof update.type === "string"
        ? update.type
        : null;
    if (method === "session/update" && (!type || !Object.hasOwn(SESSION_UPDATE_TYPES, type)))
      return;
    if (method === "session/btw_update" && (!type || !Object.hasOwn(BTW_UPDATE_TYPES, type)))
      return;
    for (const handler of handlers) (handler as (params: unknown) => void)(params);
  }

  private async handleServerRequest(id: number | string, method: string, params: unknown) {
    const handler = this.requestHandlers.get(method);
    if (!handler) {
      this.send({
        jsonrpc: "2.0",
        id,
        error: { code: ErrorCode.MethodNotFound, message: `未实现的方法：${method}` },
      });
      return;
    }
    try {
      const result = await (handler as (params: unknown) => Promise<unknown>)(params);
      this.send({ jsonrpc: "2.0", id, result });
    } catch (error) {
      this.send({
        jsonrpc: "2.0",
        id,
        error: { code: ErrorCode.InternalError, message: String(error) },
      });
    }
  }
}

type RawMessage = {
  id?: number | string | null;
  method?: unknown;
  params?: unknown;
  result?: unknown;
  error?: { code: number; message?: string; data?: unknown };
};
