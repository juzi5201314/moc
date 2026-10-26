import { describe, expect, it, vi } from "vitest";
import { ErrorCode, RpcError } from "./errors";
import { RpcClient, type SocketLike } from "./rpc";

class FakeSocket implements SocketLike {
  readyState = 0;
  sent: Record<string, unknown>[] = [];
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;

  send(data: string): void {
    this.sent.push(JSON.parse(data));
  }
  close(): void {
    this.readyState = 3;
    this.onclose?.({});
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.({});
  }
  receive(message: object): void {
    this.onmessage?.({ data: JSON.stringify(message) });
  }
  last(): Record<string, unknown> {
    const message = this.sent.at(-1);
    if (!message) throw new Error("nothing sent");
    return message;
  }
}

/** 建立一个已握手的客户端。 */
async function connected() {
  const socket = new FakeSocket();
  const client = new RpcClient({
    url: "ws://test/ws",
    clientVersion: "test",
    createSocket: () => socket,
    reconnect: false,
  });
  client.connect();
  socket.open();
  const init = socket.last();
  expect(init.method).toBe("initialize");
  socket.receive({
    jsonrpc: "2.0",
    id: init.id,
    result: { protocol_version: "1.4", server: { name: "moc", version: "0" }, capabilities: {} },
  });
  await Promise.resolve();
  await Promise.resolve();
  return { socket, client };
}

describe("RpcClient", () => {
  it("assigns increasing ids and pairs responses", async () => {
    const { socket, client } = await connected();
    const first = client.call<string>("a/one");
    const second = client.call<string>("a/two");
    const [one, two] = socket.sent.slice(-2);
    expect(Number(two?.id)).toBe(Number(one?.id) + 1);
    socket.receive({ jsonrpc: "2.0", id: two?.id, result: "B" });
    socket.receive({ jsonrpc: "2.0", id: one?.id, result: "A" });
    await expect(first).resolves.toBe("A");
    await expect(second).resolves.toBe("B");
  });

  it("turns error responses into RpcError with code", async () => {
    const { socket, client } = await connected();
    const call = client.call("session/load");
    socket.receive({
      jsonrpc: "2.0",
      id: socket.last().id,
      error: { code: ErrorCode.SessionLocked, message: "locked" },
    });
    const error = await call.catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(RpcError);
    expect((error as RpcError).code).toBe(ErrorCode.SessionLocked);
  });

  it("answers reverse requests with the original id", async () => {
    const { socket, client } = await connected();
    client.onRequest("session/request_permission", async () => ({ option_id: "allow" }));
    socket.receive({
      jsonrpc: "2.0",
      id: -7,
      method: "session/request_permission",
      params: {},
    });
    await vi.waitFor(() =>
      expect(socket.last()).toEqual({ jsonrpc: "2.0", id: -7, result: { option_id: "allow" } }),
    );
  });

  it("replies -32601 for unregistered reverse requests", async () => {
    const { socket } = await connected();
    socket.receive({ jsonrpc: "2.0", id: -8, method: "session/unknown", params: {} });
    await vi.waitFor(() => expect(socket.last().id).toBe(-8));
    expect((socket.last().error as { code: number }).code).toBe(-32601);
  });

  it("drops session/update with unknown type", async () => {
    const { socket, client } = await connected();
    const seen: string[] = [];
    client.on("session/update", (params) => seen.push(params.update.type));
    socket.receive({
      jsonrpc: "2.0",
      method: "session/update",
      params: { session_id: "s", update: { type: "future_thing" } },
    });
    socket.receive({
      jsonrpc: "2.0",
      method: "session/update",
      params: { session_id: "s", update: { type: "title_changed", title: "x" } },
    });
    expect(seen).toEqual(["title_changed"]);
  });

  it("rejects all pending calls on disconnect", async () => {
    const { socket, client } = await connected();
    const a = client.call("x/a").catch((error: RpcError) => error.code);
    const b = client.call("x/b").catch((error: RpcError) => error.code);
    socket.close();
    await expect(a).resolves.toBe(ErrorCode.ConnectionClosed);
    await expect(b).resolves.toBe(ErrorCode.ConnectionClosed);
    expect(client.status).toBe("offline");
  });
});
