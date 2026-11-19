import { afterEach, expect, it, vi } from "vitest";
import { rpc } from "@/protocol/client";
import type { FsReadFileResult } from "@/protocol/types";
import {
  commitNaming,
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
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
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
