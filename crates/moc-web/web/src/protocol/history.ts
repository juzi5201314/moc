// 大历史页按字节块重组；与普通 RPC 一样只在完整结果就绪后交给 store。
import type { RpcClient } from "./rpc";
import type { SessionLoadPageResult } from "./types";

export type HistoryPageParams = {
  session_id: string;
  before_entry_id?: string;
  max_entries: number;
  max_message_bytes?: number;
  full_messages?: boolean;
  redo_page?: { before_entry_id?: string; max_entries: number };
  plan_page?: { before?: number; max_entries: number };
};
type Result =
  | { type: "page"; page: SessionLoadPageResult }
  | { type: "chunk"; transfer_id: string; offset: number; data: string; next_offset: number | null }
  | { type: "closed" };

export async function loadHistoryPage(
  client: Pick<RpcClient, "call">,
  page: HistoryPageParams,
  signal?: AbortSignal,
): Promise<SessionLoadPageResult> {
  signal?.throwIfAborted();
  let result = await client.call<Result>("session/history", { action: "open", page });
  if (result.type === "page") {
    signal?.throwIfAborted();
    return result.page;
  }
  if (result.type !== "chunk") throw new Error("历史传输未返回页面");
  const id = result.transfer_id;
  const parts: string[] = [];
  const encoder = new TextEncoder();
  let offset = 0;
  try {
    while (result.type === "chunk") {
      signal?.throwIfAborted();
      const next = offset + encoder.encode(result.data).byteLength;
      if (
        result.transfer_id !== id ||
        result.offset !== offset ||
        (result.next_offset !== null && (result.next_offset !== next || next <= offset))
      ) {
        throw new Error("历史块不连续");
      }
      parts.push(result.data);
      if (result.next_offset === null) {
        const json = parts.join("");
        parts.length = 0;
        return JSON.parse(json) as SessionLoadPageResult;
      }
      offset = result.next_offset;
      result = await client.call<Result>("session/history", {
        action: "read",
        transfer_id: id,
        offset,
      });
    }
    throw new Error("历史传输中途改变结果类型");
  } finally {
    parts.length = 0;
    await client
      .call("session/history", { action: "close", transfer_id: id })
      .catch(() => undefined);
  }
}
