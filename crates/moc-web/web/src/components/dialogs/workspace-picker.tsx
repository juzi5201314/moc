// 工作区目录选择器：浏览 core 所在机器的目录并据此添加工作区。
//
// 交互对齐 deepseek-harness 的目录选择器：当前目录 + 重命名式路径跳转、逐级进入、
// 新建文件夹、显示隐藏目录；只列目录，不列文件。

import {
  ArrowUpIcon,
  ChevronRightIcon,
  FolderIcon,
  FolderOpenIcon,
  FolderPlusIcon,
  Loader2Icon,
  PencilIcon,
  PlusIcon,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type { FsCreateDirResult, FsListDirResult } from "@/protocol/types";
import { useUi } from "@/store/ui";

/** 目录路径的展示名：主目录显示为「主目录」，其余取最后一段。 */
function dirLabel(path: string, home?: string): string {
  if (home && path === home) return "主目录";
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

function errorMessage(error: unknown): string {
  return error instanceof RpcError ? error.message : String(error);
}

export function WorkspacePicker() {
  const picker = useUi((state) => state.folderPicker);
  const set = useUi((state) => state.set);

  const [listing, setListing] = useState<FsListDirResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [showHidden, setShowHidden] = useState(false);
  /** 非 `null` 时路径行是一段可编辑输入框。 */
  const [pathDraft, setPathDraft] = useState<string | null>(null);
  /** 非 `null` 时列表顶部是一行「新建文件夹」输入框。 */
  const [newName, setNewName] = useState<string | null>(null);
  const newNameRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async (path: string, hidden: boolean) => {
    setLoading(true);
    setError(null);
    try {
      const result = await rpc.call<FsListDirResult>("fs/list_dir", {
        path,
        show_hidden: hidden,
      });
      setListing(result);
      setPathDraft(null);
      setNewName(null);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setLoading(false);
    }
  }, []);

  // 只在打开时取起点（默认主目录），其余依赖变化由各交互自己触发重载。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 只有打开瞬间需要初始化
  useEffect(() => {
    if (!picker.open) return;
    setShowHidden(false);
    setPathDraft(null);
    setNewName(null);
    void load(picker.start ?? "~", false);
  }, [picker.open]);

  useEffect(() => {
    if (newName !== null) newNameRef.current?.focus();
  }, [newName]);

  const close = () => set({ folderPicker: { open: false } });

  const confirm = () => {
    if (!listing) return;
    picker.onPick?.(listing.path);
    close();
  };

  const toggleHidden = () => {
    const next = !showHidden;
    setShowHidden(next);
    if (listing) void load(listing.path, next);
  };

  const submitPath = () => {
    const raw = pathDraft?.trim();
    if (raw) void load(raw, showHidden);
  };

  const createFolder = async () => {
    const name = newName?.trim();
    if (!listing || !name) return;
    setError(null);
    try {
      const created = await rpc.call<FsCreateDirResult>("fs/create_dir", {
        parent: listing.path,
        name,
      });
      await load(created.path, showHidden);
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  const label = listing ? dirLabel(listing.path, listing.home) : "目录";

  return (
    <Dialog open={picker.open} onOpenChange={(open) => !open && close()}>
      <DialogContent className="gap-3 sm:max-w-xl">
        <DialogHeader>
          <DialogTitle className="text-base font-semibold">选择工作区目录</DialogTitle>
        </DialogHeader>

        {/* 当前目录：常驻显示，点铅笔改成手输路径 */}
        <div className="flex h-9 items-center gap-1 rounded-md px-1">
          {pathDraft === null ? (
            <>
              <FolderOpenIcon className="size-4 shrink-0 text-muted-foreground" />
              <span className="shrink-0 text-[13px] font-medium text-foreground">{label}</span>
              <span className="truncate text-[11px] text-muted-foreground" title={listing?.path}>
                {listing?.path}
              </span>
            </>
          ) : (
            <Input
              autoFocus
              value={pathDraft}
              onChange={(event) => setPathDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  submitPath();
                }
                if (event.key === "Escape") setPathDraft(null);
              }}
              placeholder="输入绝对路径或 ~/ 开头的路径"
              className="h-7 font-mono text-xs"
            />
          )}

          <div className="ml-auto flex shrink-0 items-center gap-0.5">
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              aria-label="上一级目录"
              title="上一级目录"
              disabled={!listing?.parent}
              onClick={() => listing?.parent && void load(listing.parent, showHidden)}
            >
              <ArrowUpIcon />
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              aria-label={pathDraft === null ? "手动输入路径" : "取消手动输入"}
              title={pathDraft === null ? "手动输入路径" : "取消手动输入"}
              onClick={() =>
                setPathDraft((draft) => (draft === null ? (listing?.path ?? "") : null))
              }
            >
              <PencilIcon />
            </Button>
          </div>
        </div>

        {/* 子目录列表 */}
        <div className="h-72 overflow-y-auto rounded-lg border border-border bg-background p-1">
          {newName !== null && (
            <div className="flex items-center gap-2.5 rounded-md bg-accent px-2.5 py-1.5">
              <FolderPlusIcon className="size-4 shrink-0 text-muted-foreground" />
              <Input
                ref={newNameRef}
                value={newName}
                onChange={(event) => setNewName(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void createFolder();
                  }
                  if (event.key === "Escape") setNewName(null);
                }}
                placeholder="新建文件夹名称"
                className="h-7 text-[13px]"
              />
            </div>
          )}

          {loading ? (
            <div className="flex h-full items-center justify-center text-muted-foreground">
              <Loader2Icon className="size-4 animate-spin" />
            </div>
          ) : listing && listing.entries.length === 0 && newName === null ? (
            <p className="py-10 text-center text-xs text-muted-foreground">没有子目录</p>
          ) : (
            listing?.entries.map((entry) => (
              <button
                key={entry.path}
                type="button"
                onClick={() => void load(entry.path, showHidden)}
                className="group flex h-9 w-full items-center gap-2.5 rounded-md px-2.5 text-left text-[13px] text-foreground transition-colors hover:bg-accent"
              >
                <FolderIcon className="size-4 shrink-0 text-muted-foreground" />
                <span className="truncate">{entry.name}</span>
                <ChevronRightIcon className="ml-auto size-3.5 shrink-0 text-muted-foreground/40 group-hover:text-muted-foreground" />
              </button>
            ))
          )}
        </div>

        {error && (
          <p role="alert" className="text-xs text-destructive">
            {error}
          </p>
        )}

        <DialogFooter className="items-center justify-between gap-2 sm:justify-between">
          <div className="flex items-center gap-1">
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="text-xs"
              disabled={!listing || newName !== null}
              onClick={() => setNewName("")}
            >
              <PlusIcon className="size-3.5" />
              新建文件夹
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-pressed={showHidden}
              className={cn("text-xs", showHidden ? "text-foreground" : "text-muted-foreground")}
              onClick={toggleHidden}
            >
              显示隐藏文件
            </Button>
          </div>
          <div className="flex items-center gap-2">
            <Button type="button" variant="outline" size="sm" className="text-xs" onClick={close}>
              取消
            </Button>
            <Button
              type="button"
              size="sm"
              className="text-xs"
              disabled={!listing || loading}
              onClick={confirm}
            >
              打开
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
