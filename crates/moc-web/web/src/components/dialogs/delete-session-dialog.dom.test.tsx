import { act, cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { toast } from "sonner";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useI18n } from "@/lib/i18n";
import type { SessionInfo } from "@/protocol/types";

vi.mock("@/protocol/client", () => ({
  rpc: { on: vi.fn(), onRequest: vi.fn(), call: vi.fn(), connect: vi.fn(), dispose: vi.fn() },
  clientListeners: { ready: new Set(), status: new Set(), handshakeError: new Set() },
}));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() }, Toaster: () => null }));
vi.mock("@/components/layout/AppFrame", () => ({ AppFrame: () => null }));
vi.mock("@/components/workflow/workflow-approval-dialog", () => ({
  WorkflowApprovalDialog: () => null,
}));
vi.mock("@/components/workflow/workflow-run-panel", () => ({ WorkflowRunPanel: () => null }));
vi.mock("@/components/settings/settings-dialog", () => ({ SettingsDialog: () => null }));
vi.mock("@/components/dialogs/command-palette", () => ({ CommandPalette: () => null }));
vi.mock("@/components/dialogs/workspace-picker", () => ({ WorkspacePicker: () => null }));
vi.mock("@/components/context/context-dialog", () => ({ ContextDialog: () => null }));
vi.mock("@/lib/shortcuts", () => ({ useGlobalShortcuts: () => {} }));
vi.mock("@/lib/theme", () => ({ useThemeSync: () => {} }));

