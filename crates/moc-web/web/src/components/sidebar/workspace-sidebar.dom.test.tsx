import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { PlaygroundSidebar } from "@/components/image-playground/playground-sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { PlaygroundSessionSummary } from "@/lib/image-playground-api";
import type { SessionInfo } from "@/protocol/types";
import { useImagePlayground } from "@/store/image-playground";
import { SESSION_ORDER_KEY, SESSION_UNREAD_KEY, useSessionMeta } from "@/store/session-meta";
import { useSessions } from "@/store/sessions";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";
import { useWorkflow } from "@/store/workflow";
import { WorkspaceSidebar } from "./workspace-sidebar";

const initial = useWorkflow.getState();
const initialSettings = useUi.getState().settings;
const initialSessions = useSessions.getState();
const initialTabs = useTabs.getState();
const initialTranscript = useTranscript.getState();
const initialPlayground = useImagePlayground.getState();
afterEach(() => {
  cleanup();
  localStorage.removeItem(SESSION_ORDER_KEY);
  localStorage.removeItem(SESSION_UNREAD_KEY);
  useSessionMeta.setState({ orderByCwd: {}, unread: {} });
  useWorkflow.setState(initial, true);
  useSessions.setState(initialSessions, true);
  useTabs.setState(initialTabs, true);
  useTranscript.setState(initialTranscript, true);
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

it("支持同工作区拖拽排序、新会话自动置顶、重置顺序与自动 GC 清理，且三点按钮无矩形背景与焦点框", () => {
  const make = (id: string, title: string, updatedAt: number): SessionInfo => ({
    id,
    title,
    cwd: "/repo/moc",
    model: "p/m",
    engine: "standard",
    autonomy: "default",
    permission: "workspace",
    plan_active: false,
    fast_mode: false,
    engine_locked: false,
    created_at_ms: updatedAt,
    updated_at_ms: updatedAt,
  });
  const s1 = make("s-1", "会话 1", 300);
  const s2 = make("s-2", "会话 2", 200);
  const s3 = make("s-3", "会话 3", 100);
  useSessions.setState({ list: [s1, s2, s3], extra: {}, draftSession: null });

  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );

  // 右上角「…」按钮不带矩形背景与焦点外框。
  const actionBtn = sidebar.getAllByRole("button", { name: "Session actions" })[0];
  expect(actionBtn.className).not.toContain("hover:bg-accent");
  expect(actionBtn.className).toContain("focus-visible:outline-none");

  const titles = () =>
    sidebar
      .getAllByRole("button", { name: /^会话 \d+$/ })
      .map((el) => el.getAttribute("aria-label"));
  expect(titles()).toEqual(["会话 1", "会话 2", "会话 3"]);

  // 把「会话 3」拖到「会话 1」上半区（before）。
  const row1 = sidebar.getByRole("button", { name: "会话 1" }).parentElement;
  const row3 = sidebar.getByRole("button", { name: "会话 3" }).parentElement;
  if (!row1 || !row3) throw new Error("会话行缺失");
  row1.getBoundingClientRect = () =>
    ({
      top: 100,
      height: 44,
      bottom: 144,
      left: 0,
      right: 200,
      width: 200,
      x: 0,
      y: 100,
      toJSON: () => ({}),
    }) as DOMRect;
  fireEvent.dragStart(row3);
  fireEvent(row1, new MouseEvent("dragover", { bubbles: true, cancelable: true, clientY: 105 }));
  fireEvent.drop(row1);
  expect(titles()).toEqual(["会话 3", "会话 1", "会话 2"]);
  expect(JSON.parse(localStorage.getItem(SESSION_ORDER_KEY) ?? "{}")).toEqual({
    "/repo/moc": ["s-3", "s-1", "s-2"],
  });

  // 已有会话更新时间戳不打乱手动顺序；新建会话自动置顶。
  act(() => {
    useSessions.getState().upsert({ ...s2, updated_at_ms: 900 });
    useSessions.getState().upsert(make("s-4", "会话 4", 1000));
  });
  expect(titles()).toEqual(["会话 4", "会话 3", "会话 1", "会话 2"]);

  // 拖到下半区（after）可将「会话 4」移到「会话 1」之后。
  const row4 = sidebar.getByRole("button", { name: "会话 4" }).parentElement;
  const row1After = sidebar.getByRole("button", { name: "会话 1" }).parentElement;
  if (!row4 || !row1After) throw new Error("会话行缺失");
  row1After.getBoundingClientRect = () =>
    ({
      top: 100,
      height: 44,
      bottom: 144,
      left: 0,
      right: 200,
      width: 200,
      x: 0,
      y: 100,
      toJSON: () => ({}),
    }) as DOMRect;
  fireEvent.dragStart(row4);
  fireEvent(
    row1After,
    new MouseEvent("dragover", { bubbles: true, cancelable: true, clientY: 138 }),
  );
  fireEvent.drop(row1After);
  expect(titles()).toEqual(["会话 3", "会话 1", "会话 4", "会话 2"]);

  // 删除与 prune 自动裁剪失效 ID，工作区剩余 <=1 条自定义项时回收整个 key。
  act(() => {
    useSessions.getState().remove("s-3");
    useSessionMeta.getState().prune(
      new Set(["s-4", "s-1"]),
      new Map([
        ["s-4", "/repo/moc"],
        ["s-1", "/repo/moc"],
      ]),
    );
  });
  expect(JSON.parse(localStorage.getItem(SESSION_ORDER_KEY) ?? "{}")).toEqual({
    "/repo/moc": ["s-1", "s-4"],
  });
  act(() => {
    useSessions.getState().remove("s-4");
  });
  expect(localStorage.getItem(SESSION_ORDER_KEY)).toBeNull();
  expect(useSessionMeta.getState().orderByCwd).toEqual({});
});

it("会话卡按 running > attention > unread > idle 显示指示点，未读状态持久化并可随打开或清理清除", () => {
  const session: SessionInfo = {
    id: "s-bg",
    title: "后台会话",
    cwd: "/repo/moc",
    model: "p/m",
    engine: "standard",
    autonomy: "default",
    permission: "workspace",
    plan_active: false,
    fast_mode: false,
    engine_locked: false,
    created_at_ms: 100,
    updated_at_ms: 100,
  };
  useSessions.setState({ list: [session], extra: {}, draftSession: null });

  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );

  const indicator = () =>
    sidebar
      .getByRole("button", { name: "后台会话" })
      .parentElement?.querySelector("[data-indicator]");
  expect(indicator()?.getAttribute("data-indicator")).toBe("idle");

  // 标为未读后显示蓝点并写入 localStorage。
  act(() => useSessionMeta.getState().markUnread("s-bg"));
  expect(indicator()?.getAttribute("data-indicator")).toBe("unread");
  expect(indicator()?.className).toContain("bg-brand");
  expect(JSON.parse(localStorage.getItem(SESSION_UNREAD_KEY) ?? "[]")).toEqual(["s-bg"]);

  // 同时存在 attention 时优先橙点，attention 消除后恢复未读蓝点。
  act(() =>
    useTranscript.setState({
      permissions: {
        "s-bg": [
          {
            request_id: "r1",
            session_id: "s-bg",
            prompt_id: "p1",
            tool_call_id: "tc1",
            tool_name: "bash",
            title: "allow",
            options: [],
          },
        ],
      },
    }),
  );
  expect(indicator()?.getAttribute("data-indicator")).toBe("attention");
  act(() => useTranscript.setState({ permissions: {} }));
  expect(indicator()?.getAttribute("data-indicator")).toBe("unread");

  // 打开会话后清除未读并移除空 localStorage key。
  act(() => useSessionMeta.getState().markRead("s-bg"));
  expect(indicator()?.getAttribute("data-indicator")).toBe("idle");
  expect(localStorage.getItem(SESSION_UNREAD_KEY)).toBeNull();
});
