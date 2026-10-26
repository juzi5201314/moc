// 目录选择器：调 `fs/list_dir` 浏览 core 所在机器的目录。顶部可编辑路径（回车跳转）+ 面包屑，
// 主目录 / 上一级按钮，git 仓库带标记，可切换显示隐藏目录。

import { ArrowUp, Folder, FolderGit2, Home, Loader2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import { FS_LIST_DIR_MAX_ENTRIES, type FsListDirResult } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useUi } from "@/store/ui";

/** 绝对路径 → 面包屑（每段带累积路径）。兼容 `/` 与 Windows 盘符。 */
export function breadcrumbs(path: string): { name: string; path: string }[] {
  const parts = path.split(/[\\/]/).filter(Boolean);
  if (/^[A-Za-z]:[\\/]?/.test(path)) {
    // Windows：首段是盘符，根是 `C:\`，其余段用 `\` 拼在根后面。
    const root = `${parts[0]}\\`;
    return parts.map((name, index) => ({
      name,
      path: index === 0 ? root : root + parts.slice(1, index + 1).join("\\"),
    }));
  }
  return [
    { name: "/", path: "/" },
    ...parts.map((name, index) => ({ name, path: `/${parts.slice(0, index + 1).join("/")}` })),
  ];
}

export function FolderPicker() {
  const state = useUi((s) => s.folderPicker);
  const set = useUi((s) => s.set);
  const fallback = useConnection((s) => s.config?.cwd);
  const [listing, setListing] = useState<FsListDirResult | null>(null);
  const [input, setInput] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [showHidden, setShowHidden] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);

  const load = useCallback(async (path: string | undefined, hidden: boolean) => {
    setLoading(true);
    setError(null);
    try {
      const result = await rpc.call<FsListDirResult>("fs/list_dir", {
        ...(path ? { path } : {}),
        show_hidden: hidden,
      });
      setListing(result);
      setInput(result.path);
      setSelected(null);
    } catch (err) {
      setError(err instanceof RpcError ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  // 只在打开时取起点；showHidden 切换由开关自己触发重载。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 只在打开时取起点，其余依赖变化不应触发重载
  useEffect(() => {
    if (state.open) void load(state.start ?? fallback, showHidden);
  }, [state.open]);

  const close = () => set({ folderPicker: { open: false } });
  const target = selected ?? listing?.path ?? null;
  const confirm = () => {
    if (!target) return;
    state.onPick?.(target);
    close();
  };

  return (
    <Dialog open={state.open} onOpenChange={(open) => !open && close()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>选择工作目录</DialogTitle>
          <DialogDescription>浏览运行 moc 的机器上的目录。</DialogDescription>
        </DialogHeader>
        <div className="flex items-center gap-1">
          <IconButton
            label="主目录"
            disabled={!listing?.home}
            onClick={() => listing?.home && void load(listing.home, showHidden)}
          >
            <Home aria-hidden />
          </IconButton>
          <IconButton
            label="上一级"
            disabled={!listing?.parent}
            onClick={() => listing?.parent && void load(listing.parent, showHidden)}
          >
            <ArrowUp aria-hidden />
          </IconButton>
          <Input
            aria-label="路径"
            value={input}
            onChange={(event) => setInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && input.trim()) {
                event.preventDefault();
                void load(input.trim(), showHidden);
              }
            }}
            className="h-8 font-mono text-xs"
          />
        </div>
        {listing ? (
          <nav aria-label="路径层级" className="flex flex-wrap items-center gap-0.5 text-xs">
            {breadcrumbs(listing.path).map((crumb, index, all) => (
              <span key={crumb.path} className="flex items-center gap-0.5">
                <button
                  type="button"
                  onClick={() => void load(crumb.path, showHidden)}
                  className={cn(
                    "rounded px-1 hover:bg-accent",
                    index === all.length - 1 ? "font-medium" : "text-muted-foreground",
                  )}
                >
                  {crumb.name}
                </button>
                {index < all.length - 1 && crumb.name !== "/" ? (
                  <span className="text-muted-foreground">/</span>
                ) : null}
              </span>
            ))}
          </nav>
        ) : null}
        {error ? (
          <p
            role="alert"
            className="rounded-md border border-destructive/40 px-3 py-2 text-destructive text-xs"
          >
            {error}
          </p>
        ) : null}
        <div className="h-72 overflow-y-auto rounded-md border">
          {loading ? (
            <div className="flex h-full items-center justify-center text-muted-foreground">
              <Loader2 className="size-4 animate-spin" aria-hidden />
              <span className="sr-only">加载中</span>
            </div>
          ) : listing && listing.entries.length === 0 ? (
            <p className="p-6 text-center text-muted-foreground text-sm">没有子目录</p>
          ) : (
            <ul aria-label="子目录">
              {listing?.entries.map((entry) => (
                <li key={entry.path}>
                  <button
                    type="button"
                    aria-pressed={selected === entry.path}
                    onClick={() => setSelected(entry.path)}
                    onDoubleClick={() => void load(entry.path, showHidden)}
                    className={cn(
                      "flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm hover:bg-accent",
                      selected === entry.path && "bg-accent",
                    )}
                  >
                    {entry.git ? (
                      <FolderGit2 className="size-4 text-brand" aria-label="git 仓库" />
                    ) : (
                      <Folder className="size-4 text-muted-foreground" aria-hidden />
                    )}
                    <span className="truncate">{entry.name}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
        {listing?.truncated ? (
          <p className="text-muted-foreground text-xs">仅显示前 {FS_LIST_DIR_MAX_ENTRIES} 项</p>
        ) : null}
        <DialogFooter className="items-center sm:justify-between">
          <div className="flex items-center gap-2 text-sm">
            <Switch
              id="folder-picker-hidden"
              checked={showHidden}
              onCheckedChange={(checked) => {
                setShowHidden(checked);
                void load(listing?.path, checked);
              }}
            />
            <label htmlFor="folder-picker-hidden">显示隐藏目录</label>
          </div>
          <div className="flex gap-2">
            <Button variant="outline" onClick={close}>
              取消
            </Button>
            <Button disabled={!target} onClick={confirm}>
              选择{selected ? "所选目录" : "当前目录"}
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
