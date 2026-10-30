// 待发区派发：steer 被 core 应用后移出待发区；队列项在提示结束后按顺序自动发送。
// 设置或计划审阅弹窗打开时暂停派发，弹窗关闭后补发。
// unattended 自主程度下计划提交不等审阅，代用户批准「压缩并执行」（与 TUI / CLI 一致）。
// 本端提示的续发与自动批准都在 actions 里等 RPC 返回后进行，这里只处理事件分流。

import { rpc } from "@/protocol/client";
import { deferAutoApproval, dispatchNext, hasOwnPrompt } from "./actions";
import { useComposer } from "./composer";
import { useTranscript } from "./transcript";
import { dispatchPaused, useUi } from "./ui";

rpc.on("session/update", ({ session_id: sessionId, update }) => {
  if (update.type === "steering_applied") {
    const item = (useComposer.getState().pending[sessionId] ?? []).find(
      (entry) => entry.kind === "steer" && entry.steerId === update.id,
    );
    if (item) useComposer.getState().removePending(sessionId, item.key);
  } else if (update.type === "prompt_finished") {
    // 其它前端发起的提示没有 RPC 可等，只能在结束推送后尝试；transcript store 的同名
    // 监听先注册，这里 running 已经是 false。本端提示由 dispatchNext 自行跳过。
    queueMicrotask(() => dispatchNext(sessionId));
  } else if (update.type === "plan_submitted") {
    // 只接管本端发起的提示：其它前端提交的计划由它自己按自主程度处理。
    const autonomy = useTranscript.getState().bySession[sessionId]?.info.autonomy;
    if (autonomy === "unattended" && hasOwnPrompt(sessionId)) {
      deferAutoApproval(sessionId, update.plan.plan_id);
    }
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
