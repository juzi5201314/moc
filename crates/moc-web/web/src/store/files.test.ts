import { afterEach, expect, it, vi } from "vitest";
import { rpc } from "@/protocol/client";
import {
  commitNaming,
  type OpenFile,
  renameFile,
  saveFile,
  startNaming,
  targetKey,
  useFiles,
} from "./files";

vi.mock("@/protocol/client", () => ({
  rpc: { on: vi.fn(), onRequest: vi.fn(), call: vi.fn() },
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
            tabs: [original, unrelated, resource],
            active: targetKey(original.target),
          },
        },
      }));
    }
    vi.mocked(rpc.call).mockImplementation(async (method) => {
      if (method === "fs/rename") return { path: to };
      if (method === "fs/write_file") return { size: 13, modified_ms: 43 };
      return { entries: [] };
    });
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
