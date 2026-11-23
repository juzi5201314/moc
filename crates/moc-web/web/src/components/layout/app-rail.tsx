// 最左侧标签栏：顶部切换顶层页面，底部放设置与会话列表折叠；坐在窗框 chrome 上，不属于任何卡片。

import {
  ImageIcon,
  MessagesSquareIcon,
  PanelLeftIcon,
  PuzzleIcon,
  SettingsIcon,
} from "lucide-react";
import type { ReactNode } from "react";
import { Kbd } from "@/components/common/icon-button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useT } from "@/lib/i18n";
import { type AppRoute, navigate, useRoute } from "@/lib/route";
import { useNarrow } from "@/lib/use-narrow";
import { cn } from "@/lib/utils";
import { useLayout } from "@/store/layout";
import { openSettings } from "@/store/ui";

function RailButton({
  label,
  shortcut,
  active,
  pressed,
  onClick,
  children,
}: {
  label: string;
  shortcut?: string;
  active?: boolean;
  /** 开关型按钮（折叠）：只表达按下态，不是当前页。 */
  pressed?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={label}
          aria-current={active ? "page" : undefined}
          aria-pressed={pressed}
          onClick={onClick}
          className={cn(
            "flex size-9 items-center justify-center rounded-[10px] transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring [&_svg]:size-5",
            active || pressed
              ? "bg-black/[0.07] text-foreground dark:bg-white/[0.09]"
              : "text-muted-foreground hover:bg-black/[0.045] hover:text-foreground dark:hover:bg-white/[0.06]",
          )}
        >
          {children}
        </button>
      </TooltipTrigger>
      <TooltipContent side="right" className="flex items-center gap-2">
        {label}
        {shortcut && <Kbd className="border-background/20 text-background/70">{shortcut}</Kbd>}
      </TooltipContent>
    </Tooltip>
  );
}

const TABS: {
  route: AppRoute;
  label: "sb.sessions" | "rail.image" | "sd.plugins";
  icon: ReactNode;
}[] = [
  { route: "agent", label: "sb.sessions", icon: <MessagesSquareIcon /> },
  { route: "image", label: "rail.image", icon: <ImageIcon /> },
  { route: "plugins", label: "sd.plugins", icon: <PuzzleIcon /> },
];

export function AppRail({ hasSidebar }: { hasSidebar: boolean }) {
  const t = useT();
  const route = useRoute();
  const narrow = useNarrow();
  const collapsed = useLayout((state) => state.sidebarCollapsed);
  const overlay = useLayout((state) => state.sidebarOverlay);
  const sidebarShown = narrow ? overlay : !collapsed;

  return (
    <nav
      aria-label={t("rail.label")}
      className="flex w-[52px] shrink-0 flex-col items-center justify-between py-1.5 md:py-0"
    >
      <div className="flex flex-col items-center gap-1">
        {TABS.map((tab) => (
          <RailButton
            key={tab.route}
            label={t(tab.label)}
            active={route === tab.route}
            onClick={() => {
              if (narrow) useLayout.getState().setSidebarOverlay(false);
              navigate(tab.route);
            }}
          >
            {tab.icon}
          </RailButton>
        ))}
      </div>
      <div className="flex flex-col items-center gap-1">
        <RailButton
          label={t("sb.settings")}
          onClick={() => openSettings(route === "image" ? "image_playground" : undefined)}
        >
          <SettingsIcon />
        </RailButton>
        {hasSidebar && (
          <RailButton
            label={sidebarShown ? t("layout.close_sidebar") : t("hd.sidebar")}
            shortcut="Ctrl B"
            pressed={sidebarShown}
            onClick={() => useLayout.getState().toggleSidebar()}
          >
            <PanelLeftIcon />
          </RailButton>
        )}
      </div>
    </nav>
  );
}
