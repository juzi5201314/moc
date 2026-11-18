import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useUi } from "@/store/ui";
import { useWorkflow } from "@/store/workflow";
import { WorkspaceSidebar } from "./workspace-sidebar";

const initial = useWorkflow.getState();
const initialSettings = useUi.getState().settings;
afterEach(() => {
  cleanup();
  useWorkflow.setState(initial, true);
  useUi.setState({ settings: initialSettings });
});

it("会话栏内可打开搜索、设置及插件，并在退出搜索后恢复焦点", () => {
  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );
  const search = sidebar.getByRole("button", { name: "搜索" });
  fireEvent.click(search);
  const input = sidebar.getByRole("textbox", { name: "筛选会话与目录" });
  expect(document.activeElement).toBe(input);
  fireEvent.change(input, { target: { value: "moc" } });
  fireEvent.keyDown(input, { key: "Escape" });
  expect(document.activeElement).toBe(search);
  fireEvent.click(search);
  expect(sidebar.getByRole("textbox", { name: "筛选会话与目录" })).toHaveProperty("value", "");

  fireEvent.click(sidebar.getByRole("button", { name: "插件" }));
  expect(useUi.getState().settings).toEqual({ open: true, section: "plugins" });
  fireEvent.click(sidebar.getByRole("button", { name: "设置" }));
  expect(useUi.getState().settings.open).toBe(true);
  expect(useUi.getState().settings.section).toBeUndefined();
});

it("空 workflow 侧栏正常挂载，徽标变化后仍能更新", () => {
  const sidebar = render(
    <TooltipProvider>
      <WorkspaceSidebar />
    </TooltipProvider>,
  );
  expect(sidebar.getByRole("button", { name: /新会话/ })).toBeTruthy();
  expect(sidebar.queryByRole("button", { name: /workflow/ })).toBeNull();
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
  expect(sidebar.getByRole("button", { name: /workflow.*1跑/ })).toBeTruthy();
});
