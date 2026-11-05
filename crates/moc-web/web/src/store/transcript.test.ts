import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { SessionInfo, SessionUpdate, SessionUpdateParams } from "@/protocol/types";

const handlers = vi.hoisted(() => new Map<string, (params: SessionUpdateParams) => void>());
vi.mock("@/protocol/client", () => ({
  rpc: {
    on: (name: string, handler: (params: SessionUpdateParams) => void) =>
      handlers.set(name, handler),
    onRequest: vi.fn(),
    call: vi.fn(),
  },
  clientListeners: { ready: new Set(), status: new Set(), handshakeError: new Set() },
}));

vi.mock("./prefs", () => ({ usePrefs: { getState: () => ({ workspaces: [] }) } }));

import { rpc } from "@/protocol/client";
import { closeSession, loadOlder, openSession, reloadReadOnly } from "./lifecycle";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";
import { useTurn, useTurnIds } from "./turns";

const info: SessionInfo = {
  id: "batch-test",
  title: "合帧测试",
  model: "m",
  permission: "workspace",
  engine: "standard",
  autonomy: "default",
  plan_active: false,
  fast_mode: false,
  engine_locked: false,
  cwd: "/tmp",
  created_at_ms: 0,
  updated_at_ms: 0,
};
function send(update: SessionUpdate): void {
  handlers.get("session/update")?.({ session_id: info.id, update });
}
function delta(text: string, type: "text" | "reasoning" = "text"): void {
  send({ type: "assistant_delta", prompt_id: "p", delta: { type, delta: text } });
}
function contents(): string[] {
  return useTranscript
    .getState()
    .bySession[info.id].blocks.flatMap((block) =>
      block.kind === "text" || block.kind === "reasoning" ? [block.text] : [],
    );
}

afterEach(() => {
  useTranscript.getState().interruptAll();
  useTranscript.setState({ bySession: {}, activity: {}, permissions: {}, asks: {} });
  vi.useRealTimers();
});

