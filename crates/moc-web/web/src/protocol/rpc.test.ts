import { describe, expect, it, vi } from "vitest";
import { ErrorCode, RpcError } from "./errors";
import { loadHistoryPage } from "./history";
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
  it("历史块按 UTF-8 字节接续，完整还原并在成功或失败后释放", async () => {
    const call = vi.fn<RpcClient["call"]>();
    const client = { call: call as RpcClient["call"] };
    const page = { session_id: "s", max_entries: 1 };
    const expected = {
      entries: [{ text: '中文🙂\\"\n完整结果' }],
      plans: [{ markdown: "# 计划" }],
    };
    call.mockResolvedValueOnce({ type: "page", page: expected });
    expect(await loadHistoryPage(client, page)).toEqual(expected);
    expect(call).toHaveBeenCalledTimes(1);
    const json = JSON.stringify(expected);
    const split = json.indexOf("🙂") + 2;
    const head = json.slice(0, split);
    const bytes = new TextEncoder().encode(head).byteLength;
    const firstChunk = {
      type: "chunk",
      transfer_id: "t",
      offset: 0,
      data: head,
      next_offset: bytes,
    };
    call.mockClear();
    call
      .mockResolvedValueOnce(firstChunk)
      .mockResolvedValueOnce({
        type: "chunk",
        transfer_id: "t",
        offset: bytes,
        data: json.slice(split),
        next_offset: null,
      })
      .mockResolvedValueOnce({ type: "closed" });
    expect(await loadHistoryPage(client, page)).toEqual(expected);
    expect(call.mock.calls[1]).toEqual([
      "session/history",
      { action: "read", transfer_id: "t", offset: bytes },
    ]);
    expect(call.mock.calls[2]).toEqual(["session/history", { action: "close", transfer_id: "t" }]);
    for (const failure of ["错位", "断线"]) {
      call.mockClear();
      call.mockResolvedValueOnce(firstChunk);
      if (failure === "错位") call.mockResolvedValueOnce(firstChunk);
      else call.mockRejectedValueOnce(new Error("断线"));
      call.mockResolvedValueOnce({ type: "closed" });
      await expect(loadHistoryPage(client, page)).rejects.toThrow(
        failure === "错位" ? "不连续" : "断线",
      );
      expect(call.mock.calls.at(-1)).toEqual([
        "session/history",
        { action: "close", transfer_id: "t" },
      ]);
    }
    const pending = Promise.withResolvers<unknown>();
    const controller = new AbortController();
    call.mockClear();
    call.mockReturnValueOnce(pending.promise).mockResolvedValueOnce({ type: "closed" });
    const cancelled = loadHistoryPage(client, page, controller.signal);
    controller.abort();
    pending.resolve(firstChunk);
    await expect(cancelled).rejects.toMatchObject({ name: "AbortError" });
    expect(call.mock.calls).toEqual([
      ["session/history", { action: "open", page }],
      ["session/history", { action: "close", transfer_id: "t" }],
    ]);
  });
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
    socket.receive({
      jsonrpc: "2.0",
      method: "session/update",
      params: { session_id: "s", update: { type: "prompt_rewound", replayed_tools: false } },
    });
    expect(seen).toEqual(["title_changed", "prompt_rewound"]);
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
