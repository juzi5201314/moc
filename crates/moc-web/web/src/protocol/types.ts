// moc 协议线上形状：对照 crates/moc-protocol 与 crates/moc-llm 的 serde DTO 手写。
// 会增长的枚举（SessionUpdate / AssistantDelta / BtwUpdate）遇到未知 type 必须整条丢弃。

export const PROTOCOL_VERSION = "1.4";

// ---------- moc-llm 内容类型（外部标签，serde 默认） ----------

export type Role = "System" | "Developer" | "User" | "Assistant" | "Tool" | "Custom";

export type ImageSource = { Url: string } | { Base64: { media_type: string; data: string } };
export type ImageDetail = "auto" | "low" | "high";

export type ReasoningBlock =
  | { Text: { text: string; signature: string | null } }
  | { Redacted: { data: string } }
  | { Item: unknown };

export type ToolCall = {
  id: string;
  name: string;
  arguments: string;
};

export type ToolResult = {
  tool_call_id: string;
  content: string;
  is_error: boolean;
  images?: ImageSource[];
};

export type ContentPart =
  | { Text: string }
  | { Image: { source: ImageSource; detail: ImageDetail | null } }
  | { Reasoning: ReasoningBlock }
  | { ToolCall: ToolCall }
  | { ToolResult: ToolResult };

export type Message = {
  role: Role;
  content: ContentPart[];
};

export type FinishReason =
  | "Stop"
  | "StopSequence"
  | "Length"
  | "ToolCalls"
  | "ContentFilter"
  | "Refusal"
  | "PauseTurn"
  | "Cancelled"
  | "Interrupted"
  | "LoopDetected"
  | { Other: string };

export type Usage = {
  input_tokens: number;
  output_tokens: number;
  total_tokens: number | null;
  cached_input_tokens: number | null;
  cache_creation_input_tokens: number | null;
  reasoning_tokens: number | null;
};

// ---------- initialize / config ----------

export type ClientInfo = {
  name: string;
  version: string;
  frontend: string;
};

export type InitializeResult = {
  protocol_version: string;
  server: { name: string; version: string };
  capabilities: {
    permission_requests?: boolean;
    session_locking?: boolean;
    ask_requests?: boolean;
  };
};

export type ModelInput = "text" | "image";

export type ModelPricing = {
  input: number;
  output: number;
  cache_read?: number;
  cache_write?: number;
};

export type ModelInfo = {
  id: string;
  display_name: string;
  thinking_levels?: string[];
  context_window?: number;
  max_output_tokens?: number;
  pricing?: ModelPricing;
  inputs?: ModelInput[];
  remote_compaction: boolean;
};

export type ConfigGetResult = {
  models: ModelInfo[];
  default_model?: string;
  agents: { id: string; display_name: string; description?: string }[];
  default_agent?: string;
  cwd: string;
  /** `[models.fusion]` 默认组合的规范 selector；双侧都配置时才有值。 */
  fusion?: { lead: string; sidekick: string };
};

// ---------- config/manage ----------

export type ConfigScope = "global" | "project";

export type ConfigAction =
  | { action: "read"; scope: ConfigScope; cwd: string }
  | { action: "save"; scope: ConfigScope; cwd: string; document: unknown; revision: string }
  | { action: "models"; id: string; provider: unknown }
  | { action: "maintenance"; apply: boolean };

export type ChoiceOption = {
  value: string;
  label: string;
};

export type FieldKind =
  | { type: "bool" }
  | { type: "integer" }
  | { type: "number" }
  | { type: "text" }
  | { type: "secret" }
  | { type: "multiline" }
  | { type: "strings" }
  | { type: "object" }
  | { type: "choice"; options: ChoiceOption[] };

export type ConfigField = {
  path: string[];
  tab: string;
  label: string;
  description: string;
  kind: FieldKind;
  advanced: boolean;
};

