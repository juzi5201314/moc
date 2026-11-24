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
