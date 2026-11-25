// 会话侧栏的分段展开与一键收起：避免长列表一次性挂载数百条会话。

import { useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";

/** 每次点击展开增量显示的会话数。 */
export const SESSION_PAGE_STEP = 20;
/** 搜索态首屏展示的会话数。 */
export const SEARCH_VISIBLE_SESSIONS = 20;
/** 剩余条数不超过「一步 + 余量」时一次展完，避免末尾只剩寥寥几条仍需再点一次。 */
const TAIL_MERGE = 4;

/** 计算本次分段展开应增加的条数。 */
export function nextPageStep(hidden: number, step = SESSION_PAGE_STEP): number {
  if (hidden <= 0) return 0;
  return hidden <= step + TAIL_MERGE ? hidden : step;
}

/** 列表底部控制行：还有剩余时按段展开，展开超过默认条数后同排提供一键全部收起。 */
export function SessionPagingRow({
  hidden,
  canCollapse,
  onMore,
  onCollapse,
  className,
}: {
  hidden: number;
  canCollapse: boolean;
  onMore: (step: number) => void;
  onCollapse: () => void;
  className?: string;
}) {
  const t = useT();
  if (hidden <= 0 && !canCollapse) return null;
  const step = nextPageStep(hidden);
  return (
    <div
      className={cn(
        "flex h-9 items-center justify-between gap-2 text-ui text-text-tertiary",
        className,
      )}
    >
      {hidden > 0 && (
        <button
          type="button"
          onClick={() => onMore(step)}
          className="min-w-0 truncate transition-colors hover:text-foreground"
        >
          {step < hidden
            ? t("sb.more_step", { step, total: hidden })
            : t("sb.more_n", { n: hidden })}
        </button>
      )}
      {canCollapse && (
        <button
          type="button"
          onClick={onCollapse}
          className={cn(
            "shrink-0 transition-colors hover:text-foreground",
            hidden > 0 && "ml-auto",
          )}
        >
          {t("th.collapse")}
        </button>
      )}
    </div>
  );
}
