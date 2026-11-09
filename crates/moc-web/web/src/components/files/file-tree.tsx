// 文件树：按需加载、键盘导航、右键菜单、inline 命名、拖拽移动与外部拖入上传。
// 会话空间根固定 5 个命名空间，中文标签 + 行尾原文 URI。

import {
  ChevronRightIcon,
  FileCode2Icon,
  FileIcon,
  FileTextIcon,
  FolderIcon,
  FolderOpenIcon,
} from "lucide-react";
import { type DragEvent, type KeyboardEvent, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import { useComposer } from "@/store/composer";
import {
  activateTab,
  askDelete,
  cancelDelete,
  cancelNaming,
  cancelUpload,
  commitNaming,
  confirmDelete,
  confirmUpload,
  type FileRoot,
  loadDir,
  openFile,
  parentPath,
  setStackedView,
  startNaming,
  type TreeNode,
  targetKey,
  toggleDir,
  uploadFiles,
  useFiles,
} from "@/store/files";
import { openTerminal } from "@/store/terminal";
import { useTranscript } from "@/store/transcript";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

const NS_LABEL: Record<string, string> = {
  "local://": "计划",
  "artifact://jobs": "任务日志",
  "artifact://artifacts": "工具归档",
  "history://": "历史",
  "skill://": "技能",
};

const TEXT_EXTS: Record<string, typeof FileIcon> = {
  rs: FileCode2Icon,
  ts: FileCode2Icon,
  tsx: FileCode2Icon,
  js: FileCode2Icon,
  jsx: FileCode2Icon,
  py: FileCode2Icon,
  go: FileCode2Icon,
  md: FileTextIcon,
};

function fileIcon(name: string) {
  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  return TEXT_EXTS[ext] ?? FileIcon;
}

/** 每级 12px 缩进 + 极淡引导线。 */
function Row({
  node,
  depth,
  changed,
  sessionRoot,
  children,
}: {
  node: TreeNode;
  depth: number;
  changed: boolean;
  sessionRoot: boolean;
  children?: React.ReactNode;
}) {
  const files = useFiles((state) => state.bySession[sessionKey]);
  const open = Boolean(files?.expanded[node.key]);
  const naming = files?.naming;
  const isRenaming = naming?.kind === "rename" && naming.fromKey === node.key;
  const isDir = node.kind === "dir";
  const Icon = isDir ? (open ? FolderOpenIcon : FolderIcon) : fileIcon(node.name);
  const isNsRoot = sessionRoot && depth === 0;
  const label = isNsRoot ? (NS_LABEL[node.key] ?? node.name) : node.name;

  return (
    // biome-ignore lint/a11y/useFocusableInteractive: 键盘导航在树容器上集中处理，行不单独聚焦
    <div
      role="treeitem"
      aria-expanded={isDir ? open : undefined}
      className={cn(
        "group/row flex h-7 cursor-default items-center gap-1 pr-2 text-ui text-muted-foreground hover:text-foreground",
        "border-l border-transparent",
      )}
      style={{ paddingLeft: 8 + depth * 12 }}
      data-key={node.key}
    >
      {isDir ? (
        <ChevronRightIcon
          className={cn(
            "size-3.5 shrink-0 text-text-tertiary transition-transform",
            open && "rotate-90",
          )}
        />
      ) : (
        <span className="w-3.5 shrink-0" />
      )}
      <Icon className="size-3.5 shrink-0 text-text-tertiary" />
      {isRenaming ? (
        <NamingInput initial={node.name} nodeKey={node.key} />
      ) : (
        <span className="min-w-0 flex-1 truncate">{label}</span>
      )}
      {isNsRoot && (
        <span className="shrink-0 font-mono text-2xs text-text-tertiary">{node.key}</span>
      )}
      {node.description && !isNsRoot && (
        <span className="min-w-0 shrink truncate text-2xs text-text-tertiary">
          {node.description}
        </span>
      )}
      {changed && <span className="size-1.5 shrink-0 rounded-full bg-brand" />}
      {children}
    </div>
  );
}

let sessionKey = "";

/** inline 命名输入：Enter 提交，Esc 取消。 */
function NamingInput({ initial, nodeKey }: { initial: string; nodeKey: string }) {
  const ref = useRef<HTMLInputElement>(null);
  const naming = useFiles((state) => state.bySession[sessionKey]?.naming);
  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);
  return (
    <input
      ref={ref}
      defaultValue={initial}
      onKeyDown={(event) => {
        if (event.key === "Enter")
          void commitNaming(sessionKey, rootFor(sessionKey), event.currentTarget.value);
        if (event.key === "Escape") cancelNaming(sessionKey);
        event.stopPropagation();
      }}
      onBlur={(event) => {
        if (naming) void commitNaming(sessionKey, rootFor(sessionKey), event.currentTarget.value);
      }}
      className="min-w-0 flex-1 bg-transparent text-ui text-foreground outline-none"
      data-parent={nodeKey}
    />
  );
}

