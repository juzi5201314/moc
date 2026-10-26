// 右侧面板：计划 / Todo / 变更 / 子代理 / 上下文 / 旁路。各页内容由后续组件填充。

import { X } from "lucide-react";
import type { ReactNode } from "react";
import { IconButton } from "@/components/common/icon-button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { type RightTab, usePrefs } from "@/store/prefs";

export const RIGHT_TAB_LABEL: Record<RightTab, string> = {
  plan: "计划",
  todo: "Todo",
  changes: "变更",
  agents: "子代理",
  context: "上下文",
  btw: "旁路",
};

const TAB_ORDER: RightTab[] = ["plan", "todo", "changes", "agents", "context", "btw"];

/** 各页内容注册表；未注册的页显示占位。 */
export const RIGHT_PANES: Partial<Record<RightTab, (props: { sessionId: string }) => ReactNode>> =
  {};

export function RightPanel({ sessionId }: { sessionId: string | null }) {
  const tab = usePrefs((state) => state.rightPanel.tab);
  const setRightPanel = usePrefs((state) => state.setRightPanel);

  return (
    <aside aria-label="右侧面板" className="flex h-full min-w-0 flex-col border-l bg-panel">
      <Tabs
        value={tab}
        onValueChange={(value) => setRightPanel({ tab: value as RightTab })}
        className="flex min-h-0 flex-1 flex-col gap-0"
      >
        <div className="flex items-center gap-1 border-b px-2 py-1.5">
          <TabsList className="h-8 min-w-0 flex-1 justify-start overflow-x-auto bg-transparent p-0">
            {TAB_ORDER.map((item) => (
              <TabsTrigger key={item} value={item} className="h-7 flex-none px-2 text-xs">
                {RIGHT_TAB_LABEL[item]}
              </TabsTrigger>
            ))}
          </TabsList>
          <IconButton
            label="关闭面板"
            shortcut="Alt+R"
            onClick={() => setRightPanel({ open: false })}
          >
            <X aria-hidden />
          </IconButton>
        </div>
        {TAB_ORDER.map((item) => {
          const Pane = RIGHT_PANES[item];
          return (
            <TabsContent key={item} value={item} className="min-h-0 flex-1 overflow-y-auto">
              {!sessionId ? (
                <p className="p-6 text-center text-muted-foreground text-sm">未打开会话</p>
              ) : Pane ? (
                <Pane sessionId={sessionId} />
              ) : (
                <p className="p-6 text-center text-muted-foreground text-sm">暂无内容</p>
              )}
            </TabsContent>
          );
        })}
      </Tabs>
    </aside>
  );
}