export type SettingsDocument = {
  path: string;
  revision: string;
  document: Record<string, unknown>;
  effective: Record<string, unknown>;
  inherited: Record<string, unknown>;
  defaults: Record<string, unknown>;
  fields: ConfigField[];
  provider_template: unknown;
  model_template: unknown;
};

export type CatalogModel = {
  id: string;
  name: string;
};

export type MaintenanceReport = {
  removed: string[];
  protected: number;
  bytes: number;
  applied: boolean;
};

export type ConfigReply =
  | { kind: "settings"; data: SettingsDocument }
  | { kind: "models"; data: CatalogModel[] }
  | { kind: "maintenance"; data: MaintenanceReport }
  | { kind: "updated"; data: string };

// ---------- 会话 ----------

/** 会话引擎：模型与工具的交互形态。跑过首条提示后锁定（`engine_locked`）。 */
export type SessionEngine = "standard" | "ptc" | "clm_experimental";
/** 会话自主程度：哪些确认由系统代答而不打扰用户。 */
export type SessionAutonomy = "default" | "dont_ask" | "unattended";
/** 单次提示的执行模式；只约束这一次 prompt，结束即失效。 */
export type PromptMode = "ask";
export type SessionPermission = "full" | "workspace" | "workspace_only";

export type SessionLockInfo = {
  pid: number;
  frontend: string;
  process_name?: string;
  acquired_at_ms: number;
};

export type SessionInfo = {
  id: string;
  title: string;
  model: string;
  permission: SessionPermission;
  engine: SessionEngine;
  /** 会话自主程度。 */
  autonomy: SessionAutonomy;
  /** plan 模式是否激活；与引擎正交，可随时进出。 */
  plan_active: boolean;
  /** 引擎是否已锁定：会话跑过第一条用户提示后为真。 */
  engine_locked: boolean;
  parent_session_id?: string;
  forked_from?: string;
  cwd: string;
  created_at_ms: number;
  updated_at_ms: number;
  lock?: SessionLockInfo;
};

export type ToolStatus = "completed" | "error" | "denied" | "cancelled";

export type ToolExecution = {
  tool_call_id: string;
  status: ToolStatus;
  duration_ms?: number;
};

export type BashCommand = {
  command: string;
  exclude_from_context: boolean;
};

export type AgentStatus = "queued" | "running" | "completed" | "stopped" | "failed" | "idle";
export type AgentMessageOutcome = "injected" | "woken" | "failed";

export type AgentMessageMeta = {
  from_id: string;
  from_title: string;
  delivery: AgentMessageOutcome;
};

export type AgentInfo = {
  id: string;
  parent_id: string;
  title: string;
  model: string;
  cwd: string;
  status: AgentStatus;
  created_at_ms: number;
  updated_at_ms: number;
  last_message?: string;
};

export type ToolProgressRow = {
  agent_id: string;
  title: string;
  status: AgentStatus;
  elapsed_ms: number;
};

export type ReminderItem = {
  key: string;
  digest: string;
};

export type ReminderStamp = {
  items: ReminderItem[];
};

export type TranscriptEntry = {
  id: string;
  at_ms: number;
  message: Message;
  tool?: ToolExecution;
  bash?: BashCommand;
  agent?: AgentMessageMeta;
  reminder?: ReminderStamp;
};

export type TranscriptChildCall = {
  id: string;
  parent_tool_call_id: string;
  name: string;
  arguments: string;
  result: string;
  status: ToolStatus;
  duration_ms?: number;
};

export type TodoStatus = "pending" | "in_progress" | "completed" | "blocked" | "abandoned";

export type TodoItem = {
  status: TodoStatus;
  task: string;
  reason?: string;
};

export type TodoPhase = {
  phase: string;
  items: TodoItem[];
};

export type PlanStatus = "draft" | "submitted" | "revised" | "superseded" | "approved" | "rejected";
export type PlanAction = "execute" | "execute_compacted" | "execute_new_session" | "reject";

