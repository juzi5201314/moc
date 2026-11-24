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

export type RefChipData = {
  /** `command` / `image` / `file-ref` 或 `resource/inspect` 返回的资源类别。 */
  kind: string;
  label: string;
  detail?: string | null;
  /** 图片引用的缩略图地址。 */
  thumb?: string;
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
}: {
  chip: RefChipData;
  /** 消息气泡里复制时带出的原始 token；可见短名不参与选择。 */
  raw?: string;
}) {
  const Icon = ICONS[chip.kind] ?? FileTextIcon;
  const body = (
    <span
      // 输入框里由 CodeMirror 承担光标与删除，控件本身不可选中、不抢焦点
      contentEditable={false}
      className="mx-px inline-flex h-5 max-w-64 items-center gap-1 rounded-md bg-accent px-1 align-middle text-[0.92em] leading-5 text-foreground select-none"
    >
      {chip.thumb ? (
        <img src={chip.thumb} alt="" className="size-3.5 shrink-0 rounded-[3px] object-cover" />
      ) : (
        <Icon className="size-3.5 shrink-0 text-muted-foreground" />
      )}
      <span className="truncate">{chip.label}</span>
    </span>
  );
  return (
    <>
      {raw !== undefined && <span className="sr-only">{raw}</span>}
      {chip.detail ? (
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
