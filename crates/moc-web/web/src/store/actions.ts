// 跨组件共享的会话动作：发送提示（composer 与「重试」共用）。

import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type { InputPart, PromptContract, PromptResult } from "@/protocol/types";
import { useComposer } from "./composer";
import { newSession, syncEntryIds } from "./lifecycle";
import { useSessions } from "./sessions";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";

export type SendOptions = { contract?: PromptContract; echo?: boolean };

/**
 * 发送一条提示。本地先插入用户块（`echo` 默认 true），流式内容走 session/update；
 * 返回时提示已结束，再补 entry id 并刷新侧栏。
 * 如果当前处于空白临时会话（draft-session），在此处创建真正的后端会话并无缝切换。
 */
export async function sendPrompt(
  sessionId: string,
  input: InputPart[],
  options: SendOptions = {},
): Promise<void> {
  if (input.length === 0) return;

  // 1. 如果是临时草稿会话，先提交后端创建真实会话
  if (sessionId === "draft-session") {
    const draft = useSessions.getState().draftSession;
    const realInfo = await newSession({
      cwd: draft?.cwd,
      model: draft?.model,
      mode: draft?.mode ?? "normal",
      plan: draft?.plan ?? false,
    });
    useSessions.getState().setDraftSession(null);
    useTabs.getState().replace("draft-session", realInfo.id);
    useTranscript.getState().drop("draft-session");
    // 对真实的 session 发送提示词
    return sendPrompt(realInfo.id, input, options);
  }

  const store = useTranscript.getState();
  if (options.echo ?? true) {
    const text = input.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("\n");
    const images = input.flatMap((part) => (part.type === "image" ? [part.source] : []));
    store.localUser(sessionId, text, images);
  }
  useComposer.getState().setLastInput(sessionId, input, options.contract);
  try {
    await rpc.call<PromptResult>("session/prompt", {
      session_id: sessionId,
      input,
      ...(options.contract ? { contract: options.contract } : {}),
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

/** 用最后一次提交的输入重试（保留原执行契约）。 */
export function retryLast(sessionId: string): void {
  const last = useComposer.getState().lastInput[sessionId];
  if (last) void sendPrompt(sessionId, last.input, { contract: last.contract });
}
