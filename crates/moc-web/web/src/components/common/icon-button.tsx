// 工作台里到处出现的「图标按钮 + 悬停提示」与快捷键徽标。
// shadcn 的 Button 没有自带 tooltip，这里只做组合，不另起样式体系。

import { type ComponentProps, forwardRef, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

type IconButtonProps = Omit<ComponentProps<typeof Button>, "size"> & {
  label: string;
  /** 提示里额外显示的快捷键。 */
  shortcut?: string;
  side?: "top" | "bottom" | "left" | "right";
  size?: "xs" | "sm" | "md";
  active?: boolean;
};

const SIZES = {
  xs: "size-6 rounded-md [&_svg:not([class*='size-'])]:size-3.5",
  sm: "size-7 rounded-md [&_svg:not([class*='size-'])]:size-4",
  md: "size-8 rounded-lg [&_svg:not([class*='size-'])]:size-4",
};

export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(function IconButton(
  { label, shortcut, side = "bottom", size = "sm", active = false, className, ...props },
  ref,
) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          ref={ref}
          type="button"
          variant="ghost"
          aria-label={label}
          aria-pressed={active || undefined}
          className={cn(
            SIZES[size],
            "p-0 text-muted-foreground hover:bg-accent hover:text-foreground dark:hover:bg-accent",
            active && "bg-accent text-foreground",
            className,
          )}
          {...props}
        />
      </TooltipTrigger>
      <TooltipContent side={side} className="flex items-center gap-2">
        {label}
        {shortcut && <Kbd className="border-background/20 text-background/70">{shortcut}</Kbd>}
      </TooltipContent>
    </Tooltip>
  );
});

export function Kbd({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <kbd
      className={cn(
        "inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-[5px] border border-border px-1 font-sans text-[10.5px] font-medium text-muted-foreground",
        className,
      )}
    >
      {children}
    </kbd>
  );
}
