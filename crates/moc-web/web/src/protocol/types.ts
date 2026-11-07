// moc 协议线上类型。主体由 `protocol.schema.json` 生成（`types.gen.ts`，
// 勿手改）；本文件只留生成管线覆盖不到的内容：协议版本、web 侧常量、
// helper、非 RPC 面的类型（/commands.json）与历史命名别名。
// Rust 侧改 DTO 后：MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol && pnpm gen:protocol。
// 会增长的枚举（SessionUpdate / AssistantDelta / BtwUpdate）遇到未知 type 必须整条丢弃。

import type {
  BtwUpdate,
  ContextContentSection,
  FinishReason,
  SessionUpdate,
  ToolDef,
} from "./types.gen";

export * from "./types.gen";

export const PROTOCOL_VERSION = "1.4";

// ---------- /commands.json（HTTP 面，不走 RPC，不进 schema） ----------

/** `/commands.json` 返回的内置斜杠命令元数据（core 命令注册表的可序列化视图）。 */
export type CommandMeta = {
  name: string;
  aliases: string[];
  usage: string;
  description: string;
  /** 分组的机器键：session/mode/context/agents/plugins/ui。 */
  group: string;
  needsSession: boolean;
  availableDuringTask: boolean;
  availableInSide: boolean;
  /** 依赖的特性键；`config.features[flag] === false` 时命令禁用。 */
  flag?: string;
};

// ---------- 历史命名别名：Rust 侧名 ≠ 前端既有名 ----------

export type ToolDefinition = ToolDef;
export type ContextNode = ContextContentSection;

// ---------- 会话更新类型表 ----------

export type SessionUpdateType = SessionUpdate["type"];

/** 已知的 update 类型；不在表里的整条丢弃。键集合与 `SessionUpdate` 一一对应，漏项编译失败。 */
export const SESSION_UPDATE_TYPES: Record<SessionUpdateType, true> = {
  agent_started: true,
  agent_status_changed: true,
  agent_message: true,
  tool_progress: true,
  tool_batch_started: true,
  prompt_started: true,
  assistant_delta: true,
  generation_started: true,
  bash_started: true,
  tool_started: true,
  tool_finished: true,
  permission_requested: true,
  permission_resolved: true,
  prompt_finished: true,
  guard_notice: true,
  plugin_notice: true,
  title_changed: true,
  lock_changed: true,
  permission_changed: true,
  engine_changed: true,
  autonomy_changed: true,
  plan_mode_changed: true,
  model_changed: true,
  fast_mode_changed: true,
  goal_changed: true,
  goal_round_message: true,
  todo_changed: true,
  todo_reminder: true,
  plan_submitted: true,
  plan_updated: true,
  context_compaction_started: true,
  context_compacted: true,
  context_compaction_failed: true,
  job_started: true,
  job_updated: true,
  job_output: true,
  job_finished: true,
  workspace_changed: true,
  upstream_mismatch: true,
  steering_applied: true,
  workflow: true,
  error: true,
  prompt_rewound: true,
};

export const BTW_UPDATE_TYPES: Record<BtwUpdate["type"], true> = {
  started: true,
  waiting: true,
  generating: true,
  text_delta: true,
  reasoning_delta: true,
};

export const MAX_BTW_HISTORY_TURNS = 20;

export const FS_LIST_DIR_MAX_ENTRIES = 1000;

/** FinishReason 的展示用小写键。 */
export function finishReasonKey(reason: FinishReason): string {
  return typeof reason === "string" ? reason : reason.Other;
}

// ---------- chart/analyze 的 UI 侧形状（Rust 侧 spec 是自由 JSON） ----------

export type ChartPoint = {
  value: number;
  text: string;
  upper: number | null;
  total: number | null;
};

export type ChartSeries = {
  name: string;
  dim: "Count" | "Percent" | "Duration" | "Bytes" | "Currency" | "Ratio" | "Fraction" | "Rate";
  unit: string;
  factor: number;
  polarity: "Higher" | "Lower" | null;
  points: Array<ChartPoint | null>;
};

export type ChartSpec = {
  kind: "Bar" | "Grouped" | "Paired" | "Line" | "Stacked" | "Progress" | "Heatmap" | "Scatter";
  axis: string;
  categories: string[];
  series: ChartSeries[];
  normalized: boolean;
  diverging: boolean;
  notes: Array<string | null>;
};
