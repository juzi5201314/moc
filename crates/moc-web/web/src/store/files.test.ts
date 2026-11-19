import { afterEach, expect, it, vi } from "vitest";
import type { FsReadFileResult } from "@/protocol/types";

vi.mock("@/protocol/client", () => ({
  rpc: { on: vi.fn(), onRequest: vi.fn(), call: vi.fn() },
  accessToken: () => "",
  clientListeners: { ready: new Set(), status: new Set(), handshakeError: new Set() },
}));

import { rpc } from "@/protocol/client";
import { noteExternalChange, reloadFromDisk, setDraft, useFiles } from "./files";

const PATH = "/w/a.txt";
const KEY = `fs:${PATH}`;

function read(content: string, modifiedMs: number): FsReadFileResult {
  return {
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

afterEach(() => {
  useFiles.setState({ bySession: {} });
  vi.resetAllMocks();
});

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
