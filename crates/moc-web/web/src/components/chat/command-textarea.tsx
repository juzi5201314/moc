import { type ComponentProps, useCallback, useLayoutEffect, useRef } from "react";
import { cn } from "@/lib/utils";

type Props = ComponentProps<"textarea"> & {
  command: { start: number; end: number } | null;
};

// 原生 textarea 保留输入、选择和 IME；镜像层只负责命令着色，与其共享排版和滚动。
export function CommandTextarea({ command, value, className, ref, onScroll, ...props }: Props) {
  const input = useRef<HTMLTextAreaElement>(null);
  const mirror = useRef<HTMLDivElement>(null);
  const bind = useCallback(
    (node: HTMLTextAreaElement | null) => {
      input.current = node;
      if (typeof ref === "function") return ref(node);
      if (ref) ref.current = node;
    },
    [ref],
  );
  const sync = useCallback(() => {
    if (!input.current || !mirror.current) return;
    mirror.current.style.width = `${input.current.clientWidth}px`;
    mirror.current.style.height = `${input.current.clientHeight}px`;
    mirror.current.scrollTop = input.current.scrollTop;
    mirror.current.scrollLeft = input.current.scrollLeft;
  }, []);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 输入变化后同步镜像滚动位置
  useLayoutEffect(() => {
    if (!command || !input.current) return;
    sync();
    const observer = new ResizeObserver(sync);
    observer.observe(input.current);
    return () => observer.disconnect();
  }, [command, value, sync]);
  const text = String(value ?? "");
  return (
    <div className="relative">
      {command && (
        <div
          ref={mirror}
          aria-hidden="true"
          className={cn(
            className,
            "pointer-events-none absolute left-0 top-0 overflow-hidden whitespace-pre-wrap break-words",
          )}
        >
          {text.slice(0, command.start)}
          <span className="text-brand">{text.slice(command.start, command.end)}</span>
          {text.slice(command.end)}
          {"\u200b"}
        </div>
      )}
      <textarea
        {...props}
        ref={bind}
        value={value}
        className={cn(
          className,
          command && "text-transparent caret-foreground selection:text-transparent",
        )}
        onScroll={(event) => {
          sync();
          onScroll?.(event);
        }}
      />
    </div>
  );
}
