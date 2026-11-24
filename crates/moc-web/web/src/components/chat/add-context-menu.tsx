// 输入框左下的「+」：上传图片，或向输入框插入 @ / / / 资源 URI 前缀并弹出对应补全。
// 只列 moc 已有的引用能力；选中后不另开搜索面板，统一复用输入框补全。
import {
  BookOpenIcon,
  ClipboardListIcon,
  FileTextIcon,
  FolderIcon,
  HistoryIcon,
  PackageIcon,
  PaperclipIcon,
  PlusIcon,
  SquareSlashIcon,
} from "lucide-react";
import { type ReactNode, useRef } from "react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { type MsgKey, useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { CHIP } from "./composer-parts";

export type AttachKind = "file" | "dir";

/** 插入输入框的前缀；`kind` 只约束 `@` 工作区补全，`/` 走斜杠命令补全。 */
const ITEMS: { label: MsgKey; icon: ReactNode; prefix: string; kind?: AttachKind }[] = [
  { label: "ac.files", icon: <FileTextIcon />, prefix: "@", kind: "file" },
  { label: "ac.folders", icon: <FolderIcon />, prefix: "@", kind: "dir" },
  { label: "ac.commands", icon: <SquareSlashIcon />, prefix: "/" },
  { label: "ac.skills", icon: <BookOpenIcon />, prefix: "skill://" },
  { label: "ac.plans", icon: <ClipboardListIcon />, prefix: "local://" },
  { label: "ac.history", icon: <HistoryIcon />, prefix: "history://" },
  { label: "ac.artifacts", icon: <PackageIcon />, prefix: "artifact://" },
];

export function AddContextMenu({
  disabled,
  canCommand,
  canReference,
  onUpload,
  onInsert,
  onClose,
}: {
  disabled: boolean;
  /** 斜杠命令只在空输入框里生效。 */
  canCommand: boolean;
  /** 资源补全需要已建立的会话。 */
  canReference: boolean;
  onUpload: () => void;
  onInsert: (prefix: string, kind?: AttachKind) => void;
  /** 菜单关闭后把焦点还给输入框，插入前缀后可直接接着打字。 */
  onClose: () => void;
}) {
  const t = useT();
  // 插入推迟到菜单完全关闭：菜单的外部点击层还在时弹出补全，补全会被当作外部交互立刻关掉
  const pending = useRef<(() => void) | null>(null);
  return (
    <DropdownMenu>
      <Tooltip>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <button
              type="button"
              disabled={disabled}
              className={cn(CHIP, "w-8 justify-center px-0")}
              aria-label={t("ac.title")}
            >
              <PlusIcon className="size-4" />
            </button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">{t("ac.hint")}</TooltipContent>
      </Tooltip>
      <DropdownMenuContent
        align="start"
        side="top"
        sideOffset={8}
        className="w-56 p-1"
        // 关闭后焦点交给输入框而不是「+」
        onCloseAutoFocus={(event) => {
          event.preventDefault();
          onClose();
          pending.current?.();
          pending.current = null;
        }}
      >
        <DropdownMenuItem onSelect={onUpload}>
          <PaperclipIcon />
          {t("ac.upload")}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {ITEMS.map((item) => (
          <DropdownMenuItem
            key={item.label}
            disabled={item.prefix === "/" ? !canCommand : !canReference}
            onSelect={() => {
              pending.current = () => onInsert(item.prefix, item.kind);
            }}
          >
            {item.icon}
            {t(item.label)}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
