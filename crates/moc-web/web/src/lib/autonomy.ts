// 会话自主程度的展示文案；会话选项菜单与 `/autonomy` 共用。顺序即循环切换顺序。

import type { MsgKey } from "@/lib/i18n";
import type { SessionAutonomy } from "@/protocol/types";

export const AUTONOMY: { id: SessionAutonomy; label: MsgKey; description: MsgKey }[] = [
  { id: "default", label: "autonomy.default", description: "autonomy.default_desc" },
  { id: "dont_ask", label: "autonomy.dont_ask", description: "autonomy.dont_ask_desc" },
  {
    id: "unattended",
    label: "autonomy.unattended",
    description: "autonomy.unattended_desc",
  },
];
