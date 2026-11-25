import { afterEach, expect, it, vi } from "vitest";
import type {
  ConfigGetResult,
  SessionInfo,
  SessionPermission,
  SkillsCompactResult,
  SkillsProgressParams,
} from "@/protocol/types";

const prefsMock = vi.hoisted(() => {
  type State = {
    workspaces: string[];
    permission: SessionPermission;
    set: (patch: Partial<{ workspaces: string[]; permission: SessionPermission }>) => void;
  };
  const listeners = new Set<(state: State, prev: State) => void>();
  const update = (patch: Partial<{ workspaces: string[]; permission: SessionPermission }>) => {
    const prev = { ...state };
    Object.assign(state, patch);
    for (const listener of listeners) listener(state, prev);
  };
  const state: State = {
    workspaces: [],
    permission: "full",
    set: vi.fn(update),
  };
  return { state, listeners, update };
});

vi.mock("@/protocol/client", () => ({
  rpc: { on: vi.fn(), onRequest: vi.fn(), call: vi.fn() },
  clientListeners: { ready: new Set(), status: new Set(), handshakeError: new Set() },
}));
vi.mock("./prefs", () => ({
  usePrefs: {
    getState: () => prefsMock.state,
    subscribe: (
      listener: (state: typeof prefsMock.state, prev: typeof prefsMock.state) => void,
    ) => {
      prefsMock.listeners.add(listener);
      return () => prefsMock.listeners.delete(listener);
    },
  },
}));

import { rpc } from "@/protocol/client";
import { useConnection } from "./connection";
import {
  compactSkills,
  createQuickSession,
  setSessionModel,
  setSessionPermission,
} from "./lifecycle";
import { useSessions } from "./sessions";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";
import { useUi } from "./ui";

const config: ConfigGetResult = {
  models: [],
  agents: [],
  cwd: "/tmp",
  default_model: "p/recent:high",
};

const previous: SessionInfo = {
  id: "old",
  title: "旧会话",
  model: "p/old:low",
  cwd: "/tmp",
  engine: "standard",
  autonomy: "default",
  permission: "full",
  plan_active: true,
  fast_mode: false,
  engine_locked: false,
  created_at_ms: 0,
  updated_at_ms: 0,
};

afterEach(() => {
  useSessions.getState().setDraftSession(null);
  useTabs.setState({ active: null, open: [] });
  useTranscript.setState({ bySession: {} });
  useConnection.setState({ config: null, status: "offline" });
  useUi.setState({ skillsWorking: null });
  prefsMock.state.workspaces = [];
  prefsMock.state.permission = "full";
  vi.clearAllMocks();
  vi.mocked(prefsMock.state.set).mockImplementation(prefsMock.update);
});

it("打开旧会话后新建草稿仍使用 core 的最近模型", () => {
  useConnection.setState({ config });
  useTranscript.getState().ensureView(previous);
  useTabs.getState().add(previous.id);
  const draft = createQuickSession();
  expect(draft.model).toBe("p/recent:high");
  expect(draft.plan_active).toBe(false);
  expect(useSessions.getState().draftSession?.model).toBeUndefined();
  useConnection.setState({ config: { ...config, default_model: "p/tui:off" } });
  expect(useTranscript.getState().bySession[draft.id].info.model).toBe("p/tui:off");
  expect(rpc.call).not.toHaveBeenCalled();
});

it("草稿主动选择模型经 core 保存，后续配置刷新不覆盖草稿选择", async () => {
  useConnection.setState({ config });
  createQuickSession();
  vi.mocked(rpc.call).mockImplementation(async (method) => {
    if (method === "config/manage") return { kind: "updated", data: "p/chosen:high" };
    return { ...config, default_model: "p/chosen:high" };
  });
  await setSessionModel("draft-session", "p/chosen");
  expect(rpc.call).toHaveBeenCalledWith("config/manage", {
    action: "select_model",
    cwd: "/tmp",
    selector: "p/chosen",
  });
  expect(useSessions.getState().draftSession?.model).toBe("p/chosen:high");
  useConnection.setState({ config: { ...config, default_model: "p/tui:off" } });
  expect(useTranscript.getState().bySession["draft-session"].info.model).toBe("p/chosen:high");
});