/** 计划批注：行号是计划源码的 1-based 行，`body` 可含 ```suggestion 围栏。 */
export type PlanAnnotation = {
  id: string;
  start_line: number;
  end_line: number;
  quote: string;
  body: string;
};

export type PlanInfo = {
  plan_id: string;
  title: string;
  path: string;
  markdown: string;
  status: PlanStatus;
  /** 审阅版本号，从 1 开始；草稿为 0。 */
  version: number;
  previous_plan_id?: string | null;
  /** core 在为空时省略该字段（`skip_serializing_if = "Vec::is_empty"`）。 */
  annotations?: PlanAnnotation[];
};

export type SessionPlanReviseResult = {
  session_id: string;
  engine: SessionEngine;
  plan_active: boolean;
  /** 由 core 生成的修订提示；前端按普通用户消息发送。 */
  prompt: string;
};

export type SessionPlanDiffResult = {
  session_id: string;
  plan_id: string;
  previous_plan_id?: string | null;
  diff: ToolDiff;
};

export type SessionPlanApproveResult = {
  session_id: string;
  engine: SessionEngine;
  plan_active: boolean;
  auto_prompt?: string;
  plan_markdown?: string;
};

export type SessionListResult = {
  sessions: SessionInfo[];
};

export type SessionLoadResult = {
  session: SessionInfo;
  entries: TranscriptEntry[];
  truncated: boolean;
  todos?: TodoPhase[];
  plans?: PlanInfo[];
  child_calls?: TranscriptChildCall[];
};

export type SessionLoadPageResult = {
  session: SessionInfo;
  entries: TranscriptEntry[];
  has_more: boolean;
  todos?: TodoPhase[];
  plans?: PlanInfo[];
  child_calls?: TranscriptChildCall[];
  content_truncated?: boolean;
  // 最近一次回退截掉、仍可前滚接回的条目（core 进程内存态，重启即失效）。
  redo_entries?: TranscriptEntry[];
};

export type SessionModelOverflowAction = "switch" | "compact" | "ask";

export type ModelSwitchOverflow = {
  tokens: number;
  context_window: number;
  threshold: number;
  target_tokens: number;
};

export type SessionSetModelResult = {
  session: SessionInfo;
  overflow?: ModelSwitchOverflow;
  compaction_error?: string;
};

export type CompactResult = {
  method: string;
  tokens_before: number;
  tokens_after: number;
  full: boolean;
  summary_chars: number;
};

export type ContextUsageResult = {
  context_window: number;
  total_tokens: number;
  sections: { name: string; tokens: number }[];
};

export type ContextContentResult = {
  context_window: number;
  total_tokens: number;
  sections: { name: string; text: string; tokens: number; truncated_bytes: number }[];
};

export type ModelUsageSummary = {
  model: string;
  /** 用量来源：会话自身（缺省省略）、Fusion sidekick 或普通子代理。 */
  source?: "session" | "sidekick" | "subagent";
  calls: number;
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens?: number;
  cache_creation_input_tokens?: number;
  reasoning_tokens?: number;
  active_ms: number;
  ttft_ms: number;
  ttft_calls: number;
  gen_ms: number;
};

export type SessionUsageResult = {
  models: ModelUsageSummary[];
  total: ModelUsageSummary;
  tool_ms: number;
};

// ---------- 提示 ----------

export type InputPart =
  | { type: "text"; text: string }
  | { type: "image"; source: ImageSource; detail?: ImageDetail };

export type PromptResult = {
  session_id: string;
  prompt_id: string;
  message: Message;
  finish_reason: FinishReason;
  usage: Usage;
};

export type BashResult = {
  result: ToolResult;
  status: ToolStatus;
  duration_ms: number;
};

export type SteerResult = {
  accepted: boolean;
  id?: number;
};

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

export type ChartAnalyzeParams = { header: string[]; rows: string[][] };

