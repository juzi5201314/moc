// workflow 前端状态：审批队列 + 每 run 的实时视图（SessionUpdate::workflow 驱动）。
// 形状与 crates/moc-protocol/src/methods/workflow.rs 逐字段对齐。

import { produce } from "immer";
import { create } from "zustand";
import { rpc } from "@/protocol/client";
import type {
  SessionInfo,
  SessionWorkflowApproveParams,
  SessionWorkflowArtifactParams,
  SessionWorkflowRunParams,
  SessionWorkflowRunsParams,
  SessionWorkflowRunsResult,
  SessionWorkflowSetRoleModelParams,
  WorkflowApproval,
  WorkflowArtifactResult,
  WorkflowDecision,
  WorkflowEvent,
  WorkflowPhaseState,
  WorkflowRoleState,
  WorkflowRunSnapshot,
  WorkflowRunStatus,
  WorkflowRunSummary,
} from "@/protocol/types";

/** run 面板视图：快照 + 事件累积的实时投影。 */
export type WorkflowRunView = {
  run_id: string;
  title: string;
  status: WorkflowRunStatus | null;
  plan: Record<string, unknown> | null;
  phases: WorkflowPhaseState[];
  roles: WorkflowRoleState[];
  commands: string[];
  reports: { item: unknown; artifact_id?: string }[];
  artifacts: Record<string, unknown>[];
  holes: Record<string, unknown>[];
  usage: unknown;
  /** 尾部日志（最多 200 行）。 */
  log: string[];
  /** artifact_id → 选中版本（1 起；缺省最新）。 */
  artifactVersions: Record<string, number>;
};

export type WorkflowState = {
  /** 待审批请求（按到达序）。 */
  approvals: WorkflowApproval[];
  /** 已打开查看的审批（关闭后再从徽标重开）。 */
  approvalOpen: boolean;
  /** run_id → 实时视图。 */
  runs: Record<string, WorkflowRunView>;
  /** run 列表（/workflows）。 */
  runList: WorkflowRunSummary[];
  /** 打开的 run 面板 id。 */
  panelRun: string | null;
  /** run 列表面板是否打开。 */
  runsOpen: boolean;
};

const emptyView = (runId: string): WorkflowRunView => ({
  run_id: runId,
  title: runId,
  status: null,
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
});

export const useWorkflow = create<WorkflowState>()(() => ({
  approvals: [],
  approvalOpen: false,
  runs: {},
  runList: [],
  panelRun: null,
  runsOpen: false,
}));

const set = (fn: (state: WorkflowState) => void) => useWorkflow.setState(produce(fn));

export function applyWorkflowEvent(state: WorkflowState, runId: string, event: WorkflowEvent) {
  switch (event.kind) {
    case "approval_requested":
      state.approvals.push(event.request);
      state.approvalOpen = true;
      break;
    case "approval_resolved":
      state.approvals = state.approvals.filter((a) => a.request_id !== event.request_id);
      if (state.approvals.length === 0) state.approvalOpen = false;
      break;
    case "run_status":
      state.runs[runId] = { ...emptyView(runId), ...state.runs[runId], status: event.status };
      break;
    case "node_updated": {
      if (!state.runs[runId]) state.runs[runId] = emptyView(runId);
      const view = state.runs[runId];
      view.phases = event.phases;
      view.roles = event.roles;
      break;
    }
    case "log": {
      if (!state.runs[runId]) state.runs[runId] = emptyView(runId);
      const view = state.runs[runId];
      view.log.push(`[${event.level}] ${event.message}`);
      if (view.log.length > 200) view.log.splice(0, view.log.length - 200);
      break;
    }
    case "report": {
      if (!state.runs[runId]) state.runs[runId] = emptyView(runId);
      const view = state.runs[runId];
      view.reports.push({ item: event.item, artifact_id: event.artifact_id ?? undefined });
      break;
    }
    case "artifact": {
      if (!state.runs[runId]) state.runs[runId] = emptyView(runId);
      const view = state.runs[runId];
      const artifact = event.artifact as Record<string, unknown>;
      artifact.artifact_id ??= "";
      view.artifacts.push(artifact);
      break;
    }
    case "hole_opened": {
      if (!state.runs[runId]) state.runs[runId] = emptyView(runId);
      const view = state.runs[runId];
      view.holes.push({ name: event.name, prompt: event.prompt, site_id: event.site_id });
      break;
    }
    case "hole_filled": {
      const view = state.runs[runId];
      if (view) view.holes = view.holes.filter((h) => h.name !== event.name);
      break;
    }
    case "usage": {
      if (!state.runs[runId]) state.runs[runId] = emptyView(runId);
      const view = state.runs[runId];
      view.usage = event.usage;
      break;
    }
    case "phase_entered":
      break;
  }
}

