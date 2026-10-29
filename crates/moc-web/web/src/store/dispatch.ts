// 待发区派发：steer 被 core 应用后移出待发区；队列项在提示结束后按顺序自动发送。
// 设置或计划审阅弹窗打开时暂停派发，弹窗关闭后补发。

import { rpc } from "@/protocol/client";
import { sendPrompt } from "./actions";
import { toInput, useComposer } from "./composer";
import { useTranscript } from "./transcript";
import { dispatchPaused, useUi } from "./ui";

/** 会话空闲且未暂停时，发出队首的队列项。 */
export function dispatchNext(sessionId: string): void {
  if (dispatchPaused(useUi.getState())) return;
  const view = useTranscript.getState().bySession[sessionId];
  if (!view || view.running || view.readOnly) return;
  const next = (useComposer.getState().pending[sessionId] ?? []).find(
    (item) => item.kind === "queue",
  );
  if (!next) return;
  useComposer.getState().removePending(sessionId, next.key);
  void sendPrompt(sessionId, toInput(next.text, next.images), { mode: next.mode });
}

rpc.on("session/update", ({ session_id: sessionId, update }) => {
  if (update.type === "steering_applied") {
    const item = (useComposer.getState().pending[sessionId] ?? []).find(
      (entry) => entry.kind === "steer" && entry.steerId === update.id,
    );
    if (item) useComposer.getState().removePending(sessionId, item.key);
  } else if (update.type === "prompt_finished") {
    // transcript store 的同名监听先注册，这里 running 已经是 false。
    queueMicrotask(() => dispatchNext(sessionId));
  }
});

let wasPaused = dispatchPaused(useUi.getState());
useUi.subscribe((state) => {
  const paused = dispatchPaused(state);
  if (wasPaused && !paused) {
    for (const sessionId of Object.keys(useComposer.getState().pending)) dispatchNext(sessionId);
  }
  wasPaused = paused;
});