export type ChartSpec = {
  kind: "Bar" | "Grouped" | "Paired" | "Line" | "Stacked" | "Progress" | "Heatmap" | "Scatter";
  axis: string;
  categories: string[];
  series: ChartSeries[];
  normalized: boolean;
  diverging: boolean;
  notes: Array<string | null>;
};

export type ChartAnalyzeResult = { spec: ChartSpec | null };
// ---------- diff ----------

export type DiffLineKind = "equal" | "delete" | "insert";

export type DiffLine = {
  kind: DiffLineKind;
  old_no?: number;
  new_no?: number;
  text: string;
};

export type DiffHunk = {
  old_start: number;
  new_start: number;
  lines: DiffLine[];
};

export type ToolFileDiff = {
  path: string;
  hunks: DiffHunk[];
};

export type ToolDiff = {
  files: ToolFileDiff[];
  truncated: boolean;
};
export type WorkspaceGitBranchResult = { branch: string | null };

// ---------- 权限与前端反向请求 ----------

export type PermissionOptionKind = "allow_once" | "allow_session" | "reject_once";

export type PermissionOption = {
  id: string;
  label: string;
  kind: PermissionOptionKind;
};

export type PermissionRequestParams = {
  session_id: string;
  prompt_id: string;
  request_id: string;
  tool_call_id: string;
  tool_name: string;
  title: string;
  body?: string;
  diff?: ToolDiff;
  options: PermissionOption[];
};

export type PermissionResult = {
  option_id: string;
};

export type AskOption = {
  label: string;
  description?: string;
  preview?: string;
};

export type AskQuestion = {
  id: string;
  question: string;
  header?: string;
  multi: boolean;
  recommended?: number[];
  options: AskOption[];
};

export type AskParams = {
  session_id: string;
  prompt_id: string;
  request_id: string;
  questions: AskQuestion[];
};

export type AskAnswer = {
  id: string;
  selected: string[];
  custom?: string;
};

export type AskResult = {
  answers: AskAnswer[];
};

// ---------- session/update ----------

export type AssistantDelta =
  | { type: "text"; delta: string }
  | { type: "reasoning"; delta: string }
  | { type: "usage"; usage: Usage };

export type UpstreamFieldMismatch = {
  field: "model" | "effort";
  requested: string;
  reported: string;
};

export type SessionUpdate =
  | { type: "agent_started"; agent: AgentInfo }
  | { type: "agent_status_changed"; agent_id: string; status: AgentStatus; last_message?: string }
  | { type: "agent_message"; meta: AgentMessageMeta; body: string }
  | { type: "tool_progress"; prompt_id: string; tool_call_id: string; rows: ToolProgressRow[] }
  | {
      type: "tool_batch_started";
      prompt_id: string;
      batch_id: string;
      calls: { tool_call_id: string; name: string }[];
    }
  | { type: "prompt_started"; prompt_id: string; model: string; at_ms: number }
  | { type: "assistant_delta"; prompt_id: string; delta: AssistantDelta }
  | { type: "generation_started"; prompt_id: string }
  | { type: "bash_started"; tool_call_id: string; command: BashCommand }
  | {
      type: "tool_started";
      prompt_id: string;
      tool_call_id: string;
      name: string;
      parent_tool_call_id?: string;
      arguments: string;
    }
  | {
      type: "tool_finished";
      prompt_id: string;
      tool_call_id: string;
      name: string;
      parent_tool_call_id?: string;
      arguments: string;
      status: ToolStatus;
      result: ToolResult;
      duration_ms?: number;
      diff?: ToolDiff;
    }
  | { type: "permission_requested"; prompt_id: string; request_id: string; tool_call_id: string }
  | { type: "permission_resolved"; prompt_id: string; request_id: string; option_id: string | null }
  | {
      type: "prompt_finished";
      prompt_id: string;
      message: Message;
      finish_reason: FinishReason;
      usage: Usage;
    }
  | { type: "guard_notice"; prompt_id: string; message: string; terminal: boolean }
  | { type: "plugin_notice"; plugin: string; code: string; message: string }
  | { type: "title_changed"; title: string }
  | { type: "lock_changed"; lock: SessionLockInfo | null }
  | { type: "permission_changed"; permission: SessionPermission }
  | { type: "engine_changed"; engine: SessionEngine }
  | { type: "autonomy_changed"; autonomy: SessionAutonomy }
  | { type: "plan_mode_changed"; active: boolean }
  | { type: "model_changed"; model: string }
  | { type: "todo_changed"; phases: TodoPhase[] }
  | { type: "todo_reminder"; prompt_id: string; open_items: number }
  | { type: "plan_submitted"; plan: PlanInfo }
  | { type: "plan_updated"; plan: PlanInfo }
  | { type: "context_compaction_started" }
  | {
      type: "context_compacted";
      method: string;
      tokens_before: number;
      tokens_after: number;
      full: boolean;
    }
  | { type: "context_compaction_failed"; message: string }
  | {
      type: "job_finished";
      job_id: string;
      command: string;
      exit_code: number | null;
      log_path: string;
    }
  | { type: "workspace_changed"; violation: boolean; changes: { path: string; kind: string }[] }
  | { type: "upstream_mismatch"; prompt_id: string; fields: UpstreamFieldMismatch[] }
  | { type: "steering_applied"; id: number; text: string }
  | { type: "error"; prompt_id: string; message: string; retryable: boolean }
  | { type: "prompt_rewound"; replayed_tools: boolean };

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
  todo_changed: true,
  todo_reminder: true,
  plan_submitted: true,
  plan_updated: true,
  context_compaction_started: true,
  context_compacted: true,
  context_compaction_failed: true,
  job_finished: true,
  workspace_changed: true,
  upstream_mismatch: true,
  steering_applied: true,
  error: true,
  prompt_rewound: true,
};

