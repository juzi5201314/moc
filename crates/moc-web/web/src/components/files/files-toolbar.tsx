// 文件分页顶部：左「工作区 | 会话」分段切换，右操作按钮。

import {
  FilePlus2Icon,
  FolderPlusIcon,
  MoreHorizontalIcon,
  RefreshCwIcon,
  UploadIcon,
} from "lucide-react";
import { useRef } from "react";
import { IconButton } from "@/components/common/icon-button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { type MsgKey, useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import {
  type FileRoot,
  refreshExpanded,
  setFileRoot,
  startNaming,
  uploadFiles,
  useFiles,
} from "@/store/files";
import { useLayout } from "@/store/layout";

export function FilesToolbar({
  sessionId,
  workspaceRoot,
}: {
  sessionId: string;
  workspaceRoot: FileRoot | null;
}) {
  const t = useT();
  const root = useFiles((state) => state.bySession[sessionId]?.root ?? "workspace");
  const hidden = useLayout((state) => state.filesShowHidden);
  const inputRef = useRef<HTMLInputElement>(null);
  const cwd = workspaceRoot?.kind === "workspace" ? workspaceRoot.cwd : null;

  const sessionRoot: FileRoot = { kind: "session", sessionId };

  const refresh = () => {
    const target = root === "workspace" ? workspaceRoot : sessionRoot;
    if (target) refreshExpanded(sessionId, target);
  };

  const startCreate = (kind: "new-file" | "new-dir") => {
    if (!workspaceRoot || !cwd) return;
    startNaming(sessionId, { parentKey: cwd, name: "", kind });
  };

  const upload = (files: FileList | null) => {
    if (!files || !workspaceRoot || !cwd) return;
    void uploadFiles(sessionId, workspaceRoot, cwd, [...files]);
  };

  const collapseAll = () =>
    useFiles.setState((state) => {
      const current = state.bySession[sessionId];
      return current
        ? { bySession: { ...state.bySession, [sessionId]: { ...current, expanded: {} } } }
        : state;
    });

  return (
    <div className="flex h-9 shrink-0 items-center gap-1 px-2">
      <div className="flex items-center rounded-lg bg-muted p-0.5">
        {(
          [
            ["workspace", "ft.workspace" as MsgKey],
            ["session", "ft.session" as MsgKey],
          ] as const
        ).map(([id, label]) => (
          <button
            key={id}
            type="button"
            onClick={() => setFileRoot(sessionId, id)}
            className={cn(
              "h-6 rounded-md px-2 text-ui transition-colors",
              root === id
                ? "bg-background font-medium text-foreground shadow-xs"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {t(label)}
          </button>
        ))}
      </div>
      <div className="ml-auto flex items-center gap-0.5">
        {root === "workspace" && (
          <>
            <IconButton label={t("ft.new_file")} size="xs" onClick={() => startCreate("new-file")}>
              <FilePlus2Icon />
            </IconButton>
            <IconButton label={t("ft.new_dir")} size="xs" onClick={() => startCreate("new-dir")}>
              <FolderPlusIcon />
            </IconButton>
            <IconButton label={t("ft.upload")} size="xs" onClick={() => inputRef.current?.click()}>
              <UploadIcon />
            </IconButton>
          </>
        )}
        <IconButton label={t("ft.refresh")} size="xs" onClick={refresh}>
          <RefreshCwIcon />
        </IconButton>
        {root === "workspace" && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <span>
                <IconButton label={t("sb.more")} size="xs">
                  <MoreHorizontalIcon />
                </IconButton>
              </span>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-44">
              <DropdownMenuCheckboxItem
                checked={hidden}
                onCheckedChange={(checked) => useLayout.getState().setFilesShowHidden(checked)}
              >
                {t("wsp.hidden")}
              </DropdownMenuCheckboxItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={collapseAll}>{t("ft.collapse")}</DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </div>
      <input
        ref={inputRef}
        type="file"
        multiple
        hidden
        onChange={(event) => {
          upload(event.target.files);
          event.target.value = "";
        }}
      />
    </div>
  );
}