const roots: Record<string, FileRoot> = {};
function rootFor(sessionId: string): FileRoot {
  return roots[sessionId] ?? { kind: "workspace", cwd: "" };
}

export function FileTree({
  sessionId,
  workspaceRoot,
}: {
  sessionId: string;
  workspaceRoot: FileRoot | null;
}) {
  sessionKey = sessionId;
  const files = useFiles((state) => state.bySession[sessionId]);
  const root = files?.root ?? "workspace";
  const diffs = useTranscript((state) => state.bySession[sessionId]?.diffs);
  const changed = useMemo(() => {
    const set = new Set<string>();
    for (const record of diffs ?? []) for (const file of record.diff.files) set.add(file.path);
    return set;
  }, [diffs]);
  const composer = useComposer();

  const workspaceCwd = workspaceRoot?.kind === "workspace" ? workspaceRoot.cwd : "";
  const fileRoot: FileRoot =
    root === "workspace"
      ? { kind: "workspace", cwd: workspaceCwd }
      : { kind: "session", sessionId };
  roots[sessionId] = fileRoot;

  // 根节点进入即加载。
  const rootKey = root === "workspace" ? workspaceCwd : "";
  // biome-ignore lint/correctness/useExhaustiveDependencies: fileRoot 由 root/cwd 派生，重复依赖会重复拉取
  useEffect(() => {
    if (root === "session" || rootKey) void loadDir(sessionId, fileRoot, rootKey);
  }, [sessionId, root, rootKey]);

  const tree = files?.nodes[rootKey];
  const [selected, setSelected] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const dragOver = useRef<string | null>(null);

  /** 子项一律从 nodes 缓存按 key 取（loadDir 换新节点对象，旧 children 数组里是过期引用）。 */
  const kidsOf = (node: TreeNode) => files?.nodes[node.key]?.children;

  /** 可见行的扁平序列（展开的深度优先），供键盘导航。 */
  const flat = useMemo(() => {
    const rows: { node: TreeNode; depth: number }[] = [];
    const walk = (nodes: TreeNode[] | undefined, depth: number) => {
      for (const node of nodes ?? []) {
        rows.push({ node, depth });
        if (node.kind === "dir" && files?.expanded[node.key]) {
          walk(files?.nodes[node.key]?.children, depth + 1);
        }
      }
    };
    walk(tree?.children ?? (tree ? [] : undefined), 0);
    return rows;
  }, [tree, files?.expanded, files?.nodes]);

  const dirFor = (node: TreeNode) => (node.kind === "dir" ? node.key : parentPath(node.key));

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const index = flat.findIndex((row) => row.node.key === selected);
    const move = (next: number) => {
      const row = flat[next];
      if (row) setSelected(row.node.key);
    };
    switch (event.key) {
      case "ArrowDown":
        move(Math.min(flat.length - 1, index + 1));
        break;
      case "ArrowUp":
        move(Math.max(0, index - 1));
        break;
      case "ArrowRight": {
        const row = flat[index];
        if (row?.node.kind === "dir") toggleDir(sessionId, fileRoot, row.node, true);
        break;
      }
      case "ArrowLeft": {
        const row = flat[index];
        if (!row) break;
        if (row.node.kind === "dir" && files?.expanded[row.node.key]) {
          toggleDir(sessionId, fileRoot, row.node, false);
        } else if (row.depth > 0) {
          const parent = flat
            .slice(0, index)
            .reverse()
            .find((item) => item.depth === row.depth - 1);
          if (parent) setSelected(parent.node.key);
        }
        break;
      }
      case "Enter": {
        const row = flat[index];
        if (!row) break;
        if (row.node.kind === "dir") toggleDir(sessionId, fileRoot, row.node);
        else openFile(sessionId, targetOf(row.node), { pin: true });
        break;
      }
      case "F2": {
        const row = flat[index];
        if (row && root === "workspace") {
          startNaming(sessionId, {
            parentKey: dirFor(row.node),
            name: row.node.name,
            kind: "rename",
            fromKey: row.node.key,
          });
        }
        break;
      }
      case "Delete": {
        const row = flat[index];
        if (row && root === "workspace") askDelete(sessionId, row.node.key, row.node.name);
        break;
      }
      default:
        return;
    }
    event.preventDefault();
  };

  // 拖拽移动：树内行拖到目录 → fs/rename；外部文件拖入 → 上传。
  const onDragOver = (event: DragEvent, node: TreeNode) => {
    if (node.kind !== "dir") return;
    if (
      event.dataTransfer.types.includes("Files") ||
      event.dataTransfer.types.includes("moc/tree-key")
    ) {
      event.preventDefault();
      dragOver.current = node.key;
    }
  };
  const onDrop = (event: DragEvent, node: TreeNode) => {
    event.preventDefault();
    const key = event.dataTransfer.getData("moc/tree-key");
    if (key && node.kind === "dir" && root === "workspace") {
      const name = key.split("/").pop() ?? "";
      void commitMove(key, `${node.key}/${name}`);
      return;
    }
    if (root !== "workspace" || !workspaceRoot) return;
    void uploadDropped(
      node.kind === "dir" ? node.key : parentPath(node.key),
      event.dataTransfer.files,
    );
  };

  const commitMove = async (from: string, to: string) => {
    try {
      await rpc.call("fs/rename", { from, to });
      void loadDir(sessionId, fileRoot, parentPath(from));
      void loadDir(sessionId, fileRoot, parentPath(to));
    } catch (error) {
      toast.error(`移动失败：${errorText(error)}`);
    }
  };

  const uploadDropped = (dir: string, list: FileList) =>
    void uploadFiles(sessionId, fileRoot, dir, [...list]);

  const quote = (node: TreeNode) => {
    const target = targetOf(node);
    const draft = composer.drafts[sessionId]?.text ?? "";
    const ref =
      target.kind === "fs"
        ? `@${workspaceCwd && target.path.startsWith(workspaceCwd) ? target.path.slice(workspaceCwd.length + 1) : target.path} `
        : `@${target.uri} `;
    composer.setText(sessionId, draft + ref);
  };

  const copy = (text: string) => void navigator.clipboard.writeText(text);

  const menuFor = (node: TreeNode) => {
    const session = root === "session";
    if (session) {
      return (
        <ContextMenuContent>
          <ContextMenuItem onSelect={() => openFile(sessionId, targetOf(node), { pin: true })}>
            打开
          </ContextMenuItem>
          <ContextMenuItem onSelect={() => quote(node)}>引用到输入框</ContextMenuItem>
          <ContextMenuItem onSelect={() => copy(node.key)}>复制 URI</ContextMenuItem>
        </ContextMenuContent>
      );
    }
    return (
      <ContextMenuContent>
        <ContextMenuItem onSelect={() => openFile(sessionId, targetOf(node), { pin: true })}>
          打开
        </ContextMenuItem>
        <ContextMenuItem onSelect={() => void openTerminal(dirFor(node))}>
          在终端中打开
        </ContextMenuItem>
        <ContextMenuItem onSelect={() => quote(node)}>引用到输入框</ContextMenuItem>
        <ContextMenuSeparator />
        <ContextMenuItem onSelect={() => copy(node.key)}>复制路径</ContextMenuItem>
        <ContextMenuItem
          onSelect={() =>
            copy(
              workspaceCwd && node.key.startsWith(workspaceCwd)
                ? node.key.slice(workspaceCwd.length + 1)
                : node.key,
            )
          }
        >
          复制相对路径
        </ContextMenuItem>
        {node.kind === "file" && (
          <ContextMenuItem onSelect={() => window.open(downloadUrl(node.key), "_blank")}>
            下载
          </ContextMenuItem>
        )}
        <ContextMenuSeparator />
        <ContextMenuItem
          onSelect={() =>
            startNaming(sessionId, {
              parentKey: dirFor(node),
              name: node.name,
              kind: "rename",
              fromKey: node.key,
            })
          }
        >
          重命名
        </ContextMenuItem>
        {node.kind === "dir" && (
          <>
            <ContextMenuItem
              onSelect={() =>
                startNaming(sessionId, { parentKey: node.key, name: "", kind: "new-file" })
              }
            >
              新建文件
            </ContextMenuItem>
            <ContextMenuItem
              onSelect={() =>
                startNaming(sessionId, { parentKey: node.key, name: "", kind: "new-dir" })
              }
            >
              新建文件夹
            </ContextMenuItem>
          </>
        )}
        <ContextMenuSeparator />
        <ContextMenuItem
          className="text-destructive"
          onSelect={() => askDelete(sessionId, node.key, node.name)}
        >
          删除
        </ContextMenuItem>
      </ContextMenuContent>
    );
  };

  const renderRows = (nodes: TreeNode[] | undefined, depth: number) =>
    (nodes ?? []).map((node) => {
      const open = Boolean(files?.expanded[node.key]);
      const namingHere =
        files?.naming && files.naming.kind !== "rename" && files.naming.parentKey === node.key;
      return (
        <div key={node.key}>
          <ContextMenu>
            <ContextMenuTrigger asChild>
              {/* biome-ignore lint/a11y/noStaticElementInteractions lint/a11y/useKeyWithClickEvents: 行键盘导航在树容器集中处理 */}
              <div
                onClick={() => {
                  setSelected(node.key);
                  if (node.kind === "dir") toggleDir(sessionId, fileRoot, node);
                  else openFile(sessionId, targetOf(node));
                }}
                onDoubleClick={() => {
                  if (node.kind === "file") openFile(sessionId, targetOf(node), { pin: true });
                }}
                draggable={root === "workspace"}
                onDragStart={(event) => event.dataTransfer.setData("moc/tree-key", node.key)}
                onDragOver={(event) => onDragOver(event, node)}
                onDrop={(event) => onDrop(event, node)}
                className={cn(selected === node.key && "text-foreground")}
              >
                <Row
                  node={node}
                  depth={depth}
                  changed={changed.has(node.key)}
                  sessionRoot={root === "session"}
                />
              </div>
            </ContextMenuTrigger>
            {menuFor(node)}
          </ContextMenu>
          {namingHere && (
            <div
              className="flex h-7 items-center pr-2"
              style={{ paddingLeft: 8 + (depth + 1) * 12 }}
            >
              <span className="w-3.5 shrink-0" />
              <NamingInput initial="" nodeKey={node.key} />
            </div>
          )}
          {open && node.kind === "dir" && (
            <div>
              {files?.nodes[node.key]?.error && (
                <p className="px-8 text-xs text-destructive">{files?.nodes[node.key]?.error}</p>
              )}
              {renderRows(kidsOf(node), depth + 1)}
            </div>
          )}
        </div>
      );
    });

  return (
    <div
      ref={containerRef}
      role="tree"
      tabIndex={0}
      onKeyDown={onKeyDown}
      onDragOver={(event) => {
        if (event.dataTransfer.types.includes("Files")) event.preventDefault();
      }}
      onDrop={(event) => {
        if (root === "workspace" && workspaceRoot) {
          event.preventDefault();
          void uploadDropped(workspaceCwd, event.dataTransfer.files);
        }
      }}
      className="min-h-full py-1 outline-none"
    >
      {/* 层叠模式的树视图：有打开标签时在树上方给一条标签条，点回去 */}
      {files && files.stackedView === "tree" && files.tabs.length > 0 && (
        <div className="mb-1 flex items-center gap-0.5 overflow-x-auto px-1 [scrollbar-width:none]">
          {files.tabs.map((tab) => {
            const key = targetKey(tab.target);
            const dirty = tab.draft !== undefined && tab.draft !== tab.content;
            return (
              <button
                key={key}
                type="button"
                onClick={() => {
                  activateTab(sessionId, key);
                  setStackedView(sessionId, "editor");
                }}
                className="flex h-7 shrink-0 items-center gap-1.5 rounded-lg bg-muted px-2.5 text-ui text-muted-foreground hover:text-foreground"
              >
                <span className="max-w-32 truncate">
                  {(tab.target.kind === "fs" ? tab.target.path : tab.target.uri)
                    .split("/")
                    .filter(Boolean)
                    .at(-1)}
                </span>
                {dirty && <span className="size-1.5 rounded-full bg-brand" />}
              </button>
            );
          })}
        </div>
      )}
      {files?.loading[rootKey] && !tree ? (
        <p className="px-4 py-6 text-xs text-text-tertiary">读取目录…</p>
      ) : tree?.error ? (
        <p className="px-4 py-6 text-xs text-destructive">{tree.error}</p>
      ) : (
        <>
          {/* 根目录级新建/重命名：根本身不在 children 列表里，命名输入补在树顶 */}
          {files?.naming &&
            files.naming.kind !== "rename" &&
            files.naming.parentKey === rootKey && (
              <div className="flex h-7 items-center pr-2" style={{ paddingLeft: 8 }}>
                <span className="w-3.5 shrink-0" />
                <NamingInput initial="" nodeKey={rootKey} />
              </div>
            )}
          {renderRows(tree?.children ?? [], 0)}
        </>
      )}
      <DeleteDialog sessionId={sessionId} fileRoot={fileRoot} />
      <UploadDialog sessionId={sessionId} fileRoot={fileRoot} />
    </div>
  );
}