export type SessionUpdateParams = {
  session_id: string;
  update: SessionUpdate;
};

// ---------- btw ----------

export type BtwExchange = {
  question: string;
  answer: string;
};

export type BtwUpdate =
  | { type: "started" }
  | { type: "waiting" }
  | { type: "generating" }
  | { type: "text_delta"; text: string }
  | { type: "reasoning_delta"; text: string };

export const BTW_UPDATE_TYPES: Record<BtwUpdate["type"], true> = {
  started: true,
  waiting: true,
  generating: true,
  text_delta: true,
  reasoning_delta: true,
};

export type BtwUpdateParams = {
  session_id: string;
  request_id: number;
  update: BtwUpdate;
};

export type BtwResult = {
  session_id: string;
  request_id: number;
  model: string;
  text: string;
  finish_reason: FinishReason;
  usage: Usage;
  history_turns_used: number;
};

export const MAX_BTW_HISTORY_TURNS = 20;

// ---------- 插件 / 资源 / 代理 / fs ----------

export type PluginInfo = {
  name: string;
  version?: string | null;
  description?: string | null;
  source: string;
  dir: string;
  tools: { name: string; description: string; modes: string[] }[];
  commands: { name: string; description: string; usage: string }[];
  error?: string | null;
};

export type PluginListResult = {
  plugins: PluginInfo[];
};

export type PluginRunCommandResult = {
  /** 插件返回的文本；前端按普通输出展示。 */
  text: string;
};

export type ResourceCandidate = {
  uri: string;
  label: string;
  description: string;
  kind: string;
  append_space: boolean;
};

export type AgentListResult = {
  agents: AgentInfo[];
};

export type FsDirEntry = {
  name: string;
  path: string;
  git: boolean;
};

export type FsListDirResult = {
  path: string;
  parent?: string;
  home?: string;
  entries: FsDirEntry[];
  truncated: boolean;
};

export const FS_LIST_DIR_MAX_ENTRIES = 1000;

export type FsCreateDirResult = {
  path: string;
};

/** FinishReason 的展示用小写键。 */
export function finishReasonKey(reason: FinishReason): string {
  return typeof reason === "string" ? reason : reason.Other;
}
