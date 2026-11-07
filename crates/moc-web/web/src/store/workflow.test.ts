import { describe, expect, it } from "vitest";
import type { WorkflowRunStatus } from "@/protocol/types";
import { applyWorkflowEvent, type WorkflowState } from "./workflow";

const base = (): WorkflowState => ({
  approvals: [],
  approvalOpen: false,
  runs: {},
  runList: [],
  panelRun: null,
  runsOpen: false,
});

const approval = {
  request_id: "req-1",
  run_id: "run-1",
  script: 'phase("a")',
  plan: {},
  defaults: { role_models: {}, model_ref_map: {} },
  args: undefined,
  max_concurrency: 4,
  kind: "launch" as const,
};

describe("workflow store 归约", () => {
  it("approval_requested 入队并开窗；resolved 出队", () => {
    const state = base();
    applyWorkflowEvent(state, "run-1", { kind: "approval_requested", request: approval });
    expect(state.approvals).toHaveLength(1);
    expect(state.approvalOpen).toBe(true);
    applyWorkflowEvent(state, "run-1", {
      kind: "approval_resolved",
      request_id: "req-1",
      approved: true,
    });
    expect(state.approvals).toHaveLength(0);
    expect(state.approvalOpen).toBe(false);
  });

  it("node_updated/report/hole/usage 投影到 run 视图", () => {
    const state = base();
    applyWorkflowEvent(state, "r", {
      kind: "node_updated",
      phases: [{ id: "p1", name: "plan", status: "running", asks_done: 1 }],
      roles: [],
    });
    applyWorkflowEvent(state, "r", { kind: "report", item: { n: 1 }, artifact_id: "t1" });
    applyWorkflowEvent(state, "r", {
      kind: "hole_opened",
      site_id: "hole#1",
      ordinal: 0,
      name: "missing",
      prompt: "fill",
    });
    applyWorkflowEvent(state, "r", { kind: "usage", usage: { spent_tokens: 5 } });
    const view = state.runs.r;
    expect(view.phases[0].status).toBe("running");
    expect(view.reports[0].artifact_id).toBe("t1");
    expect(view.holes[0].name).toBe("missing");
    expect((view.usage as { spent_tokens: number }).spent_tokens).toBe(5);
    applyWorkflowEvent(state, "r", { kind: "hole_filled", name: "missing" });
    expect(view.holes).toHaveLength(0);
  });

  it("run_status 终态落视图；log 有 200 行上限", () => {
    const state = base();
    applyWorkflowEvent(state, "r", {
      kind: "run_status",
      status: { state: "completed" } as WorkflowRunStatus,
    });
    expect(state.runs.r.status?.state).toBe("completed");
    for (let i = 0; i < 210; i += 1) {
      applyWorkflowEvent(state, "r", { kind: "log", level: "info", message: `m${i}` });
    }
    expect(state.runs.r.log).toHaveLength(200);
    expect(state.runs.r.log[199]).toContain("m209");
  });
});
