import { afterEach, expect, it, vi } from "vitest";
import { rpc } from "@/protocol/client";
import type { FsListDirResult, FsReadFileResult, FsWriteFileResult } from "@/protocol/types";
import {
  askDelete,
  commitNaming,
  confirmDelete,
  loadDir,
  loadFileContent,
  noteExternalChange,
  type OpenFile,
  reloadFromDisk,
  renameFile,
  saveFile,
  setDraft,
  startNaming,
  targetKey,
  useFiles,
} from "./files";

vi.mock("@/protocol/client", () => ({
  rpc: { on: vi.fn(), onRequest: vi.fn(), call: vi.fn() },
  accessToken: () => "",
  clientListeners: { ready: new Set(), status: new Set(), handshakeError: new Set() },
}));

afterEach(() => {
  useFiles.setState({ bySession: {} });
  vi.resetAllMocks();
});

it("重命名文件和目录后所有会话的标签保留草稿并保存到新路径，失败或空名称不变", async () => {
  const root = { kind: "workspace", cwd: "/work" } as const;
  const tab = (path: string): OpenFile => ({
    target: { kind: "fs", path },
    preview: false,
    loading: false,
    encoding: "utf8",
    size: 3,
    modifiedMs: 42,
    truncated: false,
    content: "old",
    draft: "unsaved draft",
    line: 7,
    lineNonce: 2,
  });
  for (const [from, to, oldPath, newPath] of [
    ["/work/old.txt", "/work/new.txt", "/work/old.txt", "/work/new.txt"],
    ["/work/old", "/work/new", "/work/old/nested/file.txt", "/work/new/nested/file.txt"],
    [
      "/work/old",
      "/work/dest/moved",
      "/work/old/nested/file.txt",
      "/work/dest/moved/nested/file.txt",
    ],
  ]) {
    const original = tab(oldPath);
    const staleDestination = { ...tab(newPath), draft: "old" };
    const unrelated = tab(`${from}-other/file.txt`);
    const resource: OpenFile = { ...original, target: { kind: "resource", uri: oldPath } };
    useFiles.setState({ bySession: {} });
    for (const id of ["first", "second"]) {
      startNaming(id, { kind: "rename", parentKey: "/work", fromKey: from, name: "" });
      useFiles.setState((state) => ({
        bySession: {
          ...state.bySession,
          [id]: {
            ...state.bySession[id],
            tabs:
              id === "first"
                ? [staleDestination, original, unrelated, resource]
                : [
                    original,
                    { ...staleDestination, draft: "deleted file draft" },
                    unrelated,
                    resource,
                  ],
            active: targetKey(id === "first" ? original.target : staleDestination.target),
            closing: {
              key: targetKey(id === "first" ? original.target : staleDestination.target),
              title: "file.txt",
            },
          },
        },
      }));
    }
    vi.mocked(rpc.call).mockImplementation(async (method) => {
      if (method === "fs/rename") return { path: to };
      if (method === "fs/write_file") return { size: 13, modified_ms: 43 };
      return { entries: [] };
    });
    vi.mocked(rpc.call).mockClear();
    const beforeRejectedRename = useFiles.getState();
    await expect(renameFile(from, to)).rejects.toThrow(newPath);
    expect(rpc.call).not.toHaveBeenCalled();
    expect(useFiles.getState()).toBe(beforeRejectedRename);
    setDraft("second", `fs:${newPath}`, "old");
    if (to.includes("/dest/")) await renameFile(from, to);
    else await commitNaming("first", root, to.split("/").at(-1) ?? "");
    for (const id of ["first", "second"]) {
      const files = useFiles.getState().bySession[id];
      expect(files.tabs).toEqual([
        { ...original, target: { kind: "fs", path: newPath } },
        unrelated,
        resource,
      ]);
      expect(files.active).toBe(`fs:${newPath}`);
      expect(files.closing).toEqual(
        id === "first" ? { key: `fs:${newPath}`, title: "file.txt" } : undefined,
      );
      expect(await saveFile(id, files.active ?? "")).toBe(true);
      expect(rpc.call).toHaveBeenCalledWith("fs/write_file", {
        path: newPath,
        content: "unsaved draft",
        expected_modified_ms: 42,
      });
    }
    for (const name of ["", "rejected.txt"]) {
      startNaming("first", { kind: "rename", parentKey: "/work", fromKey: to, name });
      const before = useFiles.getState().bySession.first.tabs;
      vi.mocked(rpc.call).mockRejectedValue(new Error("rename rejected"));
      await commitNaming("first", root, name);
      expect(useFiles.getState().bySession.first.tabs).toBe(before);
    }
  }
});
const PATH = "/w/a.txt";
const KEY = `fs:${PATH}`;

