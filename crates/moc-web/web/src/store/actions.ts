// 跨组件共享的会话动作：发送提示（composer 与「重试」共用）。

import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type { InputPart, PromptResult, SessionMode } from "@/protocol/types";
import { useComposer } from "./composer";
import { syncEntryIds } from "./lifecycle";
import { useSessions } from "./sessions";
import { useTranscript } from "./transcript";

export type SendOptions = { mode?: SessionMode; echo?: boolean };

/**
 * 发送一条提示。本地先插入用户块（`echo` 默认 true），流式内容走 session/update；
 * 返回时提示已结束，再补 entry id 并刷新侧栏。
 */
export async function sendPrompt(
  sessionId: string,
  input: InputPart[],
  options: SendOptions = {},
): Promise<void> {
  if (input.length === 0) return;
  const store = useTranscript.getState();
  if (options.echo ?? true) {
    const text = input.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("\n");
    const images = input.flatMap((part) => (part.type === "image" ? [part.source] : []));
    store.localUser(sessionId, text, images);
  }
  useComposer.getState().setLastInput(sessionId, input);
  try {
    await rpc.call<PromptResult>("session/prompt", {
      session_id: sessionId,
      input,
      ...(options.mode ? { mode: options.mode } : {}),
    });
  } catch (error) {
    const message = error instanceof RpcError ? error.message : String(error);
    useTranscript.getState().localStatus(sessionId, "error", `发送失败：${message}`);
    return;
  }
  void syncEntryIds(sessionId).catch(() => undefined);
  void useSessions
    .getState()
    .refresh()
    .catch(() => undefined);
}

/** 用最后一次提交的输入重试。 */
export function retryLast(sessionId: string): void {
  const input = useComposer.getState().lastInput[sessionId];
  if (input) void sendPrompt(sessionId, input);
}
