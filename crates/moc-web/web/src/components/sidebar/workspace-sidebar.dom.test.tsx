import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { SessionInfo } from "@/protocol/types";
import { useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useUi } from "@/store/ui";
import { useWorkflow } from "@/store/workflow";
import { WorkspaceSidebar } from "./workspace-sidebar";

const initial = useWorkflow.getState();
const initialSettings = useUi.getState().settings;
const initialSessions = useSessions.getState();
const initialTabs = useTabs.getState();
afterEach(() => {
  cleanup();
  useWorkflow.setState(initial, true);
  useSessions.setState(initialSessions, true);
  useTabs.setState(initialTabs, true);
  useUi.setState({ settings: initialSettings });
});

it("会话栏内可打开搜索，并在退出搜索后恢复焦点", () => {
  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );
  const search = sidebar.getByRole("button", { name: "Search" });
  fireEvent.click(search);
  const input = sidebar.getByRole("textbox", { name: "筛选 session 与目录" });
  expect(document.activeElement).toBe(input);
  fireEvent.change(input, { target: { value: "moc" } });
  fireEvent.keyDown(input, { key: "Escape" });
  expect(document.activeElement).toBe(search);
  fireEvent.click(search);
  expect(sidebar.getByRole("textbox", { name: "筛选 session 与目录" })).toHaveProperty("value", "");
});

it("空 workflow 侧栏正常挂载，徽标变化后仍能更新", () => {
  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );
  expect(sidebar.getByRole("button", { name: /New session/ })).toBeTruthy();
  expect(sidebar.queryByRole("button", { name: /Workflow/ })).toBeNull();
  act(() =>
    useWorkflow.setState({
      runs: {
        test: {
          run_id: "test",
          title: "测试",
          status: { state: "running" },
          plan: null,
          phases: [],
          roles: [],
          commands: [],
          reports: [],
          artifacts: [],
          holes: [],
          usage: null,
          log: [],
          artifactVersions: {},
        },
      },
    }),
  );
  expect(sidebar.getByRole("button", { name: /Workflow.*1 running/ })).toBeTruthy();
});

it("新建草稿会话不在左侧工作区列表占位，真正创建会话后才加入列表", () => {
  const existing: SessionInfo = {
    id: "s-existing",
    title: "已有会话",
    cwd: "/repo/moc",
    model: "p/m",
    engine: "standard",
    autonomy: "default",
    permission: "workspace",
    plan_active: false,
    fast_mode: false,
    engine_locked: false,
    created_at_ms: 1,
    updated_at_ms: 1,
  };
  useSessions.setState({
    list: [existing],
    extra: {},
    draftSession: {
      cwd: "/repo/moc",
      engine: "standard",
      autonomy: "default",
      plan: false,
      fast: false,
    },
  });
  useTabs.setState({ open: ["draft-session"], active: "draft-session" });

  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );

  // 仅顶部导航保留一个 New session 按钮，工作区列表内不出现临时草稿占位项。
  const newButtons = sidebar.getAllByRole("button", { name: "New session" });
  expect(newButtons).toHaveLength(1);
  expect(newButtons[0].getAttribute("aria-current")).toBe("true");
  expect(sidebar.getByRole("button", { name: "已有会话" })).toBeTruthy();

  // 真正创建新会话后立即出现在左侧列表并切换高亮。
  const created: SessionInfo = {
    ...existing,
    id: "s-created",
    title: "新创建的会话",
    updated_at_ms: 2,
  };
  act(() => {
    useSessions.getState().upsert(created);
    useSessions.getState().setDraftSession(null);
    useTabs.setState({ open: [created.id], active: created.id });
  });

  expect(sidebar.getByRole("button", { name: "新创建的会话" }).getAttribute("aria-current")).toBe(
    "true",
  );
  expect(sidebar.getByRole("button", { name: "New session" }).getAttribute("aria-current")).toBe(
    null,
  );
});
