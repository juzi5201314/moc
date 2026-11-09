// 文件分页的状态：根（工作区/会话空间）、按需加载的目录树、打开的文件标签。
//
// 树节点只在展开时才拉取；标签内容（utf8 正文与 mtime）按会话记忆在内存里，
// 会话空间整体只读。刷新信号统一 debounce 300ms 后重拉已展开目录。

import { toast } from "sonner";
import { create } from "zustand";
import type { FileTarget } from "@/lib/file-target";
import { uploadFile } from "@/lib/web-api";
import { rpc } from "@/protocol/client";
import { ErrorCode } from "@/protocol/errors";
import type {
  FsCreateFileResult,
  FsDirEntry,
  FsListDirResult,
  FsReadFileResult,
  FsRenameResult,
  FsWriteFileResult,
  ResourceListResult,
  ResourceReadResult,
  SessionUpdateParams,
} from "@/protocol/types";
import { useLayout } from "./layout";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";

/** 树根的两种来源。 */
export type FileRoot = { kind: "workspace"; cwd: string } | { kind: "session"; sessionId: string };

/** 树节点：key 是 fs 绝对路径或资源 URI。 */
export type TreeNode = {
  key: string;
  name: string;
  kind: "dir" | "file";
  /** 目录节点已加载过的子项；未加载为 undefined。 */
  children?: TreeNode[];
  /** 本次列举失败时的错误文本。 */
  error?: string;
  description?: string | null;
};

/** 已打开文件的查看/编辑状态（正文只记 utf8；二进制只记元信息）。 */
export type OpenFile = {
  target: FileTarget;
  /** 预览标签（斜体、会被替换）；pin 后固定。 */
  preview: boolean;
  loading: boolean;
  error?: string;
  encoding: "utf8" | "binary";
  /** 服务端返回的原始字节数与 mtime。 */
  size: number;
  modifiedMs?: number | null;
  truncated: boolean;
  /** utf8 正文（已加载才有）。 */
  content?: string;
  /** 编辑中的草稿；与 content 不等即未保存。 */
  draft?: string;
  /** 磁盘被外部修改的提示：dirty 时提示冲突，clean 时静默重载。 */
  diskConflict?: boolean;
  /** 请求跳转的行号。 */
  line?: number;
  /** 行号跳转的触发计数：同一文件再带 line 打开时自增，让编辑器重滚。 */
  lineNonce: number;
};

export function targetKey(target: FileTarget): string {
  return target.kind === "fs" ? `fs:${target.path}` : `res:${target.uri}`;
}

export type SessionFiles = {
  root: "workspace" | "session";
  /** 展开的目录 key 集合（fs 路径或资源 URI）。 */
  expanded: Record<string, boolean>;
  /** key → 节点缓存；根目录 key 是 cwd 或 ""。 */
  nodes: Record<string, TreeNode>;
  /** 各目录是否正在加载。 */
  loading: Record<string, boolean>;
  /** 打开的标签（按顺序）。 */
  tabs: OpenFile[];
  /** 当前激活的标签 key。 */
  active?: string;
  /** 窄面板层叠模式的当前视图；有标签打开时为 editor。 */
  stackedView: "tree" | "editor";
  /** 正在 inline 输入名字的节点（新建/重命名）。 */
  naming?: {
    parentKey: string;
    name: string;
    kind: "new-file" | "new-dir" | "rename";
    fromKey?: string;
  };
  /** 正在删除确认的目标。 */
  deleting?: { key: string; name: string };
  /** 关闭未保存标签的确认框。 */
  closing?: { key: string; title: string };
  /** 上传遇到同名文件的覆盖确认。 */
  uploadConfirm?: { dir: string; files: File[]; index: number; name: string };
};

export type FilesState = {
  /** 每个会话一份；key=sessionId。 */
  bySession: Record<string, SessionFiles>;
};

const EMPTY_SESSION: SessionFiles = {
  root: "workspace",
  expanded: {},
  nodes: {},
  loading: {},
  tabs: [],
  stackedView: "tree",
};

function session(state: FilesState, sessionId: string): SessionFiles {
  return state.bySession[sessionId] ?? EMPTY_SESSION;
}

export const useFiles = create<FilesState>()(() => ({ bySession: {} }));

