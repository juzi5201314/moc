// 旁路问答（BTW）：每会话一条内存线程，最多 20 轮；不进主转写、不落盘。

import { produce } from "immer";
import { create } from "zustand";
import { rpc } from "@/protocol/client";
import { ErrorCode, RpcError } from "@/protocol/errors";
import type { BtwExchange, BtwResult, BtwUpdateParams, FinishReason } from "@/protocol/types";

export const MAX_BTW_TURNS = 20;

export type BtwStatus = "waiting" | "generating" | "done" | "cancelled" | "failed";

export type BtwTurn = {
  requestId: number;
  question: string;
  answer: string;
  reasoning: string;
  status: BtwStatus;
  model?: string;
  error?: string;
};

export type BtwThread = { turns: BtwTurn[]; inflight: number | null };

export type BtwState = {
  threads: Record<string, BtwThread>;
  /** 提交一条旁路问题；同会话已有在飞请求时返回 false（调用方保留草稿）。 */
  ask: (sessionId: string, question: string) => boolean;
  cancel: (sessionId: string) => void;
  clear: (sessionId: string) => void;
};

let nextRequestId = 1;

const EMPTY_THREAD: BtwThread = { turns: [], inflight: null };

function isCancelled(reason: FinishReason): boolean {
  return reason === "Cancelled" || reason === "Interrupted";
}

export const useBtw = create<BtwState>()((set, get) => {
  const mutate = (sessionId: string, recipe: (thread: BtwThread) => void) =>
    set(
      produce((state: BtwState) => {
        state.threads[sessionId] ??= { turns: [], inflight: null };
        recipe(state.threads[sessionId]);
      }),
    );

  return {
    threads: {},
    ask: (sessionId, question) => {
      const thread = get().threads[sessionId] ?? EMPTY_THREAD;
      if (thread.inflight !== null) return false;
      const requestId = nextRequestId++;
      // 只有成功完整的问答回放给下一轮。
      const history: BtwExchange[] = thread.turns
        .filter((turn) => turn.status === "done")
        .slice(-MAX_BTW_TURNS)
        .map((turn) => ({ question: turn.question, answer: turn.answer }));
      mutate(sessionId, (draft) => {
        draft.turns.push({ requestId, question, answer: "", reasoning: "", status: "waiting" });
        if (draft.turns.length > MAX_BTW_TURNS)
          draft.turns.splice(0, draft.turns.length - MAX_BTW_TURNS);
        draft.inflight = requestId;
      });
      rpc
        .call<BtwResult>("session/btw", {
          session_id: sessionId,
          request_id: requestId,
          question,
          history,
        })
        .then((result) => {
          mutate(sessionId, (draft) => {
            const turn = draft.turns.find((item) => item.requestId === requestId);
            if (turn) {
              turn.answer = result.text;
              turn.model = result.model;
              turn.status = isCancelled(result.finish_reason)
                ? "cancelled"
                : result.finish_reason === "Stop"
                  ? "done"
                  : "failed";
              if (turn.status === "failed") turn.error = "输出未完整结束";
            }
            if (draft.inflight === requestId) draft.inflight = null;
          });
        })
        .catch((error: unknown) => {
          const message = error instanceof RpcError ? error.message : String(error);
          const busy = error instanceof RpcError && error.code === ErrorCode.SessionBusy;
          mutate(sessionId, (draft) => {
            const turn = draft.turns.find((item) => item.requestId === requestId);
            if (turn) {
              turn.status = "failed";
              turn.error = busy ? "已有一条旁路请求在进行" : message;
            }
            if (draft.inflight === requestId) draft.inflight = null;
          });
        });
      return true;
    },
    cancel: (sessionId) => {
      const inflight = get().threads[sessionId]?.inflight;
      if (inflight == null) return;
      void rpc
        .call("session/btw_cancel", { session_id: sessionId, request_id: inflight })
        .catch(() => {});
    },
    clear: (sessionId) => {
      get().cancel(sessionId);
      // 清空后迟到的结果找不到对应 turn，自然丢弃。
      mutate(sessionId, (draft) => {
        draft.turns = [];
        draft.inflight = null;
      });
    },
  };
});

rpc.on("session/btw_update", (params: BtwUpdateParams) => {
  const thread = useBtw.getState().threads[params.session_id];
  if (!thread?.turns.some((turn) => turn.requestId === params.request_id)) return;
  useBtw.setState(
    produce((state: BtwState) => {
      const turn = state.threads[params.session_id]?.turns.find(
        (item) => item.requestId === params.request_id,
      );
      if (!turn) return;
      const update = params.update;
      switch (update.type) {
        case "started":
        case "waiting":
          turn.status = "waiting";
          break;
        case "generating":
          turn.status = "generating";
          break;
        case "text_delta":
          turn.status = "generating";
          turn.answer += update.text;
          break;
        case "reasoning_delta":
          turn.reasoning += update.text;
          break;
        default:
          break;
      }
    }),
  );
});
