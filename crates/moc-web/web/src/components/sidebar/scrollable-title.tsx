// 会话标题：静置时省略号截断，鼠标指向且确实溢出时横向滚动露出全文。
//
// shadcn / AI Elements 没有等价组件，这里用最少量的自定义样式实现：
// `.moc-scroll-title` 的规则与 keyframes 见 `src/index.css`。

import { type CSSProperties, useLayoutEffect, useRef, useState } from "react";
import { cn } from "@/lib/utils";

export function ScrollableTitle({ text, className }: { text: string; className?: string }) {
  const boxRef = useRef<HTMLSpanElement>(null);
  const [scrolling, setScrolling] = useState({ shift: 0, duration: 4 });

  // 标题字符串变了但盒子宽度不变，ResizeObserver 捕捉不到，只能按 text 重测一次。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 以 text 作为重测信号
  useLayoutEffect(() => {
    const box = boxRef.current;
    if (!box) return;
    const measure = () => {
      const overflow = box.scrollWidth - box.clientWidth;
      setScrolling({
        shift: Math.max(0, overflow),
        // 距离越长滚得越久，避免长标题一闪而过
        duration: Math.min(10, Math.max(2, overflow / 40)),
      });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(box);
    return () => observer.disconnect();
  }, [text]);

  return (
    <span
      ref={boxRef}
      className={cn("moc-scroll-title", className)}
      data-overflow={scrolling.shift > 0 ? "" : undefined}
      style={
        {
          "--moc-title-shift": `-${scrolling.shift}px`,
          "--moc-title-duration": `${scrolling.duration}s`,
        } as CSSProperties
      }
    >
      <span className="moc-scroll-title-text">{text}</span>
    </span>
  );
}
