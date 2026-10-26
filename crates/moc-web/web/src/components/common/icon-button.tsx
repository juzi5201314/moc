// 带 aria-label 与 Tooltip 的图标按钮：所有纯图标按钮都走这里，保证可访问性。

import type { ComponentProps, ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

type IconButtonProps = Omit<ComponentProps<typeof Button>, "children" | "aria-label"> & {
  label: string;
  /** 快捷键提示，显示在 tooltip 里。 */
  shortcut?: string;
  side?: "top" | "bottom" | "left" | "right";
  children: ReactNode;
};

export function IconButton({
  label,
  shortcut,
  side = "bottom",
  variant = "ghost",
  size = "icon-sm",
  children,
  ...props
}: IconButtonProps) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button aria-label={label} variant={variant} size={size} {...props}>
          {children}
        </Button>
      </TooltipTrigger>
      <TooltipContent side={side} sideOffset={4}>
        {label}
        {shortcut ? <span className="ml-2 opacity-60">{shortcut}</span> : null}
      </TooltipContent>
    </Tooltip>
  );
}
