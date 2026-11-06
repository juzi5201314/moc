import { afterEach, expect, it, vi } from "vitest";
import type { ConfigGetResult, SessionInfo } from "@/protocol/types";

vi.mock("@/protocol/client", () => ({
  rpc: { on: vi.fn(), onRequest: vi.fn(), call: vi.fn() },
  clientListeners: { ready: new Set(), status: new Set(), handshakeError: new Set() },
}));
vi.mock("./prefs", () => ({ usePrefs: { getState: () => ({ workspaces: [] }) } }));

import { rpc } from "@/protocol/client";
import { useConnection } from "./connection";
import { createQuickSession, setSessionModel } from "./lifecycle";
import { useSessions } from "./sessions";
import { useTabs } from "./tabs";
import { useTranscript } from "./transcript";

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
  vi.resetAllMocks();
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
