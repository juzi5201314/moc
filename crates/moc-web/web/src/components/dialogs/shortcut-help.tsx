// 快捷键帮助（Ctrl+/）。列表与 lib/shortcuts 的绑定保持一致。

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useUi } from "@/store/ui";

const GROUPS: { title: string; items: [keys: string, label: string][] }[] = [
  {
    title: "全局",
    items: [
      ["Ctrl+K", "命令面板"],
      ["Alt+N", "新建会话"],
      ["Alt+W", "关闭标签"],
      ["Alt+[ / Alt+]", "上一个 / 下一个标签"],
      ["Alt+1..9", "跳到第 n 个标签"],
      ["Alt+S", "切换侧栏"],
      ["Alt+R", "切换右侧面板"],
      ["Ctrl+;", "打开旁路"],
      ["Ctrl+/", "快捷键帮助"],
    ],
  },
  {
    title: "会话",
    items: [
      ["Alt+P", "模型选择"],
      ["Alt+M", "循环切换模式"],
      ["Alt+T", "切换思考显示档位"],
      ["Alt+O", "切换工具显示档位"],
      ["Esc", "停止生成 / 关闭浮层"],
    ],
  },
  {
    title: "输入框",
    items: [
      ["Enter", "发送（运行中：插队或中断）"],
      ["Shift+Enter", "换行"],
      ["Ctrl+Enter", "加入队列"],
      ["Alt+↑", "撤回最近一条待发消息"],
      ["/", "斜杠命令"],
      ["!cmd / !!cmd", "运行 shell（!! 不进上下文）"],
    ],
  },
  {
    title: "授权卡片",
    items: [
      ["1..n", "选择对应选项"],
      ["Esc", "拒绝"],
    ],
  },
];

export function ShortcutHelp() {
  const open = useUi((state) => state.help);
  const set = useUi((state) => state.set);
  return (
    <Dialog open={open} onOpenChange={(next) => set({ help: next })}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>快捷键</DialogTitle>
          <DialogDescription>避开了浏览器保留的 Ctrl+N/T/W/Tab。</DialogDescription>
        </DialogHeader>
        <div className="max-h-[60vh] space-y-4 overflow-y-auto">
          {GROUPS.map((group) => (
            <section key={group.title}>
              <h3 className="mb-1 font-medium text-muted-foreground text-xs">{group.title}</h3>
              <dl className="divide-y rounded-lg border text-sm">
                {group.items.map(([keys, label]) => (
                  <div key={keys + label} className="flex items-center justify-between px-3 py-1.5">
                    <dt>{label}</dt>
                    <dd>
                      <kbd className="rounded border bg-muted px-1.5 py-0.5 font-mono text-xs">
                        {keys}
                      </kbd>
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}