// SessionUpdate::workflow 路由：每 run 累积视图 + 审批队列。
rpc.on("session/update", ({ update }) => {
  if (update.type !== "workflow") return;
  set((state) => applyWorkflowEvent(state, update.run_id, update.event));
});

// —— rpc 动作 ——

export async function workflowActivate(sessionId: string): Promise<SessionInfo> {
  return rpc.call<SessionInfo>("session/workflow_activate", {
    session_id: sessionId,
  });
}

export async function workflowApprove(
  sessionId: string,
  requestId: string,
  decision: WorkflowDecision,
): Promise<void> {
  const params: SessionWorkflowApproveParams = {
    session_id: sessionId,
    request_id: requestId,
    decision,
  };
  await rpc.call("session/workflow_approve", params);
}

export async function workflowRuns(sessionId: string): Promise<WorkflowRunSummary[]> {
  const params: SessionWorkflowRunsParams = { session_id: sessionId };
  const result = await rpc.call<SessionWorkflowRunsResult>("session/workflow_runs", params);
  return result.runs ?? [];
}

export async function workflowRun(sessionId: string, runId: string): Promise<WorkflowRunSnapshot> {
  const params: SessionWorkflowRunParams = { session_id: sessionId, run_id: runId };
  return rpc.call<WorkflowRunSnapshot>("session/workflow_run", params);
}

export async function workflowStop(sessionId: string, runId: string): Promise<void> {
  const params: SessionWorkflowRunParams = { session_id: sessionId, run_id: runId };
  await rpc.call("session/workflow_stop", params);
}

export async function workflowResume(sessionId: string, runId: string): Promise<void> {
  const params: SessionWorkflowRunParams = { session_id: sessionId, run_id: runId };
  await rpc.call("session/workflow_resume", params);
}

export async function workflowSetRoleModel(
  sessionId: string,
  runId: string,
  roleId: string,
  model: string,
): Promise<void> {
  const params: SessionWorkflowSetRoleModelParams = {
    session_id: sessionId,
    run_id: runId,
    role_id: roleId,
    model,
  };
  await rpc.call("session/workflow_set_role_model", params);
}

export async function workflowArtifact(
  params: SessionWorkflowArtifactParams,
): Promise<WorkflowArtifactResult> {
  return rpc.call<WorkflowArtifactResult>("session/workflow_artifact", params);
}

/** 打开 run 面板：快照灌进视图（事件投影在其上叠加）。 */
export async function openWorkflowRun(sessionId: string, runId: string): Promise<void> {
  const snapshot = await workflowRun(sessionId, runId);
  set((state) => {
    const view = emptyView(runId);
    view.title = snapshot.title ?? runId;
    view.status = snapshot.status;
    view.plan = (snapshot.plan as Record<string, unknown>) ?? null;
    view.phases = snapshot.phases ?? [];
    view.roles = snapshot.roles ?? [];
    view.commands = snapshot.commands ?? [];
    view.reports = (snapshot.reports ?? []).map((r) => {
      const entry = r as { item?: unknown; artifact_id?: string };
      return entry.item !== undefined
        ? { item: entry.item, artifact_id: entry.artifact_id }
        : { item: r };
    });
    view.artifacts = (snapshot.artifacts ?? []) as Record<string, unknown>[];
    view.holes = (snapshot.holes ?? []) as Record<string, unknown>[];
    view.usage = snapshot.usage;
    state.runs[runId] = view;
    state.panelRun = runId;
    state.runsOpen = false;
  });
}

/** 徽标数：待审批 + 活跃 run。 */
export function selectWorkflowBadges(state: WorkflowState): { approvals: number; runs: number } {
  return {
    approvals: state.approvals.length,
    runs: Object.values(state.runs).filter(
      (view) => view.status?.state === "running" || view.status?.state === "pending",
    ).length,
  };
}
