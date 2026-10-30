// 会话自主程度的展示文案；会话选项菜单与 `/autonomy` 共用。顺序即循环切换顺序。

import type { SessionAutonomy } from "@/protocol/types";

export const AUTONOMY: { id: SessionAutonomy; label: string; description: string }[] = [
  { id: "default", label: "逐项确认", description: "提问与计划都等你本人回应" },
  { id: "dont_ask", label: "自动作答", description: "模型提问时按推荐项代答，计划仍需审阅" },
  {
    id: "unattended",
    label: "无人值守",
    description: "在自动作答之上，计划提交后自动压缩并执行",
  },
];