function patch(sessionId: string, recipe: (files: SessionFiles) => SessionFiles): void {
  useFiles.setState((state) => ({
    bySession: { ...state.bySession, [sessionId]: recipe(session(state, sessionId)) },
  }));
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function setFileRoot(sessionId: string, root: "workspace" | "session"): void {
  patch(sessionId, (files) => ({ ...files, root }));
}

export function setStackedView(sessionId: string, view: "tree" | "editor"): void {
  patch(sessionId, (files) => ({ ...files, stackedView: view }));
}

export function patchFileTab(sessionId: string, key: string, update: Partial<OpenFile>): void {
  patch(sessionId, (files) => ({
    ...files,
    tabs: files.tabs.map((tab) => (targetKey(tab.target) === key ? { ...tab, ...update } : tab)),
  }));
}

/** 展开/收起一个目录节点；展开时未加载则拉取子项。 */
export function toggleDir(sessionId: string, root: FileRoot, node: TreeNode, open?: boolean): void {
  const files = session(useFiles.getState(), sessionId);
  const expanded = { ...files.expanded };
  expanded[node.key] = open ?? !expanded[node.key];
  patch(sessionId, (current) => ({ ...current, expanded }));
  // 行内 node.children 可能是过期引用，以 nodes 缓存为准。
  if (expanded[node.key] && !files.nodes[node.key]?.children) {
    void loadDir(sessionId, root, node.key);
  }
}

function nodeFromFs(entry: FsDirEntry): TreeNode {
  return { key: entry.path, name: entry.name, kind: entry.kind };
}

function nodeFromResource(entry: ResourceListResult["entries"][number]): TreeNode {
  return {
    key: entry.uri,
    name: entry.name,
    kind: entry.kind,
    description: entry.description,
  };
}

/** 拉取一个目录的子项并更新节点缓存。 */
export async function loadDir(sessionId: string, root: FileRoot, key: string): Promise<TreeNode[]> {
  patch(sessionId, (files) => ({ ...files, loading: { ...files.loading, [key]: true } }));
  try {
    let children: TreeNode[];
    if (root.kind === "workspace") {
      const result = await rpc.call<FsListDirResult>("fs/list_dir", {
        path: key,
        include_files: true,
        show_hidden: useLayout.getState().filesShowHidden,
      });
      children = result.entries.map(nodeFromFs);
    } else {
      const result = await rpc.call<ResourceListResult>("resource/list", {
        session_id: sessionId,
        uri: key,
      });
      children = result.entries.map(nodeFromResource);
    }
    patch(sessionId, (files) => {
      const nodes = { ...files.nodes };
      nodes[key] = {
        ...(nodes[key] ?? { key, name: key, kind: "dir" }),
        children,
        error: undefined,
      };
      return { ...files, nodes, loading: { ...files.loading, [key]: false } };
    });
    return children;
  } catch (error) {
    const message = errorText(error);
    patch(sessionId, (files) => {
      const nodes = { ...files.nodes };
      nodes[key] = { ...(nodes[key] ?? { key, name: key, kind: "dir" }), error: message };
      return { ...files, nodes, loading: { ...files.loading, [key]: false } };
    });
    return [];
  }
}

/** 已展开目录全部重拉（自动刷新用，调用方负责 debounce）。 */
export function refreshExpanded(sessionId: string, root: FileRoot): void {
  const files = session(useFiles.getState(), sessionId);
  for (const key of Object.keys(files.expanded)) {
    if (files.expanded[key]) void loadDir(sessionId, root, key);
  }
}

// ---------- 标签与文件操作 ----------

/** 读一份文件/资源内容并填进标签。 */
export async function loadFileContent(sessionId: string, target: FileTarget): Promise<void> {
  const key = targetKey(target);
  try {
    const result =
      target.kind === "fs"
        ? await rpc.call<FsReadFileResult>("fs/read_file", { path: target.path })
        : await rpc.call<ResourceReadResult>("resource/read", {
            session_id: sessionId,
            uri: target.uri,
          });
    patchFileTab(sessionId, key, {
      loading: false,
      error: undefined,
      encoding: result.encoding,
      size: result.size,
      modifiedMs: result.modified_ms,
      truncated: result.truncated,
      content: result.content ?? undefined,
      draft: result.content ?? undefined,
    });
  } catch (error) {
    patchFileTab(sessionId, key, { loading: false, error: errorText(error) });
  }
}

/**
 * 打开文件的统一入口：开面板 → 切 files 分页 → 切根 → 展开树 → 开标签。
 * `pin` 打开为固定标签，否则是预览标签（再开别的会被替换）。
 */
export function openFile(
  sessionId: string,
  target: FileTarget,
  options: { line?: number; pin?: boolean } = {},
): void {
  const layout = useLayout.getState();
  if (!(layout.rightbarOpen && layout.inspectorTab === "files")) {
    layout.openInspector("files");
  }
  const root: "workspace" | "session" = target.kind === "fs" ? "workspace" : "session";

  const key = targetKey(target);
  const files = session(useFiles.getState(), sessionId);
  const existing = files.tabs.find((tab) => targetKey(tab.target) === key);
  if (existing) {
    patchFileTab(sessionId, key, {
      preview: options.pin ? false : existing.preview,
      line: options.line ?? existing.line,
      lineNonce: options.line !== undefined ? existing.lineNonce + 1 : existing.lineNonce,
      diskConflict: undefined,
    });
  } else {
    // 预览标签被替换：同一个 preview 槽位只留一个。
    const tab: OpenFile = {
      target,
      preview: !options.pin,
      loading: true,
      encoding: "utf8",
      size: 0,
      truncated: false,
      line: options.line,
      lineNonce: options.line !== undefined ? 1 : 0,
    };
    patch(sessionId, (current) => ({
      ...current,
      tabs: [...current.tabs.filter((item) => !item.preview), tab],
      active: key,
    }));
    void loadFileContent(sessionId, target);
  }
  // 层叠模式下开文件即切到编辑视图。
  patch(sessionId, (current) => ({ ...current, root, active: key, stackedView: "editor" }));
  // 树里展开到该文件所在的各级目录。
  if (target.kind === "fs") {
    const cwd = useTranscript.getState().bySession[sessionId]?.info.cwd ?? "";
    expandTo(sessionId, { kind: "workspace", cwd }, target.path);
  } else {
    expandTo(sessionId, { kind: "session", sessionId }, target.uri);
  }
}

/** 沿 key 的各级父目录依次展开并加载。 */
export function expandTo(sessionId: string, root: FileRoot, key: string): void {
  const files = session(useFiles.getState(), sessionId);
  const expanded = { ...files.expanded };
  const parents: string[] = [];
  if (root.kind === "workspace") {
    let dir = parentPath(key);
    const base = root.cwd;
    while (dir && dir !== base && dir.startsWith(base)) {
      parents.unshift(dir);
      dir = parentPath(dir);
    }
    if (base) parents.unshift(base);
  } else {
    // 资源 URI：local://a/b → local://a、local://
    const match = /^([a-z]+:\/\/)(.*)$/.exec(key);
    if (!match) return;
    const segments = match[2].split("/").filter(Boolean);
    let uri = match[1];
    parents.push(uri);
    for (let index = 0; index < segments.length - 1; index++) {
      uri = `${uri}${uri.endsWith("/") ? "" : "/"}${segments[index]}`;
      parents.push(uri);
    }
  }
  for (const parent of parents) expanded[parent] = true;
  patch(sessionId, (files2) => ({ ...files2, expanded }));
  for (const parent of parents) {
    if (!files.nodes[parent]?.children) void loadDir(sessionId, root, parent);
  }
}

export function parentPath(path: string): string {
  const index = path.replace(/[\\/]+$/, "").lastIndexOf("/");
  return index > 0 ? path.slice(0, index) : index === 0 ? "/" : "";
}

export function activateTab(sessionId: string, key: string): void {
  patch(sessionId, (files) => ({ ...files, active: key, stackedView: "editor" }));
}

/** 双击或开始编辑 → 预览标签转正。 */
export function pinTab(sessionId: string, key: string): void {
  patchFileTab(sessionId, key, { preview: false });
}

export function closeTab(sessionId: string, key: string, force = false): void {
  const files = session(useFiles.getState(), sessionId);
  const tab = files.tabs.find((item) => targetKey(item.target) === key);
  if (!tab) return;
  if (!force && tab.draft !== undefined && tab.draft !== tab.content) {
    patch(sessionId, (current) => ({
      ...current,
      closing: { key, title: titleOf(tab.target) },
    }));
    return;
  }
  patch(sessionId, (current) => {
    const tabs = current.tabs.filter((item) => targetKey(item.target) !== key);
    const last = tabs.at(-1);
    const active =
      current.active === key ? (last ? targetKey(last.target) : undefined) : current.active;
    return { ...current, tabs, active, stackedView: tabs.length ? "editor" : "tree" };
  });
}

function titleOf(target: FileTarget): string {
  const raw = target.kind === "fs" ? target.path : target.uri;
  return raw.split("/").filter(Boolean).at(-1) ?? raw;
}

/** 编辑草稿；首次改动把预览标签转正。 */
export function setDraft(sessionId: string, key: string, draft: string): void {
  patch(sessionId, (current) => ({
    ...current,
    tabs: current.tabs.map((item) =>
      targetKey(item.target) === key ? { ...item, draft, preview: false } : item,
    ),
  }));
}

/** 保存：mtime 乐观锁；-32016 时打 diskConflict 标记交给 UI 出 banner。 */
export async function saveFile(
  sessionId: string,
  key: string,
  overwrite = false,
): Promise<boolean> {
  const files = session(useFiles.getState(), sessionId);
  const tab = files.tabs.find((item) => targetKey(item.target) === key);
  if (tab?.target.kind !== "fs") return false;
  try {
    const result = await rpc.call<FsWriteFileResult>("fs/write_file", {
      path: tab.target.path,
      content: tab.draft ?? "",
      expected_modified_ms: overwrite ? undefined : tab.modifiedMs,
    });
    patchFileTab(sessionId, key, {
      content: tab.draft,
      size: result.size,
      modifiedMs: result.modified_ms,
      diskConflict: undefined,
    });
    return true;
  } catch (error) {
    const conflict =
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      (error as { code?: number }).code === ErrorCode.FileConflict;
    if (conflict) {
      patchFileTab(sessionId, key, { diskConflict: true });
    } else {
      toast.error(`保存失败：${errorText(error)}`);
    }
    return false;
  }
}

/** 放弃本地草稿，读磁盘版本。 */
export async function reloadFromDisk(sessionId: string, key: string): Promise<void> {
  const files = session(useFiles.getState(), sessionId);
  const tab = files.tabs.find((item) => targetKey(item.target) === key);
  if (!tab) return;
  patchFileTab(sessionId, key, { loading: true, diskConflict: undefined });
  await loadFileContent(sessionId, tab.target);
}

/** 自动刷新信号到来：干净文件 mtime 变了静默重载；脏文件打冲突标记。 */
export function noteExternalChange(sessionId: string): void {
  for (const tab of session(useFiles.getState(), sessionId).tabs) {
    if (tab.target.kind === "resource") continue;
    void rpc
      .call<FsReadFileResult>("fs/read_file", { path: tab.target.path, max_bytes: 1 })
      .then((result) => {
        const current = session(useFiles.getState(), sessionId);
        const now = current.tabs.find((item) => targetKey(item.target) === targetKey(tab.target));
        if (!now || result.modified_ms === now.modifiedMs) return;
        const dirty = now.draft !== undefined && now.draft !== now.content;
        if (dirty) patchFileTab(sessionId, targetKey(tab.target), { diskConflict: true });
        else void reloadFromDisk(sessionId, targetKey(tab.target));
      })
      .catch(() => {});
  }
}

// ---------- 树操作 ----------

export function startNaming(sessionId: string, naming: SessionFiles["naming"]): void {
  patch(sessionId, (files) => ({ ...files, naming }));
}

export function cancelNaming(sessionId: string): void {
  patch(sessionId, (files) => ({ ...files, naming: undefined }));
}

/** 提交 inline 命名：新建文件/目录或重命名。 */
export async function commitNaming(sessionId: string, root: FileRoot, name: string): Promise<void> {
  const files = session(useFiles.getState(), sessionId);
  const naming = files.naming;
  if (!naming || root.kind !== "workspace") return;
  const trimmed = name.trim();
  if (!trimmed) {
    cancelNaming(sessionId);
    return;
  }
  try {
    if (naming.kind === "rename" && naming.fromKey) {
      const to = `${parentPath(naming.fromKey)}/${trimmed}`;
      await rpc.call<FsRenameResult>("fs/rename", { from: naming.fromKey, to });
      void loadDir(sessionId, root, parentPath(naming.fromKey));
    } else if (naming.kind === "new-dir") {
      await rpc.call("fs/create_dir", { parent: naming.parentKey, name: trimmed });
      void loadDir(sessionId, root, naming.parentKey);
    } else {
      const result = await rpc.call<FsCreateFileResult>("fs/create_file", {
        parent: naming.parentKey,
        name: trimmed,
      });
      await loadDir(sessionId, root, naming.parentKey);
      openFile(sessionId, { kind: "fs", path: result.path });
    }
  } catch (error) {
    toast.error(`操作失败：${errorText(error)}`);
  }
  cancelNaming(sessionId);
}

export function askDelete(sessionId: string, key: string, name: string): void {
  patch(sessionId, (files) => ({ ...files, deleting: { key, name } }));
}

export function cancelDelete(sessionId: string): void {
  patch(sessionId, (files) => ({ ...files, deleting: undefined }));
}

/** 删除：移入系统回收站。 */
export async function confirmDelete(sessionId: string, root: FileRoot): Promise<void> {
  const files = session(useFiles.getState(), sessionId);
  const deleting = files.deleting;
  cancelDelete(sessionId);
  if (!deleting) return;
  try {
    await rpc.call("fs/delete", { path: deleting.key });
    if (root.kind === "workspace") void loadDir(sessionId, root, parentPath(deleting.key));
    // 已打开的标签标为已删除状态（留标签，展示错误）。
    patchFileTab(sessionId, `fs:${deleting.key}`, { error: "文件已删除" });
  } catch (error) {
    toast.error(`删除失败：${errorText(error)}`);
  }
}

// ---------- 上传（409 覆盖确认走 AlertDialog） ----------

/** 往 dir 目录上传文件；同名文件转入 uploadConfirm 逐个确认。 */
export async function uploadFiles(
  sessionId: string,
  root: FileRoot,
  dir: string,
  files: File[],
): Promise<void> {
  for (const file of files) {
    try {
      const ok = await uploadFile(`${dir}/${file.name}`, file);
      if (!ok) {
        patch(sessionId, (current) => ({
          ...current,
          uploadConfirm: { dir, files, index: files.indexOf(file), name: file.name },
        }));
        return;
      }
    } catch (error) {
      toast.error(`上传 ${file.name} 失败：${errorText(error)}`);
    }
  }
  if (root.kind === "workspace") void loadDir(sessionId, root, dir);
}

/** 覆盖确认后继续：当前文件 overwrite 重传，然后接着传队列剩下的。 */
export async function confirmUpload(sessionId: string, root: FileRoot): Promise<void> {
  const pending = session(useFiles.getState(), sessionId).uploadConfirm;
  cancelUpload(sessionId);
  if (!pending) return;
  const file = pending.files[pending.index];
  try {
    await uploadFile(`${pending.dir}/${file.name}`, file, true);
  } catch (error) {
    toast.error(`上传 ${file.name} 失败：${errorText(error)}`);
  }
  await uploadFiles(sessionId, root, pending.dir, pending.files.slice(pending.index + 1));
}

export function cancelUpload(sessionId: string): void {
  patch(sessionId, (files) => ({ ...files, uploadConfirm: undefined }));
}

// ---------- 刷新信号：本模块自订阅，不经 transcript 中转 ----------

const refreshTimers = new Map<string, ReturnType<typeof setTimeout>>();

/** 300ms debounce 后重拉已展开目录并检查打开文件的 mtime。 */
function noteRefresh(sessionId: string): void {
  const pending = refreshTimers.get(sessionId);
  if (pending) clearTimeout(pending);
  refreshTimers.set(
    sessionId,
    setTimeout(() => {
      refreshTimers.delete(sessionId);
      const state = session(useFiles.getState(), sessionId);
      const cwd = useTranscript.getState().bySession[sessionId]?.info.cwd;
      if (!state || !cwd) return;
      const root: FileRoot =
        state.root === "workspace" ? { kind: "workspace", cwd } : { kind: "session", sessionId };
      refreshExpanded(sessionId, root);
      noteExternalChange(sessionId);
    }, 300),
  );
}

rpc.on("session/update", ({ session_id: sessionId, update }: SessionUpdateParams) => {
  if (
    update.type === "workspace_changed" ||
    update.type === "job_finished" ||
    update.type === "prompt_finished" ||
    update.type === "plan_submitted" ||
    update.type === "plan_updated"
  ) {
    noteRefresh(sessionId);
  }
});

// 窗口重新获得焦点也算一次刷新信号（外面可能用别的工具改过文件）。
window.addEventListener("focus", () => {
  for (const sessionId of Object.keys(useFiles.getState().bySession)) {
    noteRefresh(sessionId);
  }
});

// Ctrl/Cmd+S 全局保存：只在文件分页有激活的、可写且有未保存修改的 fs 文件时接管。
window.addEventListener("keydown", (event) => {
  if (!(event.ctrlKey || event.metaKey) || event.key !== "s") return;
  const layout = useLayout.getState();
  if (!layout.rightbarOpen || layout.inspectorTab !== "files") return;
  const sessionId = useTabs.getState().active;
  if (!sessionId) return;
  const files = session(useFiles.getState(), sessionId);
  const key = files.active;
  if (!key) return;
  const tab = files.tabs.find((item) => targetKey(item.target) === key);
  if (tab?.target.kind !== "fs") return;
  if (tab.draft === undefined || tab.draft === tab.content) return;
  event.preventDefault();
  void saveFile(sessionId, key);
});