import { App } from "@/App";
import { rpc } from "@/protocol/client";
import { useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

const session: SessionInfo = {
  id: "delete-me",
  title: "待删除会话",
  model: "p/model",
  cwd: "/workspace",
  engine: "standard",
  autonomy: "default",
  permission: "full",
  plan_active: false,
  fast_mode: false,
  engine_locked: false,
  created_at_ms: 0,
  updated_at_ms: 0,
};
const other = { ...session, id: "keep-me", title: "保留会话" };
const initialUi = useUi.getState();
const initialTabs = useTabs.getState();
const initialSessions = useSessions.getState();
const initialTranscript = useTranscript.getState();
const initialLocale = useI18n.getState().locale;

beforeEach(() => {
  vi.clearAllMocks();
  useI18n.setState({ locale: "zh" });
  useUi.setState({ confirmDelete: null });
  useTabs.setState({ open: [session.id, other.id], active: session.id });
  useSessions.setState({ list: [session, other], extra: {}, draftSession: null });
  useTranscript.getState().ensureView(session);
  useTranscript.getState().ensureView(other);
  vi.mocked(rpc.call).mockImplementation(async (method) =>
    method === "session/list" ? { sessions: [other] } : {},
  );
});

afterEach(() => {
  cleanup();
  useUi.setState(initialUi);
  useTabs.setState(initialTabs);
  useSessions.setState(initialSessions);
  useTranscript.setState(initialTranscript);
  useI18n.setState({ locale: initialLocale });
  vi.resetAllMocks();
});

function requestDelete(title = session.title) {
  act(() => useUi.getState().set({ confirmDelete: { id: session.id, title } }));
}

it("全局挂载读取删除请求，取消不会发起 RPC", () => {
  const view = render(<App />);
  expect(view.queryByRole("alertdialog")).toBeNull();
  requestDelete();
  expect(view.getByRole("alertdialog").textContent).toContain(session.title);
  expect(rpc.call).not.toHaveBeenCalled();
  fireEvent.click(view.getByRole("button", { name: "Cancel" }));
  expect(view.queryByRole("alertdialog")).toBeNull();
  expect(useUi.getState().confirmDelete).toBeNull();
  expect(rpc.call).not.toHaveBeenCalled();
});

it("确认后调用真实删除 action，移除当前会话并切换到剩余会话", async () => {
  const pending = Promise.withResolvers<object>();
  vi.mocked(rpc.call).mockImplementationOnce(() => pending.promise);
  const view = render(<App />);
  requestDelete();
  const remove = view.getByRole("button", { name: "Delete" });
  fireEvent.click(remove);
  expect(rpc.call).toHaveBeenCalledWith("session/delete", { session_id: session.id });
  expect(remove.hasAttribute("disabled")).toBe(true);
  expect(view.getByRole("button", { name: "Cancel" }).hasAttribute("disabled")).toBe(true);
  expect(useTabs.getState().active).toBe(session.id);
  await act(async () => pending.resolve({}));
  await waitFor(() => expect(view.queryByRole("alertdialog")).toBeNull());
  expect(useTabs.getState().active).toBe(other.id);
  expect(useTabs.getState().open).toEqual([other.id]);
  expect(useTranscript.getState().bySession[session.id]).toBeUndefined();
  expect(useSessions.getState().list).toEqual([other]);
  expect(toast.success).toHaveBeenCalledWith("session 已删除。");
});

it("删除非当前会话不改变当前会话，空标题使用后备文案", async () => {
  useTabs.setState({ active: other.id });
  const view = render(<App />);
  requestDelete("");
  expect(view.getByRole("alertdialog").textContent).toContain("New session");
  fireEvent.click(view.getByRole("button", { name: "Delete" }));
  await waitFor(() => expect(toast.success).toHaveBeenCalled());
  expect(useTabs.getState().active).toBe(other.id);
});

it("删除最后一个打开的会话后显示新草稿", async () => {
  useTabs.setState({ open: [session.id] });
  const view = render(<App />);
  requestDelete();
  fireEvent.click(view.getByRole("button", { name: "Delete" }));
  await waitFor(() => expect(toast.success).toHaveBeenCalled());
  expect(useTabs.getState().active).toBe("draft-session");
  expect(useTranscript.getState().bySession[session.id]).toBeUndefined();
});

it("RPC 失败提示错误并保留会话与确认框，允许取消", async () => {
  vi.mocked(rpc.call).mockRejectedValueOnce(new Error("SESSION_LOCKED"));
  const view = render(<App />);
  requestDelete();
  fireEvent.click(view.getByRole("button", { name: "Delete" }));
  await waitFor(() =>
    expect(toast.error).toHaveBeenCalledWith("删除 session 失败：SESSION_LOCKED"),
  );
  expect(view.getByRole("alertdialog")).toBeTruthy();
  expect(useTabs.getState().active).toBe(session.id);
  expect(useTranscript.getState().bySession[session.id]).toBeTruthy();
  expect(useSessions.getState().list).toContainEqual(session);
  expect(toast.success).not.toHaveBeenCalled();
  fireEvent.click(view.getByRole("button", { name: "Cancel" }));
  expect(useUi.getState().confirmDelete).toBeNull();
});

it("删除成功后立即关闭，即使列表刷新挂起或失败也不误报删除失败", async () => {
  const refresh = Promise.withResolvers<never>();
  vi.mocked(rpc.call).mockImplementation(async (method) => {
    if (method === "session/list") return refresh.promise;
    return {};
  });
  const view = render(<App />);
  requestDelete();
  fireEvent.click(view.getByRole("button", { name: "Delete" }));
  await waitFor(() => expect(toast.success).toHaveBeenCalledWith("session 已删除。"));
  expect(view.queryByRole("alertdialog")).toBeNull();
  expect(useTabs.getState().active).toBe(other.id);
  expect(useTranscript.getState().bySession[session.id]).toBeUndefined();
  expect(useSessions.getState().list).toEqual([other]);
  await act(async () => refresh.reject(new Error("list unavailable")));
  expect(toast.error).not.toHaveBeenCalled();
  expect(useUi.getState().confirmDelete).toBeNull();
  expect(
    vi.mocked(rpc.call).mock.calls.filter(([method]) => method === "session/delete"),
  ).toHaveLength(1);
});