function targetOf(node: TreeNode) {
  return node.key.includes("://")
    ? { kind: "resource" as const, uri: node.key }
    : { kind: "fs" as const, path: node.key };
}

import { rawFileUrl } from "@/lib/web-api";

function downloadUrl(path: string): string {
  return rawFileUrl(path, true);
}

function UploadDialog({ sessionId, fileRoot }: { sessionId: string; fileRoot: FileRoot }) {
  const pending = useFiles((state) => state.bySession[sessionId]?.uploadConfirm);
  return (
    <AlertDialog
      open={pending !== undefined}
      onOpenChange={(open) => !open && cancelUpload(sessionId)}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>覆盖 {pending?.name}？</AlertDialogTitle>
          <AlertDialogDescription>目标位置已有同名文件，上传会替换它。</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction onClick={() => void confirmUpload(sessionId, fileRoot)}>
            覆盖上传
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

function DeleteDialog({ sessionId, fileRoot }: { sessionId: string; fileRoot: FileRoot }) {
  const deleting = useFiles((state) => state.bySession[sessionId]?.deleting);
  return (
    <AlertDialog
      open={deleting !== undefined}
      onOpenChange={(open) => !open && cancelDelete(sessionId)}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>删除 {deleting?.name}？</AlertDialogTitle>
          <AlertDialogDescription>会移到系统回收站，可以从回收站找回。</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction onClick={() => void confirmDelete(sessionId, fileRoot)}>
            移到回收站
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
