// 审批对话框交互：role/ModelRef 覆盖进 decision、只读禁用、拒绝带反馈。
// WorkflowModelSelect 与 xyflow Canvas 用桩替换——本测试只断言 decision 组装与禁用态。

import { fireEvent, render, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { WorkflowApproval } from "@/protocol/types";

const { approve, flags } = vi.hoisted(() => ({
  approve: vi.fn((_sessionId: string, _requestId: string, _decision: unknown) => Promise.resolve()),
  flags: { readOnly: false },
}));

vi.mock("@/store/workflow", () => ({
  workflowApprove: approve,
  useWorkflow: vi.fn(),
  openWorkflowRun: vi.fn(),
  workflowRuns: vi.fn(),
  workflowArtifact: vi.fn(),
  workflowResume: vi.fn(),
  workflowSetRoleModel: vi.fn(),
  workflowStop: vi.fn(),
}));

vi.mock("@/components/workflow/workflow-graph", () => ({
  WorkflowGraph: () => <div data-testid="graph" />,
}));

// 桩：点击按钮即回传 `<target>:high`，便于断言覆盖写进 decision。
vi.mock("@/components/workflow/workflow-model-select", () => ({
  WorkflowModelSelect: ({
    value,
    onSelect,
    disabled,
  }: {
    value: string;
    onSelect: (s: string) => void;
    disabled?: boolean;
  }) => (
    <button type="button" disabled={disabled} onClick={() => onSelect(`${value || "picked"}:high`)}>
      {value || "select"}
    </button>
  ),
}));

vi.mock("@/store/transcript", () => ({
  useTranscript: (selector: (s: { bySession: Record<string, { readOnly: boolean }> }) => unknown) =>
    selector({ bySession: { s1: { readOnly: flags.readOnly } } }),
}));

import { WorkflowApprovalDialog } from "./workflow-approval-dialog";

const approval: WorkflowApproval = {
  request_id: "req-1",
  run_id: "run-1",
  script: 'phase("a")',
  plan: {
    phases: [{ id: "p1", name: "a", sites: ["p1", "r1"] }],
    edges: [],
    roles: [
      { role_id: "r1", name_pattern: "planner" },
      { role_id: "r2", name_pattern: "reviewer" },
    ],
    model_refs: [{ ref_id: "m1" }],
  },
  defaults: { role_models: { r1: "default/model" }, model_ref_map: { m1: "default/ref" } },
  args: undefined,
  max_concurrency: 4,
  kind: "launch",
};

describe("WorkflowApprovalDialog", () => {
  beforeEach(() => {
    approve.mockClear();
    flags.readOnly = false;
  });

  it("选中 role 模型后批准：decision 携带覆盖、未选 role 不出现", async () => {
    const { getAllByText, getByText } = render(
      <WorkflowApprovalDialog sessionId="s1" approval={approval} open onOpenChange={() => {}} />,
    );
    // 两个 role + 一个 ref 的桩按钮：只点第一个（planner/r1）。
    const pickers = getAllByText(/select|default/);
    fireEvent.click(pickers[0]);
    fireEvent.click(getByText("批准"));
    await waitFor(() => expect(approve).toHaveBeenCalledTimes(1));
    const call = approve.mock.calls[0];
    expect(call[0]).toBe("s1");
    expect(call[1]).toBe("req-1");
    const decision = call[2] as {
      action: string;
      role_models: Record<string, string>;
      model_ref_map: Record<string, string>;
    };
    expect(decision.action).toBe("approve");
    expect(decision.role_models).toEqual({ r1: "default/model:high" });
    expect(decision.model_ref_map).toEqual({});
  });

  it("ModelRef 重映射写进 decision.model_ref_map", async () => {
    const { getAllByText, getByText } = render(
      <WorkflowApprovalDialog sessionId="s1" approval={approval} open onOpenChange={() => {}} />,
    );
    const pickers = getAllByText(/select|default/);
    // 三个选择器：r1、r2、m1 —— 只点 m1。
    fireEvent.click(pickers[2]);
    fireEvent.click(getByText("批准"));
    await waitFor(() => expect(approve).toHaveBeenCalledTimes(1));
    const decision = approve.mock.calls[0][2] as { model_ref_map: Record<string, string> };
    expect(decision.model_ref_map).toEqual({ m1: "default/ref:high" });
  });

  it("拒绝：feedback 进入 decision", async () => {
    const { getByText, getByPlaceholderText } = render(
      <WorkflowApprovalDialog sessionId="s1" approval={approval} open onOpenChange={() => {}} />,
    );
    fireEvent.click(getByText("拒绝"));
    fireEvent.change(getByPlaceholderText(/拒绝反馈/), { target: { value: "改一下范围" } });
    fireEvent.click(getByText("确认拒绝"));
    await waitFor(() => expect(approve).toHaveBeenCalledTimes(1));
    expect(approve.mock.calls[0][2]).toEqual({ action: "reject", feedback: "改一下范围" });
  });

  it("只读连接：选择器与操作全部 disabled（关闭钮不禁——查看允许）", () => {
    flags.readOnly = true;
    const { getByText, getAllByRole } = render(
      <WorkflowApprovalDialog sessionId="s1" approval={approval} open onOpenChange={() => {}} />,
    );
    // 桩选择器按钮（select/default 文案）全禁；批准/拒绝按钮全禁。
    // 项目未注册 jest-dom，直接断言 disabled 属性。
    for (const picker of getAllByRole("button", { name: /select|default/ })) {
      expect(picker.hasAttribute("disabled")).toBe(true);
    }
    expect(getByText("批准").hasAttribute("disabled")).toBe(true);
    expect(getByText("拒绝").hasAttribute("disabled")).toBe(true);
  });
});
