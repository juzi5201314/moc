import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { PlaygroundSidebar } from "@/components/image-playground/playground-sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { PlaygroundSessionSummary } from "@/lib/image-playground-api";
import type { SessionInfo } from "@/protocol/types";
import { useImagePlayground } from "@/store/image-playground";
import { useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useUi } from "@/store/ui";
import { useWorkflow } from "@/store/workflow";
import { WorkspaceSidebar } from "./workspace-sidebar";

const initial = useWorkflow.getState();
const initialSettings = useUi.getState().settings;
const initialSessions = useSessions.getState();
const initialTabs = useTabs.getState();
const initialPlayground = useImagePlayground.getState();
afterEach(() => {
  cleanup();
  useWorkflow.setState(initial, true);
  useSessions.setState(initialSessions, true);
  useTabs.setState(initialTabs, true);
  useImagePlayground.setState(initialPlayground, true);
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
      permission: "full",
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

it("超长会话列表按段增量展开，展开多次后可从吸顶头或底部一键全部折叠", () => {
  const sessions: SessionInfo[] = Array.from({ length: 60 }, (_, index) => ({
    id: `s-${index}`,
    title: `会话 ${index + 1}`,
    cwd: "/repo/moc",
    model: "p/m",
    engine: "standard",
    autonomy: "default",
    permission: "workspace",
    plan_active: false,
    fast_mode: false,
    engine_locked: false,
    created_at_ms: 1000 - index,
    updated_at_ms: 1000 - index,
  }));
  useSessions.setState({ list: sessions, extra: {}, draftSession: null });

  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );

  // 初始 6 条，剩余 54 条按 +20 分段
  expect(sidebar.getAllByRole("button", { name: /^会话 \d+$/ })).toHaveLength(6);
  expect(sidebar.queryByRole("button", { name: "Collapse" })).toBeNull();
  fireEvent.click(sidebar.getByRole("button", { name: "More +20 (54)" }));

  // 第 1 次展开 -> 26 条，剩 34 条，同时吸顶头与底部均提供一键 Collapse
  expect(sidebar.getAllByRole("button", { name: /^会话 \d+$/ })).toHaveLength(26);
  expect(sidebar.getAllByRole("button", { name: "Collapse" })).toHaveLength(2);
  fireEvent.click(sidebar.getByRole("button", { name: "More +20 (34)" }));

  // 第 2 次展开 -> 46 条，剩 14 条（<= 24 余量合并为一次展完）
  expect(sidebar.getAllByRole("button", { name: /^会话 \d+$/ })).toHaveLength(46);
  fireEvent.click(sidebar.getByRole("button", { name: "More (14)" }));
  expect(sidebar.getAllByRole("button", { name: /^会话 \d+$/ })).toHaveLength(60);
  expect(sidebar.queryByRole("button", { name: /^More/ })).toBeNull();

  // 点击吸顶标题行尾的 Collapse 一键收回全部展开的段
  fireEvent.click(sidebar.getAllByRole("button", { name: "Collapse" })[0]);
  expect(sidebar.getAllByRole("button", { name: /^会话 \d+$/ })).toHaveLength(6);
  expect(sidebar.getByRole("button", { name: "More +20 (54)" })).toBeTruthy();
  expect(sidebar.queryByRole("button", { name: "Collapse" })).toBeNull();
});

it("Image Playground 侧栏会话列表同步支持分段展开与一键全部折叠", () => {
  const sessions: PlaygroundSessionSummary[] = Array.from({ length: 40 }, (_, index) => ({
    id: `pg-${index}`,
    title: `生图 ${index + 1}`,
    created_at_ms: 1000 - index,
    updated_at_ms: 1000 - index,
    model: "openai/gpt-image-1",
    turn_count: 1,
    image_count: 1,
  }));
  useImagePlayground.setState({ sessions, activeSessionId: null, view: "generate" });

  const sidebar = render(
    <TooltipProvider>
      <PlaygroundSidebar />
    </TooltipProvider>,
  );

  // 初始 10 条，剩余 30 条按 +20 分段
  expect(sidebar.getAllByRole("button", { name: /^生图 \d+$/ })).toHaveLength(10);
  fireEvent.click(sidebar.getByRole("button", { name: "More +20 (30)" }));

  // 展开 1 次 -> 30 条，剩余 10 条一次展完，同时支持一键收起
  expect(sidebar.getAllByRole("button", { name: /^生图 \d+$/ })).toHaveLength(30);
  expect(sidebar.getAllByRole("button", { name: "Collapse" })).toHaveLength(2);
  fireEvent.click(sidebar.getByRole("button", { name: "More (10)" }));
  expect(sidebar.getAllByRole("button", { name: /^生图 \d+$/ })).toHaveLength(40);

  // 一键全部折叠回默认 10 条
  fireEvent.click(sidebar.getAllByRole("button", { name: "Collapse" })[0]);
  expect(sidebar.getAllByRole("button", { name: /^生图 \d+$/ })).toHaveLength(10);
  expect(sidebar.getByRole("button", { name: "More +20 (30)" })).toBeTruthy();
});
