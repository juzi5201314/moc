// JSON-RPC 错误码：与 crates/moc-protocol/src/error.rs 的 RpcErrorCode 一致。
// 未知码原样透传，前端按通用失败展示。

export const ErrorCode = {
  ConnectionClosed: -1,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  InternalError: -32603,
  ServerBusy: -32000,
  RequestCancelled: -32001,
  ServerNotInitialized: -32002,
  UnsupportedProtocolVersion: -32003,
  SessionNotFound: -32010,
  SessionBusy: -32011,
  NotSupported: -32014,
  SessionLocked: -32015,
} as const;

export class RpcError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = "RpcError";
    this.code = code;
    this.data = data;
  }
}