describe("流式合帧", () => {
  it("关闭仍在加载的标签后丢弃迟到历史并释放写租约", async () => {
    const pending = Promise.withResolvers<unknown>();
    const started = Promise.withResolvers<void>();
    const call = vi.mocked(rpc.call);
    call.mockImplementation(async (method) => {
      if (method === "session/load") return { session: info, entries: [], truncated: false };
      if (method === "session/history") {
        started.resolve();
        return pending.promise;
      }
      return { jobs: [], plugins: [], sessions: [] };
    });
    const loading = openSession(info.id);
    await started.promise;
    const closing = closeSession(info.id);
    pending.resolve({ type: "page", page: { session: info, entries: [], has_more: false } });
    await Promise.all([loading, closing]);
    expect(useTabs.getState().open).not.toContain(info.id);
    expect(useTranscript.getState().bySession[info.id]).toBeUndefined();
    expect(call).toHaveBeenCalledWith("session/unload", { session_id: info.id });

    for (const readOnly of [false, true]) {
      call.mockReset();
      const lease = Promise.withResolvers<unknown>();
      let loads = 0;
      call.mockImplementation(async (method) => {
        if (method === "session/load") {
          loads += 1;
          if (loads === 1) return lease.promise;
          return { session: info, entries: [], truncated: false };
        }
        if (method === "session/history") {
          return {
            type: "page",
            page: {
              session: { ...info, title: "重新打开" },
              entries: [],
              has_more: false,
              todos: [{ phase: "执行", items: [{ task: "保留任务", status: "pending" }] }],
              goal: { id: "g", objective: "保留目标", phase: "active" },
            },
          };
        }
        return { jobs: [], plugins: [], sessions: [] };
      });
      const first = openSession(info.id, { readOnly });
      const closing = closeSession(info.id);
      const reopened = openSession(info.id, { readOnly });
      expect(loads).toBe(1);
      lease.resolve({ session: info, entries: [], truncated: false });
      await Promise.all([first, closing, reopened]);
      expect(loads).toBe(2);
      expect(useTabs.getState().open).toContain(info.id);
      expect(useTranscript.getState().bySession[info.id].info.title).toBe("重新打开");
      expect(useTranscript.getState().bySession[info.id].todos[0].items[0].task).toBe("保留任务");
      expect(useTranscript.getState().bySession[info.id].goal?.objective).toBe("保留目标");
      expect(call).toHaveBeenCalledWith("session/load", {
        session_id: info.id,
        max_entries: 0,
        omit_auxiliary: true,
        read_only: readOnly,
      });
      expect(call.mock.calls.filter(([method]) => method === "session/unload")).toHaveLength(
        readOnly ? 0 : 1,
      );
      await closeSession(info.id);
    }

    for (const read of [loadOlder, reloadReadOnly]) {
      const response = Promise.withResolvers<unknown>();
      call.mockReset().mockReturnValue(response.promise);
      useTranscript.getState().ensureView(info);
      useTranscript.getState().mutate(info.id, (view) => {
        view.hasMore = true;
        view.beforeEntryId = "old";
      });
      const old = read(info.id);
      useTranscript.getState().drop(info.id);
      useTranscript.getState().ensureView({ ...info, title: "新视图" });
      useTranscript.getState().localUser(info.id, "新正文", []);
      useTranscript.getState().mutate(info.id, (view) => {
        view.beforeEntryId = "new";
        view.loadingOlder = true;
      });
      response.resolve({ type: "page", page: { session: info, entries: [], has_more: false } });
      await old;
      const view = useTranscript.getState().bySession[info.id];
      expect(view.info.title).toBe("新视图");
      expect(view.blocks).toHaveLength(1);
      expect(view.beforeEntryId).toBe("new");
      expect(view.loadingOlder).toBe(true);
    }
  });
  it("按帧提交同通道增量，控制事件和断线前完整落定，不复活已关闭视图", () => {
    vi.useFakeTimers();
    useTranscript.getState().ensureView(info);
    useTranscript.getState().localUser(info.id, "历史提问", []);
    useTranscript.getState().localUser(info.id, "当前提问", []);
    send({ type: "prompt_started", prompt_id: "p", model: "m", at_ms: Date.now() });
    const listRender = vi.fn();
    const pastRender = vi.fn();
    const liveRender = vi.fn();
    const ids = renderHook(() => {
      listRender();
      return useTurnIds(info.id);
    });
    const [pastId, liveId] = ids.result.current;
    const past = renderHook(() => {
      pastRender();
      return useTurn(info.id, pastId);
    });
    const live = renderHook(() => {
      liveRender();
      return useTurn(info.id, liveId);
    });
    const counts = [
      listRender.mock.calls.length,
      pastRender.mock.calls.length,
      liveRender.mock.calls.length,
    ];
    const changes = vi.fn();
    const unsubscribe = useTranscript.subscribe(changes);
    try {
      delta("一");
      delta("二");
      expect(changes).not.toHaveBeenCalled();
      act(() => vi.advanceTimersByTime(16));
      expect(changes).toHaveBeenCalledTimes(1);
      expect(contents()).toEqual(["一二"]);
      expect(listRender).toHaveBeenCalledTimes(counts[0]);
      expect(pastRender).toHaveBeenCalledTimes(counts[1]);
      expect(liveRender.mock.calls.length).toBeGreaterThan(counts[2]);
      act(() => {
        delta("思考", "reasoning");
        delta("三");
      });
      expect(contents()).toEqual(["一二", "思考"]);
      act(() => send({ type: "generation_started", prompt_id: "p" }));
      expect(contents().join("")).toContain("三");
      delta("未完");
      act(() => useTranscript.getState().interruptAll());
      expect(contents().join("")).toContain("未完");
      delta("关闭前");
      act(() => {
        useTranscript.getState().drop(info.id);
        vi.runAllTimers();
      });
      expect(useTranscript.getState().bySession[info.id]).toBeUndefined();
    } finally {
      ids.unmount();
      past.unmount();
      live.unmount();
      unsubscribe();
    }
  });
});