it("草稿模型保存失败不改变选择", async () => {
  useConnection.setState({ config });
  createQuickSession();
  vi.mocked(rpc.call).mockRejectedValueOnce(new Error("无效模型"));
  await expect(setSessionModel("draft-session", "p/missing")).rejects.toThrow("无效模型");
  expect(useSessions.getState().draftSession?.model).toBeUndefined();
  expect(useTranscript.getState().bySession["draft-session"].info.model).toBe(config.default_model);
});

it("草稿技能状态原位更新，结束后保留且后续失败不覆盖先前结果", async () => {
  useConnection.setState({ config });
  const draft = createQuickSession();
  const deferred = Promise.withResolvers<SkillsCompactResult>();
  const unsubscribe = vi.fn();
  vi.mocked(rpc.on).mockReturnValue(unsubscribe);
  vi.mocked(rpc.call).mockReturnValue(deferred.promise);
  const running = compactSkills(draft.id, "missing");
  const first = useTranscript.getState().bySession[draft.id].blocks[0];
  expect(useUi.getState().skillsWorking).toBe(draft.id);
  const callback = vi
    .mocked(rpc.on)
    .mock.calls.find(([name]) => name === "skills/progress")?.[1] as (
    p: SkillsProgressParams,
  ) => void;
  callback({ phase: "generating", completed: 7, total: 39, skipped: 2 });
  expect(useTranscript.getState().bySession[draft.id].blocks.map((b) => b.id)).toEqual([first.id]);
  deferred.resolve({
    compressed: 39,
    skipped: 2,
    removed: 0,
    tokens_before: 3920,
    tokens_after: 900,
    message: "3920 → 900 tokens",
    warnings: [],
  });
  await running;
  const finished = useTranscript.getState().bySession[draft.id].blocks[0];
  expect(finished).not.toBe(first);
  expect(finished).toMatchObject({
    id: first.id,
    kind: "status",
    localCommand: true,
    tone: "info",
  });
  expect(useUi.getState().skillsWorking).toBeNull();
  expect(unsubscribe).toHaveBeenCalledOnce();
  vi.mocked(rpc.call).mockRejectedValueOnce(new Error("upstream failure"));
  await compactSkills(draft.id, "force");
  const blocks = useTranscript.getState().bySession[draft.id].blocks;
  expect(blocks).toHaveLength(2);
  expect(blocks[0]).toEqual(finished);
  expect(blocks[1]).toMatchObject({ kind: "status", localCommand: true, tone: "error" });
  expect(rpc.call).not.toHaveBeenCalledWith("session/new", expect.anything());
  expect(rpc.call).not.toHaveBeenCalledWith("session/prompt", expect.anything());
  expect(useTabs.getState().active).toBe(draft.id);
});

it("新草稿权限默认 full，记住上次切换，且未改动草稿会同步延迟加载的偏好", async () => {
  useConnection.setState({ config });
  const initial = createQuickSession();
  expect(initial.permission).toBe("full");
  expect(useSessions.getState().draftSession?.permission).toBe("full");

  // 模拟 web.toml 在首屏草稿创建之后读回：未主动改过的草稿自动同步。
  prefsMock.update({ permission: "workspace" });
  expect(useSessions.getState().draftSession?.permission).toBe("workspace");
  expect(useTranscript.getState().bySession["draft-session"].info.permission).toBe("workspace");

  // 在草稿主动切换权限后，持久化到偏好且不再被外来偏好更新覆盖。
  await setSessionPermission("draft-session", "workspace_only");
  expect(prefsMock.state.set).toHaveBeenCalledWith({ permission: "workspace_only" });
  expect(useSessions.getState().draftSession?.permission).toBe("workspace_only");
  prefsMock.update({ permission: "full" });
  expect(useSessions.getState().draftSession?.permission).toBe("workspace_only");

  // 在真实会话主动切换权限成功后记住该选择；clearGrants 不改记忆。
  useTranscript.getState().ensureView(previous);
  vi.mocked(rpc.call).mockResolvedValueOnce({ ...previous, permission: "workspace" });
  await setSessionPermission(previous.id, "workspace");
  expect(prefsMock.state.permission).toBe("workspace");

  vi.mocked(prefsMock.state.set).mockClear();
  vi.mocked(rpc.call).mockResolvedValueOnce({ ...previous, permission: "workspace" });
  await setSessionPermission(previous.id, "workspace", true);
  expect(prefsMock.state.set).not.toHaveBeenCalled();

  // 下次新建草稿沿用上次记住的权限。
  const next = createQuickSession();
  expect(next.permission).toBe("workspace");
  expect(useSessions.getState().draftSession?.permission).toBe("workspace");
});
