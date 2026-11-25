// 引用控件：输入框与用户消息里有效引用的整体呈现（图标 + 短名，悬停给完整路径/说明）。

import {
  BookOpenIcon,
  ClipboardListIcon,
  FileTextIcon,
  FolderIcon,
  HistoryIcon,
  ImageIcon,
  PackageIcon,
  PaperclipIcon,
  SquareSlashIcon,
  WrenchIcon,
} from "lucide-react";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

export type RefChipData = {
  /** `command` / `image` / `file-ref` 或 `resource/inspect` 返回的资源类别。 */
  kind: string;
  label: string;
  /** 命令胶囊内紧跟命令名展示的非自然语言参数值（如 `["pause"]`、`["3"]`、`["cleanup", "all"]`）。 */
  args?: string[];
  detail?: string | null;
  /** 图片引用的缩略图地址。 */
  thumb?: string;
  /** 是否可点击编辑（带非自然语言参数的命令胶囊）。 */
  interactive?: boolean;
  /** 正在打开参数编辑浮层时保持高亮态并抑制悬浮 Tooltip。 */
  active?: boolean;
};

const ICONS: Record<string, typeof FileTextIcon> = {
  command: SquareSlashIcon,
  image: ImageIcon,
  "file-ref": PaperclipIcon,
  file: FileTextIcon,
  dir: FolderIcon,
  skill: BookOpenIcon,
  local: ClipboardListIcon,
  history: HistoryIcon,
  artifact: PackageIcon,
  tool: WrenchIcon,
};

export function RefChip({
  chip,
  raw,
  onClick,
}: {
  chip: RefChipData;
  /** 消息气泡里复制时带出的原始 token；可见短名不参与选择。 */
  raw?: string;
  /** 可交互命令胶囊点击回调。 */
  onClick?: () => void;
}) {
  const Icon = ICONS[chip.kind] ?? FileTextIcon;
  const hasArgs = Boolean(chip.args && chip.args.length > 0);
  const clickable = Boolean(chip.interactive && onClick);
  const className = cn(
    "mx-px inline-flex h-5 items-center gap-1 rounded-md bg-accent align-middle text-[0.92em] leading-5 text-foreground select-none",
    hasArgs ? "max-w-80 px-1.5" : "max-w-64 px-1",
    clickable &&
      "cursor-pointer transition-colors hover:bg-foreground/[0.09] focus-visible:outline-none data-[active=true]:bg-foreground/[0.11]",
  );

  const inner = (
    <>
      {chip.thumb ? (
        <img src={chip.thumb} alt="" className="size-3.5 shrink-0 rounded-[3px] object-cover" />
      ) : (
        <Icon className="size-3.5 shrink-0 text-muted-foreground" />
      )}
      <span className={cn(hasArgs ? "shrink-0" : "truncate")}>{chip.label}</span>
      {hasArgs && (
        <span data-slot="ref-chip-args" className="truncate text-muted-foreground">
          {chip.args?.join(" ")}
        </span>
      )}
    </>
  );

  const body = clickable ? (
    <button
      type="button"
      data-slot="ref-chip"
      data-active={chip.active || undefined}
      aria-haspopup="dialog"
      aria-expanded={Boolean(chip.active)}
      onMouseDown={(event) => event.preventDefault()}
      onClick={(event) => {
        event.stopPropagation();
        onClick?.();
      }}
      className={className}
    >
      {inner}
    </button>
  ) : (
    <span
      // 输入框里由 CodeMirror 承担光标与删除，控件本身不可选中、不抢焦点
      contentEditable={false}
      data-slot="ref-chip"
      className={className}
    >
      {inner}
    </span>
  );

  return (
    <>
      {raw !== undefined && <span className="sr-only">{raw}</span>}
      {chip.detail && !chip.active ? (
        <Tooltip>
          <TooltipTrigger asChild>{body}</TooltipTrigger>
          <TooltipContent side="top" className="max-w-[min(36rem,90vw)] break-all">
            {chip.detail}
          </TooltipContent>
        </Tooltip>
      ) : (
        body
      )}
    </>
  );
}
