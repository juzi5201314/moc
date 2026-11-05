import { act, cleanup, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useWorkflow } from "@/store/workflow";
import { WorkspaceSidebar } from "./workspace-sidebar";

const initial = useWorkflow.getState();
afterEach(() => {
  cleanup();
  useWorkflow.setState(initial, true);
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