function read(content: string, modifiedMs: number): FsReadFileResult {
  return {
    path: PATH,
    content,
    encoding: "utf8",
    size: content.length,
    modified_ms: modifiedMs,
    truncated: false,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function openTab(content: string, modifiedMs: number, draft = content) {
  useFiles.setState({
    bySession: {
      s: {
        root: "workspace",
        expanded: {},
        nodes: {},
        loading: {},
        tabs: [
          {
            target: { kind: "fs", path: PATH },
            preview: false,
            loading: false,
            encoding: "utf8",
            size: content.length,
            modifiedMs,
            truncated: false,
            content,
            draft,
            lineNonce: 0,
          },
        ],
        active: KEY,
        stackedView: "editor",
      },
    },
  });
}

const tab = () => useFiles.getState().bySession.s.tabs[0];
const flush = () => new Promise((done) => setTimeout(done, 0));

it("迟到的旧读取不覆盖较新的读取与之后输入的草稿", async () => {
  openTab("v0", 0);
  const older = deferred<FsReadFileResult>();
  const newer = deferred<FsReadFileResult>();
  vi.mocked(rpc.call).mockReturnValueOnce(older.promise).mockReturnValueOnce(newer.promise);

  const first = reloadFromDisk("s", KEY);
  const second = reloadFromDisk("s", KEY);
  newer.resolve(read("v2", 2));
  await second;
  setDraft("s", KEY, "v2 + 用户输入");
  older.resolve(read("v1", 1));
  await first;

  expect(tab()).toMatchObject({
    content: "v2",
    modifiedMs: 2,
    draft: "v2 + 用户输入",
    loading: false,
  });
});

it("重命名仍在加载的标签按新路径重读，不永远停在 loading", async () => {
  const loadingTab: OpenFile = {
    target: { kind: "fs", path: PATH },
    preview: false,
    loading: true,
    encoding: "utf8",
    size: 0,
    truncated: false,
    lineNonce: 0,
  };
  useFiles.setState({
    bySession: {
      s: {
        root: "workspace",
        expanded: {},
        nodes: {},
        loading: {},
        tabs: [loadingTab],
        active: KEY,
        stackedView: "editor",
      },
    },
  });
  const staleRead = deferred<FsReadFileResult>();
  vi.mocked(rpc.call).mockImplementation(async (method, params) => {
    if (method === "fs/rename") return { path: "/w/b.txt" };
    if (method === "fs/read_file") {
      return (params as { path: string }).path === PATH ? staleRead.promise : read("new", 9);
    }
    throw new Error(String(method));
  });
  void loadFileContent("s", loadingTab.target);
  await renameFile(PATH, "/w/b.txt");
  staleRead.resolve(read("stale", 1));
  await flush();
  expect(tab()).toMatchObject({
    loading: false,
    content: "new",
    draft: "new",
    modifiedMs: 9,
  });
  expect(tab().target).toMatchObject({ kind: "fs", path: "/w/b.txt" });
});

it("迟到的旧目录列举不覆盖较新的列举", async () => {
  useFiles.setState({
    bySession: {
      s: {
        root: "workspace",
        expanded: { "/w": true },
        nodes: {},
        loading: {},
        tabs: [],
        stackedView: "tree",
      },
    },
  });
  const entry = (name: string) => ({
    git: false,
    kind: "file" as const,
    name,
    path: `/w/${name}`,
  });
  const older = deferred<{ entries: ReturnType<typeof entry>[] }>();
  const newer = deferred<{ entries: ReturnType<typeof entry>[] }>();
  vi.mocked(rpc.call).mockReturnValueOnce(older.promise).mockReturnValueOnce(newer.promise);
  const root = { kind: "workspace", cwd: "/w" } as const;
  const first = loadDir("s", root, "/w");
  const second = loadDir("s", root, "/w");
  newer.resolve({ entries: [entry("new.txt")] });
  await second;
  older.resolve({ entries: [entry("stale.txt")] });
  await first;
  const files = useFiles.getState().bySession.s;
  expect(files.nodes["/w"].children?.map((child) => child.name)).toEqual(["new.txt"]);
  expect(files.loading["/w"]).toBe(false);
});

it("并发保存串行执行，第二次读取前移后的 mtime", async () => {
  openTab("v0", 0);
  useFiles.setState((state) => ({
    bySession: {
      s: {
        ...state.bySession.s,
        tabs: state.bySession.s.tabs.map((item) => ({
          ...item,
          draft: "edit",
          modifiedMs: 100,
        })),
      },
    },
  }));
  const first = deferred<FsWriteFileResult>();
  const second = deferred<FsWriteFileResult>();
  vi.mocked(rpc.call).mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  const save1 = saveFile("s", KEY);
  const save2 = saveFile("s", KEY);
  // 串行化后第二个请求要等第一次完成才发出。
  await flush();
  expect(rpc.call).toHaveBeenCalledTimes(1);
  first.resolve({ path: PATH, size: 1, modified_ms: 200 });
  await save1;
  await flush();
  expect(rpc.call).toHaveBeenCalledTimes(2);
  expect(vi.mocked(rpc.call).mock.calls[1][1]).toMatchObject({
    expected_modified_ms: 200,
  });
  second.resolve({ path: PATH, size: 1, modified_ms: 300 });
  expect(await save2).toBe(true);
  expect(tab().diskConflict).toBeUndefined();
  expect(tab().modifiedMs).toBe(300);
});

it("目录重命名迁移目录树缓存的 key", async () => {
  useFiles.setState({
    bySession: {
      s: {
        root: "workspace",
        expanded: { "/work/old": true, "/work/old/sub": true, "/work/other": true },
        nodes: {
          "/work/old": {
            key: "/work/old",
            name: "old",
            kind: "dir",
            children: [
              {
                key: "/work/old/sub",
                name: "sub",
                kind: "dir",
                children: [{ key: "/work/old/sub/a.txt", name: "a.txt", kind: "file" }],
              },
            ],
          },
          "/work/other": { key: "/work/other", name: "other", kind: "dir" },
        },
        loading: { "/work/old/sub": true },
        tabs: [],
        stackedView: "editor",
      },
    },
  });
  vi.mocked(rpc.call).mockImplementation(async (method) => {
    if (method === "fs/rename") return { path: "/work/new" };
    throw new Error(String(method));
  });
  await renameFile("/work/old", "/work/new");
  const files = useFiles.getState().bySession.s;
  expect(files.expanded).toEqual({ "/work/new": true, "/work/new/sub": true, "/work/other": true });
  expect(Object.keys(files.nodes).sort()).toEqual(["/work/new", "/work/other"]);
  expect(files.nodes["/work/new"].children?.[0].key).toBe("/work/new/sub");
  expect(files.nodes["/work/new"].children?.[0].children?.[0].key).toBe("/work/new/sub/a.txt");
  expect(files.loading).toEqual({ "/work/new/sub": true });
});

it("删除后迟到的在飞读取不清掉已删除标记", async () => {
  const loadingTab: OpenFile = {
    target: { kind: "fs", path: PATH },
    preview: false,
    loading: true,
    encoding: "utf8",
    size: 0,
    truncated: false,
    lineNonce: 0,
  };
  useFiles.setState({
    bySession: {
      s: {
        root: "session",
        expanded: {},
        nodes: {},
        loading: {},
        tabs: [loadingTab],
        active: KEY,
        stackedView: "editor",
      },
    },
  });
  const staleRead = deferred<FsReadFileResult>();
  vi.mocked(rpc.call).mockImplementation(async (method) => {
    if (method === "fs/read_file") return staleRead.promise;
    if (method === "fs/delete") return {};
    throw new Error(String(method));
  });
  void loadFileContent("s", loadingTab.target);
  askDelete("s", PATH, "a.txt");
  await confirmDelete("s", { kind: "session", sessionId: "s" });
  staleRead.resolve(read("stale", 1));
  await flush();
  expect(tab().loading).toBe(false);
  expect(tab().error).toBeTruthy();
  expect(tab().content).toBeUndefined();
});

it("删除目录后子标签标已删除并清掉子树缓存", async () => {
  const childTab: OpenFile = {
    target: { kind: "fs", path: "/work/dir/a.txt" },
    preview: false,
    loading: false,
    encoding: "utf8",
    size: 1,
    modifiedMs: 1,
    truncated: false,
    content: "x",
    lineNonce: 0,
  };
  useFiles.setState({
    bySession: {
      s: {
        root: "workspace",
        expanded: { "/work/dir": true, "/work/dir/sub": true, "/work/other": true },
        nodes: {
          "/work/dir": { key: "/work/dir", name: "dir", kind: "dir", children: [] },
          "/work/dir/sub": { key: "/work/dir/sub", name: "sub", kind: "dir" },
          "/work/other": { key: "/work/other", name: "other", kind: "dir" },
        },
        loading: { "/work/dir/sub": true },
        tabs: [childTab],
        active: "fs:/work/dir/a.txt",
        stackedView: "editor",
      },
    },
  });
  vi.mocked(rpc.call).mockImplementation(async (method) => {
    if (method === "fs/delete") return {};
    if (method === "fs/list_dir") return { entries: [] };
    throw new Error(String(method));
  });
  askDelete("s", "/work/dir", "dir");
  await confirmDelete("s", { kind: "workspace", cwd: "/work" });
  const files = useFiles.getState().bySession.s;
  expect(tab().error).toBeTruthy();
  expect(files.expanded).toEqual({ "/work/other": true });
  expect(Object.keys(files.nodes).sort()).toEqual(["/work", "/work/other"]);
  expect(files.loading).toEqual({ "/work": false });
});

it("删除目录时在飞列举的迟到响应不重建幽灵节点", async () => {
  // /work/dir/sub 只在 loading 里（在飞列举，还没落过 nodes）：
  // 删除 /work/dir 后旧响应不得把 sub 重新写回 nodes。
  useFiles.setState({
    bySession: {
      s: {
        root: "workspace",
        expanded: { "/work/dir": true, "/work/dir/sub": true },
        nodes: {},
        loading: { "/work/dir/sub": true },
        tabs: [],
        stackedView: "editor",
      },
    },
  });
  const staleList = deferred<FsListDirResult>();
  vi.mocked(rpc.call).mockImplementation(async (method, params) => {
    if (method === "fs/list_dir") {
      if ((params as { path?: string }).path === "/work/dir/sub") return staleList.promise;
      return { entries: [], path: "/work", truncated: false };
    }
    if (method === "fs/delete") return {};
    throw new Error(String(method));
  });
  askDelete("s", "/work/dir", "dir");
  await confirmDelete("s", { kind: "workspace", cwd: "/work" });
  staleList.resolve({
    entries: [{ path: "/work/dir/sub/x", name: "x", kind: "file", git: false }],
    path: "/work/dir/sub",
    truncated: false,
  });
  await flush();
  expect(useFiles.getState().bySession.s.nodes["/work/dir/sub"]).toBeUndefined();
});

it("已被较新读取取代的 mtime 探测不误报冲突", async () => {
  openTab("v0", 0);
  const probe = deferred<FsReadFileResult>();
  vi.mocked(rpc.call).mockReturnValueOnce(probe.promise).mockResolvedValueOnce(read("v2", 2));

  noteExternalChange("s");
  await reloadFromDisk("s", KEY);
  setDraft("s", KEY, "v2 + 用户输入");
  probe.resolve(read("", 1));
  await flush();

  expect(tab().diskConflict).toBeUndefined();
  expect(vi.mocked(rpc.call)).toHaveBeenCalledTimes(2);
});
