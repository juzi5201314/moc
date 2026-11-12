// Code generated from moc-protocol/protocol.schema.json; DO NOT EDIT.
// schema sha256: 2869597396f020ef
// 重生成：pnpm gen:protocol（Rust 侧先 MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol）。
// biome-ignore-all lint/complexity/noBannedTypes: 空 params 的 schema 没有字段，json2ts 生成空类型
// biome-ignore-all lint/suspicious/noEmptyInterface: 空 params 的 schema 没有字段，json2ts 生成空接口

/**
 * 子代理的运行状态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentStatus".
 */
export type AgentStatus = "queued" | "running" | "completed" | "stopped" | "failed" | "idle";
/**
 * 一次代理间消息投递的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentMessageOutcome".
 */
export type AgentMessageOutcome = "injected" | "woken" | "failed";
/**
 * 结束原因。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FinishReason".
 */
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
  | {
      Other: string;
    };
/**
 * 消息内容块。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ContentPart".
 */
export type ContentPart =
  | {
      Text: string;
    }
  | {
      ResponsePhase: string;
    }
  | {
      Image: {
        /**
         * 精度提示，仅 Chat / Responses 支持。
         */
        detail?: ImageDetail | null;
        /**
         * 图片来源。
         */
        source:
          | {
              Url: string;
            }
          | {
              Base64: {
                /**
                 * 不含 `data:` 前缀的 base64 载荷。
                 */
                data: string;
                /**
                 * MIME 类型，如 `image/png`。
                 */
                media_type: string;
              };
            };
      };
    }
  | {
      Reasoning: ReasoningBlock;
    }
  | {
      ToolCall: ToolCall;
    }
  | {
      ToolResult: ToolResult;
    };
/**
 * 图片精度提示。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ImageDetail".
 */
export type ImageDetail = "auto" | "low" | "high";
/**
 * 思维链块。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ReasoningBlock".
 */
export type ReasoningBlock =
  | {
      Text: {
        /**
         * Anthropic 的签名；其余协议为 `None`。
         */
        signature?: string | null;
        /**
         * 思维链文本。
         */
        text: string;
      };
    }
  | {
      Redacted: {
        /**
         * 上游返回的不透明数据。
         */
        data: string;
      };
    }
  | {
      Item: ReasoningItem;
    };
/**
 * 图片来源。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ImageSource".
 */
export type ImageSource =
  | {
      Url: string;
    }
  | {
      Base64: {
        /**
         * 不含 `data:` 前缀的 base64 载荷。
         */
        data: string;
        /**
         * MIME 类型，如 `image/png`。
         */
        media_type: string;
      };
    };
/**
 * assistant 增量。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AssistantDelta".
 */
export type AssistantDelta =
  | {
      /**
       * 增量文本。
       */
      delta: string;
      type: "text";
    }
  | {
      /**
       * 增量文本。
       */
      delta: string;
      type: "reasoning";
    }
  | {
      type: "usage";
      usage: Usage;
    };
/**
 * 工具执行终态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolStatus".
 */
export type ToolStatus = "completed" | "error" | "denied" | "cancelled";
/**
 * 旁路生成的一步。
 *
 * 与 [`super::update::SessionUpdate`] 一样是会增长的枚举：前端遇到不认识的
 * `type` 必须整条丢弃，不能报错或断开连接。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwUpdate".
 */
export type BtwUpdate =
  | {
      type: "started";
    }
  | {
      type: "waiting";
    }
  | {
      type: "generating";
    }
  | {
      text: string;
      type: "text_delta";
    }
  | {
      text: string;
      type: "reasoning_delta";
    };
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ConfigAction".
 */
export type ConfigAction =
  | {
      action: "read";
      cwd: string;
      scope: ConfigScope;
    }
  | {
      action: "save";
      cwd: string;
      document: unknown;
      revision: string;
      scope: ConfigScope;
    }
  | {
      action: "models";
      id: string;
      provider: unknown;
    }
  | {
      action: "select_model";
      cwd: string;
      selector: string;
    }
  | {
      action: "maintenance";
      apply: boolean;
    };
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ConfigScope".
 */
export type ConfigScope = "global" | "project";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FieldKind".
 */
export type FieldKind =
  | {
      type: "bool";
    }
  | {
      type: "integer";
    }
  | {
      type: "number";
    }
  | {
      type: "text";
    }
  | {
      type: "secret";
    }
  | {
      type: "multiline";
    }
  | {
      type: "strings";
    }
  | {
      type: "object";
    }
  | {
      options: ChoiceOption[];
      type: "choice";
    };
/**
 * 模型接受的输入模态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ModelInput".
 */
export type ModelInput = "text" | "image";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ConfigReply".
 */
export type ConfigReply =
  | {
      data: SettingsDocument;
      kind: "settings";
    }
  | {
      data: CatalogModel[];
      kind: "models";
    }
  | {
      data: MaintenanceReport;
      kind: "maintenance";
    }
  | {
      data: string;
      kind: "updated";
    };
/**
 * [`DiffLine`] 的类别。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "DiffLineKind".
 */
export type DiffLineKind = "equal" | "delete" | "insert";
/**
 * 目录项类型。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsEntryKind".
 */
export type FsEntryKind = "dir" | "file";
/**
 * 文件内容编码判定。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsFileEncoding".
 */
export type FsFileEncoding = "utf8" | "binary";
/**
 * `session/goal` 的动作。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "GoalAction".
 */
export type GoalAction =
  | {
      action: "show";
    }
  | {
      action: "create";
      objective: string;
    }
  | {
      action: "edit";
      objective: string;
    }
  | {
      action: "pause";
    }
  | {
      action: "resume";
    }
  | {
      action: "clear";
    };
/**
 * 目标的生命周期相位。armed/disarmed 是进程内属性，不在此列。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "GoalPhase".
 */
export type GoalPhase = "active" | "paused" | "blocked" | "complete";
/**
 * 一段用户输入。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "InputPart".
 */
export type InputPart =
  | {
      /**
       * 正文。
       */
      text: string;
      type: "text";
    }
  | {
      /**
       * 精度提示；`None` 交给上游默认。
       */
      detail?: ImageDetail | null;
      /**
       * 图片来源。
       */
      source:
        | {
            Url: string;
          }
        | {
            Base64: {
              /**
               * 不含 `data:` 前缀的 base64 载荷。
               */
              data: string;
              /**
               * MIME 类型，如 `image/png`。
               */
              media_type: string;
            };
          };
      type: "image";
    };
/**
 * 任务生命周期：monitored 会自然结束并回报；persistent 是常驻服务，只能被终止。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobLifecycle".
 */
export type JobLifecycle = "monitored" | "persistent";
/**
 * 任务从哪条路径进入（或尚未进入）后台。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobOrigin".
 */
export type JobOrigin = "foreground" | "explicit" | "auto_background" | "user_background";
/**
 * 任务运行状态。`exited` 时退出码见 [`JobInfo::exit_code`]。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobStatus".
 */
export type JobStatus = ("running" | "exited" | "killed" | "timed_out") | "killing";
/**
 * 选项语义分类。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PermissionOptionKind".
 */
export type PermissionOptionKind = "allow_once" | "allow_session" | "reject_once";
/**
 * 用户对计划的处理动作。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PlanAction".
 */
export type PlanAction = "execute" | "execute_compacted" | "execute_new_session" | "reject";
/**
 * 计划生命周期状态。
 *
 * 一次计划审阅的典型轨迹：`plan` 反复起草/修订（`draft`）→ `exit_plan`
 * 提交（`submitted`）→ 用户提交批注（`revised`）→ 模型写出新版后旧版
 * （`superseded`）。`approved` / `rejected` 是终态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PlanStatus".
 */
export type PlanStatus =
  | ("approved" | "rejected")
  | "draft"
  | "submitted"
  | "revised"
  | "superseded";
/**
 * 单次提示的执行模式：只约束这一次 `session/prompt`，提示结束即失效。
 *
 * 模式与 [`super::session::SessionEngine`] 引擎是两个正交轴：引擎决定交互
 * 形态且会话级锁定，模式只收紧本次提示的执行许可，不落库、不推送变更事件。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PromptMode".
 */
export type PromptMode = "ask";
/**
 * 消息角色。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "Role".
 */
export type Role = "System" | "Developer" | "User" | "Assistant" | "Tool" | "Custom";
/**
 * 会话自主程度：哪些确认由系统代答而不打扰用户。
 *
 * 档位从低到高单向收紧人工介入；`session/set_autonomy` 随时可改，随会话持久化。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionAutonomy".
 */
export type SessionAutonomy = "default" | "dont_ask" | "unattended";
/**
 * 会话引擎：模型与工具的交互形态。
 *
 * 引擎是会话级属性：`session/new` 或空白期（没有任何用户提示）的
 * `session/set_engine` 决定一次会话的引擎；会话跑过第一条用户提示后
 * [`SessionInfo::engine_locked`] 为真，引擎不能再改。
 *
 * plan 与 ask 不是引擎：plan 是叠加在任意引擎上的持久模式
 * （[`SessionInfo::plan_active`]，`session/plan_mode` 进出），ask 是
 * [`super::prompt::PromptParams::mode`] 声明的单次提示只读模式。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionEngine".
 */
export type SessionEngine = "standard" | "ptc" | "clm_experimental";
/**
 * 块偏移以冻结 JSON 的 UTF-8 字节计；同一偏移可重读，直到 close 或过期。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionHistoryParams".
 */
export type SessionHistoryParams =
  | {
      action: "open";
      page: SessionLoadPageParams;
    }
  | {
      action: "read";
      offset: number;
      transfer_id: string;
    }
  | {
      action: "close";
      transfer_id: string;
    };
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionHistoryResult".
 */
export type SessionHistoryResult =
  | {
      page: SessionLoadPageResult;
      type: "page";
    }
  | {
      data: string;
      next_offset?: number | null;
      offset: number;
      transfer_id: string;
      type: "chunk";
    }
  | {
      type: "closed";
    };
/**
 * Todo 任务状态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "TodoStatus".
 */
export type TodoStatus = "pending" | "in_progress" | "completed" | "blocked" | "abandoned";
/**
 * 目标模型容不下当前上下文时 [`SessionSetModel`] 执行的动作。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionModelOverflowAction".
 */
export type SessionModelOverflowAction = "switch" | "compact" | "ask";
/**
 * 会话命令执行权限。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPermission".
 */
export type SessionPermission = ("full" | "workspace") | "workspace_only";
/**
 * 一条会话更新。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionUpdate".
 */
export type SessionUpdate =
  | {
      agent: AgentInfo;
      type: "agent_started";
    }
  | {
      agent_id: string;
      last_message?: string | null;
      status: AgentStatus;
      type: "agent_status_changed";
    }
  | {
      /**
       * 消息正文。
       */
      body: string;
      meta: AgentMessageMeta1;
      type: "agent_message";
    }
  | {
      /**
       * 提示 ID。
       */
      prompt_id: string;
      /**
       * 进度行。
       */
      rows: ToolProgressRow[];
      /**
       * 调用 ID，与 [`SessionUpdate::ToolFinished`] 配对。
       */
      tool_call_id: string;
      type: "tool_progress";
    }
  | {
      /**
       * 批次 ID。
       */
      batch_id: string;
      /**
       * 批次内的调用，保持模型返回顺序。
       */
      calls: ToolBatchCall[];
      /**
       * 提示 ID。
       */
      prompt_id: string;
      type: "tool_batch_started";
    }
  | {
      /**
       * 开始时间的 UNIX 毫秒时间戳。
       */
      at_ms: number;
      /**
       * 实际使用的模型 ID。
       */
      model: string;
      /**
       * prompt ID。
       */
      prompt_id: string;
      type: "prompt_started";
    }
  | {
      /**
       * 增量内容。
       */
      delta:
        | {
            /**
             * 增量文本。
             */
            delta: string;
            type: "text";
          }
        | {
            /**
             * 增量文本。
             */
            delta: string;
            type: "reasoning";
          }
        | {
            type: "usage";
            usage: Usage;
          };
      /**
       * prompt ID。
       */
      prompt_id: string;
      type: "assistant_delta";
    }
  | {
      prompt_id: string;
      type: "generation_started";
    }
  | {
      command: BashCommand;
      tool_call_id: string;
      type: "bash_started";
    }
  | {
      /**
       * 参数 JSON 原文：core 实际执行的那一份，前端直接展示。
       */
      arguments: string;
      /**
       * 工具名。
       */
      name: string;
      /**
       * 父调用 ID；`Some` 表示这是 `execute` 沙盒里发起、以该调用为父的嵌套调用。
       */
      parent_tool_call_id?: string | null;
      /**
       * prompt ID。
       */
      prompt_id: string;
      /**
       * 调用 ID，与后续 [`SessionUpdate::ToolFinished`] 配对。
       */
      tool_call_id: string;
      type: "tool_started";
    }
  | {
      /**
       * 参数 JSON 原文；与 [`SessionUpdate::ToolStarted`] 同源，供前端补建缺席的调用块。
       */
      arguments: string;
      /**
       * 文件变更的结构化 diff；仅 `edit` / `write` 成功时由 core 给出，
       * 授权弹窗与 transcript 的 diff 渲染都以此为准。
       */
      diff?: ToolDiff | null;
      /**
       * 执行耗时毫秒；`None` 表示不计时。
       */
      duration_ms?: number | null;
      /**
       * 工具名；与 [`SessionUpdate::ToolStarted`] 同源，前端补建缺席的调用块时据此命名。
       */
      name: string;
      /**
       * 父调用 ID；与 [`SessionUpdate::ToolStarted`] 同源。
       */
      parent_tool_call_id?: string | null;
      /**
       * prompt ID。
       */
      prompt_id: string;
      result: ToolResult1;
      /**
       * 工具执行终态。
       */
      status: "completed" | "error" | "denied" | "cancelled";
      /**
       * 调用 ID。
       */
      tool_call_id: string;
      type: "tool_finished";
    }
  | {
      /**
       * prompt ID。
       */
      prompt_id: string;
      /**
       * 授权请求 ID。
       */
      request_id: string;
      /**
       * 相关调用 ID。
       */
      tool_call_id: string;
      type: "permission_requested";
    }
  | {
      /**
       * 前端选中的选项；`None` 表示前端没给选择（未注册处理器、选项非法、连接中断），core 按拒绝处理。
       */
      option_id?: string | null;
      /**
       * prompt ID。
       */
      prompt_id: string;
      /**
       * 授权请求 ID。
       */
      request_id: string;
      type: "permission_resolved";
    }
  | {
      /**
       * 结束原因。
       */
      finish_reason:
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
        | {
            Other: string;
          };
      message: Message3;
      /**
       * prompt ID。
       */
      prompt_id: string;
      type: "prompt_finished";
      usage: Usage3;
    }
  | {
      message: string;
      prompt_id: string;
      terminal: boolean;
      type: "guard_notice";
    }
  | {
      /**
       * 稳定错误码（`E_PLUGIN_*`）。
       */
      code: string;
      /**
       * 失败原因。
       */
      message: string;
      /**
       * 插件名。
       */
      plugin: string;
      type: "plugin_notice";
    }
  | {
      /**
       * 新标题。
       */
      title: string;
      type: "title_changed";
    }
  | {
      /**
       * 新的锁投影；`None` 表示锁已释放。
       */
      lock?: SessionLockInfo | null;
      type: "lock_changed";
    }
  | {
      permission: SessionPermission;
      type: "permission_changed";
    }
  | {
      engine: SessionEngine;
      type: "engine_changed";
    }
  | {
      event: WorkflowEvent;
      run_id: string;
      type: "workflow";
    }
  | {
      autonomy: SessionAutonomy;
      type: "autonomy_changed";
    }
  | {
      active: boolean;
      type: "plan_mode_changed";
    }
  | {
      model: string;
      type: "model_changed";
    }
  | {
      enabled: boolean;
      type: "fast_mode_changed";
    }
  | {
      /**
       * 变更后的当前目标投影。
       */
      goal?: GoalView | null;
      type: "goal_changed";
    }
  | {
      /**
       * 写入会话的正文。
       */
      body: string;
      meta: GoalRoundMeta1;
      type: "goal_round_message";
    }
  | {
      phases: TodoPhase[];
      type: "todo_changed";
    }
  | {
      /**
       * 仍未完成的待办数。
       */
      open_items: number;
      /**
       * prompt ID。
       */
      prompt_id: string;
      type: "todo_reminder";
    }
  | {
      plan: PlanInfo;
      type: "plan_submitted";
    }
  | {
      plan: PlanInfo;
      type: "plan_updated";
    }
  | {
      type: "context_compaction_started";
    }
  | {
      /**
       * 本次是否由模型重新生成了摘要；`false` 表示只裁剪了历史（`shake`）。
       */
      full: boolean;
      method: string;
      tokens_after: number;
      tokens_before: number;
      type: "context_compacted";
    }
  | {
      /**
       * 失败原因。
       */
      message: string;
      type: "context_compaction_failed";
    }
  | {
      job: JobInfo;
      type: "job_started";
    }
  | {
      job: JobInfo;
      type: "job_updated";
    }
  | {
      job_id: string;
      /**
       * 最后几行输出（整段替换，不是增量）。
       */
      tail: string;
      tool_call_id: string;
      type: "job_output";
    }
  | {
      job: JobInfo;
      type: "job_finished";
    }
  | {
      changes: PathChange[];
      type: "workspace_changed";
      violation: boolean;
    }
  | {
      /**
       * 每一处不一致；至少一项（空列表没有任何意义，core 不会发送）。
       */
      fields: UpstreamFieldMismatch[];
      /**
       * prompt ID。
       */
      prompt_id: string;
      type: "upstream_mismatch";
    }
  | {
      /**
       * core 分配的 steering ID。
       */
      id: number;
      /**
       * 已写入会话的正文字本；图片消息可能为空。
       */
      text: string;
      type: "steering_applied";
    }
  | {
      message: string;
      prompt_id: string;
      retryable: boolean;
      type: "error";
    }
  | {
      replayed_tools: boolean;
      type: "prompt_rewound";
    };
/**
 * `SessionUpdate::Workflow{run_id, event}` 的事件载荷。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowEvent".
 */
export type WorkflowEvent =
  | {
      kind: "approval_requested";
      request: WorkflowApproval;
    }
  | {
      approved: boolean;
      feedback?: string | null;
      kind: "approval_resolved";
      request_id: string;
    }
  | {
      kind: "run_status";
      status: WorkflowRunStatus;
    }
  | {
      kind: "phase_entered";
      phase: string;
    }
  | {
      kind: "node_updated";
      phases: WorkflowPhaseState[];
      roles: WorkflowRoleState[];
    }
  | {
      kind: "log";
      level: string;
      message: string;
    }
  | {
      artifact_id?: string | null;
      item: unknown;
      kind: "report";
    }
  | {
      artifact: unknown;
      kind: "artifact";
    }
  | {
      kind: "hole_opened";
      name: string;
      ordinal: number;
      prompt: string;
      schema?: unknown;
      site_id: string;
    }
  | {
      kind: "hole_filled";
      name: string;
    }
  | {
      kind: "usage";
      usage: unknown;
    };
/**
 * run 状态（对齐 journal 终态机）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowRunStatus".
 */
export type WorkflowRunStatus =
  | {
      state: "pending";
    }
  | {
      state: "running";
    }
  | {
      state: "completed";
    }
  | {
      state: "errored";
    }
  | {
      /**
       * user | interrupted | provider。
       */
      reason: "user" | "interrupted" | "provider";
      state: "stopped";
    };
/**
 * 审批决策：批准可携带每 role 模型与 ModelRef 重映射覆盖。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowDecision".
 */
export type WorkflowDecision =
  | {
      action: "approve";
      /**
       * `model()` 声明 ref → 模型 selector。
       */
      model_ref_map?: {
        [k: string]: string;
      } | null;
      /**
       * role_id → 模型 selector；未覆盖的 role 用审批窗默认。
       */
      role_models?: {
        [k: string]: string;
      } | null;
    }
  | {
      action: "reject";
      /**
       * 回给模型的拒绝反馈。
       */
      feedback?: string | null;
    };
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SkillsCompactAction".
 */
export type SkillsCompactAction = "missing" | "force" | "cleanup" | "clear";
/**
 * 仅发送到发起连接的临时进度，不属于 session/update，不写入会话。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SkillsProgressPhase".
 */
export type SkillsProgressPhase = "generating" | "committing";
/**
 * 可能被上游静默改写的请求字段。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "UpstreamField".
 */
export type UpstreamField = "model" | "effort";
/**
 * 用量来源：会话自身、Fusion sidekick 子会话，或普通子代理（含其后代）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "UsageSource".
 */
export type UsageSource = "session" | "sidekick" | "subagent";
/**
 * 思考块 / 工具块的显示档位，与 `tui.toml` 的 `thinking_detail`、`tool_detail` 同义。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebDetailLevel".
 */
export type WebDetailLevel = "hidden" | "brief" | "full";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebDiffMode".
 */
export type WebDiffMode = "unified" | "split";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebSubmitAction".
 */
export type WebSubmitAction = "steering" | "interrupt";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebTheme".
 */
export type WebTheme = "system" | "light" | "dark";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowApprovalKind".
 */
export type WorkflowApprovalKind = "launch" | "amend" | "hole_fill";
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowStopReason".
 */
export type WorkflowStopReason = "user" | "interrupted" | "provider";

/**
 * 子代理信息；`id` 直接使用持久化子会话 ID。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentInfo".
 */
export interface AgentInfo {
  created_at_ms: number;
  cwd: string;
  id: string;
  last_message?: string | null;
  model: string;
  parent_id: string;
  /**
   * 创建时固定的角色名；workflow 动态 actor 不注册为全局角色。
   */
  preset?: string | null;
  status: AgentStatus;
  title: string;
  updated_at_ms: number;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentListParams".
 */
export interface AgentListParams {
  /**
   * 调用方会话 ID。
   */
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentListResult".
 */
export interface AgentListResult {
  agents: AgentInfo[];
  roles: AgentRole[];
}
/**
 * 当前调用方可派发的固定角色；定义归配置所有，不向客户端开放覆盖。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentRole".
 */
export interface AgentRole {
  description: string;
  name: string;
}
/**
 * 一条代理间消息的来源元信息；随消息条目持久化。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentMessageMeta".
 */
export interface AgentMessageMeta {
  /**
   * 落库条目只可能是 `Injected` 或 `Woken`。
   */
  delivery: "injected" | "woken" | "failed";
  from_id: string;
  from_title: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentMessageParams".
 */
export interface AgentMessageParams {
  message: string;
  /**
   * 调用方会话 ID。
   */
  session_id: string;
  /**
   * 代理 ID、`"parent"`（仅子代理可用）或 `"all"`（全部运行中的平级代理）。
   */
  target: string;
}
/**
 * 单个目标的投递回执。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentMessageReceipt".
 */
export interface AgentMessageReceipt {
  agent_id: string;
  error?: string | null;
  outcome: AgentMessageOutcome;
  title: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentMessageResult".
 */
export interface AgentMessageResult {
  /**
   * 每个目标一条，按解析顺序。
   */
  receipts: AgentMessageReceipt[];
}
/**
 * 一个可用 agent。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentProfile".
 */
export interface AgentProfile {
  /**
   * 描述；`None` 表示未配置。
   */
  description?: string | null;
  /**
   * 展示名。
   */
  display_name: string;
  /**
   * agent ID。
   */
  id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentSpawnParams".
 */
export interface AgentSpawnParams {
  /**
   * 已注册且启用的固定角色；角色模型、提示词和工具面不接受调用方覆盖。
   */
  role: string;
  /**
   * 调用方会话 ID。
   */
  session_id: string;
  task: string;
  title?: string | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentSpawnResult".
 */
export interface AgentSpawnResult {
  agent: AgentInfo;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentStopParams".
 */
export interface AgentStopParams {
  agent_id: string;
  reason?: string | null;
  /**
   * 调用方会话 ID。
   */
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentStopResult".
 */
export interface AgentStopResult {
  status: AgentStatus;
  stopped: boolean;
}
/**
 * 单个被等待代理的快照。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentWaitEntry".
 */
export interface AgentWaitEntry {
  finish_reason?: FinishReason | null;
  id: string;
  message?: Message | null;
  status: AgentStatus;
  /**
   * workflow/Fusion 契约声明的结构化交付；自由文本角色为 `None`。
   */
  structured_result?: {
    [k: string]: unknown;
  };
  title: string;
}
/**
 * 一条对话消息。
 *
 * 角色与内容块合法组合（编码器在历史完整性校验中强制执行）：
 *
 * | 角色 | 允许的内容块 |
 * |---|---|
 * | `System` / `Developer` | `Text` |
 * | `User` | `Text`、`Image` |
 * | `Assistant` | `Text`、`Reasoning`、`ToolCall` |
 * | `Tool` | 恰好一个 `ToolResult` |
 * | `Custom` | `Text` |
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "Message".
 */
export interface Message {
  /**
   * 内容块，顺序即线上顺序（Anthropic 下与线上块顺序一致）。
   */
  content: ContentPart[];
  /**
   * 消息角色。
   */
  role: "System" | "Developer" | "User" | "Assistant" | "Tool" | "Custom";
}
/**
 * Responses reasoning 的持久化表示。flatten 保持既有会话的 Item JSON 可读，
 * 本地字段独立于 raw，编码请求时只发送 raw，绝不改写 summary 或加密数据。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ReasoningItem".
 */
export interface ReasoningItem {
  /**
   * 仅用于展示的流式文本，终态缺摘要时保留已收到的内容。
   */
  _moc_display_text?: string | null;
  [k: string]: unknown;
}
/**
 * 一次工具调用。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolCall".
 */
export interface ToolCall {
  /**
   * 参数 JSON 字符串，原样保留线上字节（流式增量拼接、回传时逐字节一致）。
   */
  arguments: string;
  /**
   * 调用 ID；工具结果靠它与调用配对。
   */
  id: string;
  /**
   * 工具名。
   */
  name: string;
}
/**
 * 一条工具执行结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolResult".
 */
export interface ToolResult {
  /**
   * 结果文本；多模态结果由 core 转成文本描述后传入。
   */
  content: string;
  /**
   * 结果携带的图片。Anthropic 原生随 tool_result 传递；Chat / Responses 的
   * tool 角色只收文本，编码器会把图片转成紧随其后的 user 消息。
   * 仅允许 [`ImageSource::Base64`]：URL 来源对 tool 结果无意义。
   */
  images?: ImageSource[];
  /**
   * 是否为失败结果（Anthropic 有对应字段，其余协议由 core 拼进文本）。
   */
  is_error: boolean;
  /**
   * 对应的 [`ToolCall::id`]。
   */
  tool_call_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentWaitParams".
 */
export interface AgentWaitParams {
  /**
   * 省略或空 = 全部运行中的直接子代理。
   */
  agent_ids?: string[] | null;
  /**
   * 调用方会话 ID。
   */
  session_id: string;
  /**
   * 无事件窗口；省略用默认 180 秒。
   */
  timeout_secs?: number | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AgentWaitResult".
 */
export interface AgentWaitResult {
  /**
   * 本次等待覆盖的全部代理快照（按解析顺序）。
   */
  agents: AgentWaitEntry[];
  /**
   * 有消息待投递（子代理消息或用户补充消息），将在下一轮输入中出现。
   */
  messages_pending: boolean;
  /**
   * 至少一个代理已进入终态。
   */
  settled: boolean;
  /**
   * 窗口内无任何事件而返回。
   */
  timed_out: boolean;
}
/**
 * 一个问题对应的选择或自定义输入。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AskAnswer".
 */
export interface AskAnswer {
  custom?: string | null;
  id: string;
  selected: string[];
}
/**
 * 一个用户可选答案。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AskOption".
 */
export interface AskOption {
  description?: string | null;
  label: string;
  /**
   * 聚焦该项时展示的预览内容（markdown，等宽框内渲染），用于
   * ASCII 示意图、代码片段等需要肉眼对比的候选；仅单选问题支持。
   */
  preview?: string | null;
}
/**
 * 一次或多次用户提问。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AskParams".
 */
export interface AskParams {
  prompt_id: string;
  questions: AskQuestion[];
  request_id: string;
  session_id: string;
}
/**
 * 一个问题及其候选答案。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AskQuestion".
 */
export interface AskQuestion {
  header?: string | null;
  id: string;
  multi: boolean;
  options: AskOption[];
  question: string;
  /**
   * 模型推荐的选项下标；空集合表示没有推荐项。下标必须落在选项范围内且
   * 不得重复，单选问题最多一个，多选问题可包含多个。
   */
  recommended?: number[];
}
/**
 * 前端为问题返回的答案集合。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "AskResult".
 */
export interface AskResult {
  answers: AskAnswer[];
}
/**
 * 用量。
 */
export interface Usage {
  /**
   * 写入提示缓存的输入 token 数（Anthropic 专有）。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 命中提示缓存的输入 token 数。
   */
  cached_input_tokens?: number | null;
  /**
   * 完整输入 token 数，含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 输出 token 数。
   */
  output_tokens: number;
  /**
   * 思维链 token 数。
   */
  reasoning_tokens?: number | null;
  /**
   * 上游给出的总量；缺失时为 `None`。
   */
  total_tokens?: number | null;
}
/**
 * 命令来源及上下文可见性；随执行结果持久化。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BashCommand".
 */
export interface BashCommand {
  command: string;
  exclude_from_context?: boolean;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BashParams".
 */
export interface BashParams {
  command: BashCommand;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BashResult".
 */
export interface BashResult {
  duration_ms: number;
  result: ToolResult;
  status: ToolStatus;
}
/**
 * `session/btw_cancel` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwCancelParams".
 */
export interface BtwCancelParams {
  /**
   * 要取消的旁路请求；不匹配时是成功的空操作。
   */
  request_id: number;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/btw_cancel` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwCancelResult".
 */
export interface BtwCancelResult {
  /**
   * 是否真的停下了该请求；请求已结束或身份不匹配时为 `false`。
   */
  cancelled: boolean;
}
/**
 * 一轮已完成的旁路问答，由前端回放。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwExchange".
 */
export interface BtwExchange {
  /**
   * 当时的完整回答。
   */
  answer: string;
  /**
   * 用户问题。
   */
  question: string;
}
/**
 * `session/btw` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwParams".
 */
export interface BtwParams {
  /**
   * 最近的有效旁路问答，最多 [`MAX_BTW_HISTORY_TURNS`] 轮。
   */
  history?: BtwExchange[];
  /**
   * 本次问题；只接受纯文本，不展开附件或资源 URI。
   */
  question: string;
  /**
   * 前端生成的请求身份；通知、结果与取消都用它匹配。
   */
  request_id: number;
  /**
   * 目标会话；旁路读取它的当前已提交上下文。
   */
  session_id: string;
}
/**
 * `session/btw` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwResult".
 */
export interface BtwResult {
  /**
   * 结束原因。
   */
  finish_reason:
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
    | {
        Other: string;
      };
  /**
   * 实际回放的历史轮数；预算不足时会少于请求里的轮数。
   */
  history_turns_used: number;
  /**
   * 本次实际使用的模型选择器。
   */
  model: string;
  /**
   * 与请求相同的身份。
   */
  request_id: number;
  /**
   * 会话 ID。
   */
  session_id: string;
  /**
   * 权威回答正文；被取消时是已收到的部分内容。
   */
  text: string;
  usage: Usage1;
}
/**
 * 本次请求的 token 用量；只供前端旁路面板展示，不写入会话统计。
 */
export interface Usage1 {
  /**
   * 写入提示缓存的输入 token 数（Anthropic 专有）。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 命中提示缓存的输入 token 数。
   */
  cached_input_tokens?: number | null;
  /**
   * 完整输入 token 数，含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 输出 token 数。
   */
  output_tokens: number;
  /**
   * 思维链 token 数。
   */
  reasoning_tokens?: number | null;
  /**
   * 上游给出的总量；缺失时为 `None`。
   */
  total_tokens?: number | null;
}
/**
 * `session/btw_update` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "BtwUpdateParams".
 */
export interface BtwUpdateParams {
  /**
   * 与请求相同的身份；旧请求的迟到更新据此丢弃。
   */
  request_id: number;
  /**
   * 会话 ID。
   */
  session_id: string;
  /**
   * 本次增量。
   */
  update:
    | {
        type: "started";
      }
    | {
        type: "waiting";
      }
    | {
        type: "generating";
      }
    | {
        text: string;
        type: "text_delta";
      }
    | {
        text: string;
        type: "reasoning_delta";
      };
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "CatalogModel".
 */
export interface CatalogModel {
  id: string;
  name: string;
}
/**
 * Markdown 表格的表头与行。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ChartAnalyzeParams".
 */
export interface ChartAnalyzeParams {
  header: string[];
  rows: string[][];
}
/**
 * 可绘图时返回 Core `ChartSpec` 的 JSON 表示，不可绘图时为空对象。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ChartAnalyzeResult".
 */
export interface ChartAnalyzeResult {
  spec?: unknown;
}
/**
 * 枚举候选：`value` 是写入配置的原文，`label` 是界面展示的说明。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ChoiceOption".
 */
export interface ChoiceOption {
  label: string;
  value: string;
}
/**
 * 前端能力。
 *
 * `permission_requests = false` 表示这个前端弹不出权限询问（如无交互的 `webui`
 * 后台任务），core 应改走自身策略而不是发反向请求。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ClientCapabilities".
 */
export interface ClientCapabilities {
  /**
   * 能否处理 [`crate::methods::ask::AskParams`] 反向请求。
   */
  ask_requests?: boolean;
  /**
   * 能否处理 [`crate::methods::permission::PermissionRequestParams`]。
   */
  permission_requests?: boolean;
}
/**
 * 前端自报家门。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ClientInfo".
 */
export interface ClientInfo {
  /**
   * 前端形态，约定值 `tui` / `webui` / `desktop`；其它取值自由。
   */
  frontend: string;
  /**
   * 前端程序名，如 `moc-tui`。
   */
  name: string;
  /**
   * 前端版本。
   */
  version: string;
}
/**
 * 一次上下文压缩的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "CompactResult".
 */
export interface CompactResult {
  /**
   * 本次是否由模型重新生成了摘要；`false` 表示只裁剪了历史（`shake`）。
   */
  full: boolean;
  method: string;
  summary_chars: number;
  tokens_after: number;
  tokens_before: number;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ConfigField".
 */
export interface ConfigField {
  advanced: boolean;
  description: string;
  kind: FieldKind;
  label: string;
  /**
   * 路径按段保存，供应商与模型名称中的点号不会被误拆。
   */
  path: string[];
  tab: string;
}
/**
 * `config/get` 的参数：空对象。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ConfigGetParams".
 */
export interface ConfigGetParams {}
/**
 * `config/get` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ConfigGetResult".
 */
export interface ConfigGetResult {
  /**
   * 可用 agent。
   */
  agents: AgentProfile[];
  /**
   * core 的当前工作目录（`session/new` 不带 `cwd` 时用它）。
   */
  cwd: string;
  /**
   * 默认 agent ID。
   */
  default_agent?: string | null;
  /**
   * 最近主动选择且在当前配置可用的完整模型 selector；供新会话使用。
   */
  default_model?: string | null;
  /**
   * 特性开关快照（`[features]` 覆盖合成后的有效状态）；旧报文没有时按注册表默认。
   */
  features?: {
    [k: string]: boolean;
  };
  /**
   * `[models.fusion]` 配置的组合；两侧都配置时才有值。
   */
  fusion?: FusionPair | null;
  /**
   * 可用模型。
   */
  models: ModelInfo[];
}
/**
 * Fusion 组合的两侧模型，均为规范 `供应商/模型:等级` selector。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FusionPair".
 */
export interface FusionPair {
  /**
   * 主导模型：规划、调查、评审与对话。
   */
  lead: string;
  /**
   * 执行模型：实现、构建与测试。
   */
  sidekick: string;
}
/**
 * 一个可用模型。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ModelInfo".
 */
export interface ModelInfo {
  /**
   * 上下文窗口 token 数；`None` 表示未配置。
   */
  context_window?: number | null;
  /**
   * 展示名。
   */
  display_name: string;
  /**
   * 是否支持 Fast Mode（模型配置 `fast_mode = true`）；前端只在支持时展示开关。
   */
  fast_mode?: boolean;
  /**
   * 模型 ID，`session/new` 的 `model` 用它。
   */
  id: string;
  /**
   * 模型接受的输入模态；空表示未声明（按纯文本处理）。
   */
  inputs?: ModelInput[];
  /**
   * 单次输出上限；`None` 表示未配置。
   */
  max_output_tokens?: number | null;
  /**
   * 每百万 token 定价（美元）；`None` 表示未配置，前端应隐藏费用段。
   */
  pricing?: ModelPricing | null;
  /**
   * 模型允许选择的思考等级；`None` 表示前端使用默认集合。
   */
  thinking_levels?: string[] | null;
}
/**
 * 每百万 token 定价（美元）。
 *
 * `input` / `output` 必填；缓存两项选填，未配置时前端可按全价估算或跳过。
 * 注意 moc-llm 的 `Usage::input_tokens` 已折入缓存命中部分，费用应按
 * `input - cached` 计非命中输入、`cached` 按 `cache_read` 单价计。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ModelPricing".
 */
export interface ModelPricing {
  /**
   * 缓存命中输入，每百万 token；`None` 表示未配置。
   */
  cache_read?: number | null;
  /**
   * 缓存写入输入，每百万 token；`None` 表示未配置。
   */
  cache_write?: number | null;
  /**
   * 非缓存输入，每百万 token。
   */
  input: number;
  /**
   * 输出，每百万 token。
   */
  output: number;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SettingsDocument".
 */
export interface SettingsDocument {
  defaults: unknown;
  /**
   * 目标文件的显式覆盖项，保存时只提交此对象，不回填所有默认值。
   */
  document: {
    [k: string]: unknown;
  };
  effective: unknown;
  fields: ConfigField[];
  inherited: unknown;
  model_template: unknown;
  path: string;
  provider_template: unknown;
  revision: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "MaintenanceReport".
 */
export interface MaintenanceReport {
  applied: boolean;
  bytes: number;
  protected: number;
  removed: string[];
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ContextContentResult".
 */
export interface ContextContentResult {
  context_window: number;
  /**
   * 当前状态无法编码成模型请求时的原因。
   */
  error?: string | null;
  model: string;
  next_cursor?: number | null;
  sections: ContextContentSection[];
  total_tokens: number;
  view_id: string;
}
/**
 * 冻结上下文中的分类或来源条目；目录与隐藏项不携带正文。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ContextContentSection".
 */
export interface ContextContentSection {
  children: number;
  /**
   * 隐藏原因；有值时不可读取正文或子节点，仍计入上下文用量。
   */
  hidden?: string | null;
  id: string;
  /**
   * `category`、`markdown` 或 `json`；不暴露编码协议的字段树。
   */
  kind: string;
  name: string;
  source?: string | null;
  tokens: number;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ContextReadResult".
 */
export interface ContextReadResult {
  children: ContextContentSection[];
  next_cursor?: number | null;
  node_id: string;
  text?: string | null;
  view_id: string;
}
/**
 * 当前会话上下文估算结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ContextUsageResult".
 */
export interface ContextUsageResult {
  /**
   * 模型上下文窗口上限。
   */
  context_window: number;
  /**
   * 按来源分类：`system`、`AGENTS.md (p+g)`、`skills`、`tools`、`messages`；
   * 插件贡献提示词时额外显示 `plugins`。运行时提醒与工具结果归入 `messages`。
   */
  sections: ContextUsageSection[];
  /**
   * 所有组成部分合计。
   */
  total_tokens: number;
}
/**
 * 一项上下文组成部分。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ContextUsageSection".
 */
export interface ContextUsageSection {
  /**
   * 展示名称，直接显示在上下文占用窗的分块列表里。
   */
  name: string;
  /**
   * 估算 token 数。
   */
  tokens: number;
}
/**
 * 一段连续变更：变更行 + 两侧上下文行。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "DiffHunk".
 */
export interface DiffHunk {
  /**
   * 该 hunk 的行序列。
   */
  lines: DiffLine[];
  /**
   * 上下文前的新文件起始行（1-based；hunk 无 insert/equal 行时等于删除点）。
   */
  new_start: number;
  /**
   * 上下文前的旧文件起始行（1-based；hunk 无 delete/equal 行时等于插入点）。
   */
  old_start: number;
}
/**
 * diff 中的一行。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "DiffLine".
 */
export interface DiffLine {
  /**
   * 行类别。
   */
  kind: "equal" | "delete" | "insert";
  /**
   * 新文件中的行号（1-based）。
   */
  new_no?: number | null;
  /**
   * 旧文件中的行号（1-based）。
   */
  old_no?: number | null;
  /**
   * 行内容（不含行尾换行）。
   */
  text: string;
}
/**
 * 空结果：序列化为 `{}`。
 *
 * 不用 `null` 表达「无结果」：`null` 与「字段缺失」「结构错误」在前端难以区分，
 * 空对象则能直接反序列化成零字段结构体。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "Empty".
 */
export interface Empty {}
/**
 * `ping` 的参数：空对象。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "EmptyParams".
 */
export interface EmptyParams {}
/**
 * `fs/create_dir` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsCreateDirParams".
 */
export interface FsCreateDirParams {
  /**
   * 新目录名：单段名字，不接受路径分隔符与 `..`。
   */
  name: string;
  /**
   * 父目录：绝对路径或以 `~` 开头，必须已存在。
   */
  parent: string;
}
/**
 * `fs/create_dir` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsCreateDirResult".
 */
export interface FsCreateDirResult {
  /**
   * 新目录的绝对路径。
   */
  path: string;
}
/**
 * `fs/create_file` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsCreateFileParams".
 */
export interface FsCreateFileParams {
  /**
   * 新文件名：单段名字，不接受路径分隔符与 `..`。
   */
  name: string;
  /**
   * 父目录：绝对路径或以 `~` 开头，必须已存在。
   */
  parent: string;
}
/**
 * `fs/create_file` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsCreateFileResult".
 */
export interface FsCreateFileResult {
  /**
   * 新文件的绝对路径。
   */
  path: string;
}
/**
 * `fs/delete` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsDeleteParams".
 */
export interface FsDeleteParams {
  /**
   * 绝对路径或以 `~` 开头。
   */
  path: string;
}
/**
 * 一个目录项。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsDirEntry".
 */
export interface FsDirEntry {
  /**
   * 目录下有 `.git`。
   */
  git: boolean;
  /**
   * 条目类型。
   */
  kind: "dir" | "file";
  /**
   * 修改时间（epoch 毫秒）；取不到时为 `None`。
   */
  modified_ms?: number | null;
  /**
   * 条目名。
   */
  name: string;
  /**
   * 绝对路径。
   */
  path: string;
  /**
   * 字节数；仅文件有值。
   */
  size?: number | null;
  /**
   * 本身是符号链接。
   */
  symlink?: boolean;
}
/**
 * `fs/list_dir` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsListDirParams".
 */
export interface FsListDirParams {
  /**
   * 是否同时列出文件（目录在前、文件在后）。
   */
  include_files?: boolean;
  /**
   * 绝对路径或以 `~` 开头；`None` 取 core 的当前目录。
   */
  path?: string | null;
  /**
   * 是否包含以 `.` 开头的目录。
   */
  show_hidden?: boolean;
}
/**
 * `fs/list_dir` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsListDirResult".
 */
export interface FsListDirResult {
  /**
   * 目录项：目录在前、文件在后，各自按名称（不区分大小写）排序。
   */
  entries: FsDirEntry[];
  /**
   * 用户主目录；取不到时为 `None`。
   */
  home?: string | null;
  /**
   * 上级目录；已经是根时为 `None`。
   */
  parent?: string | null;
  /**
   * 规范化后的绝对路径。
   */
  path: string;
  /**
   * 超过 [`FS_LIST_DIR_MAX_ENTRIES`] 被截断。
   */
  truncated: boolean;
}
/**
 * `fs/read_file` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsReadFileParams".
 */
export interface FsReadFileParams {
  /**
   * 最多读取的字节数；缺省 [`FS_READ_FILE_DEFAULT_MAX_BYTES`]，
   * 上限 [`FS_READ_FILE_MAX_BYTES`]。
   */
  max_bytes?: number | null;
  /**
   * 绝对路径或以 `~` 开头。
   */
  path: string;
}
/**
 * `fs/read_file` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsReadFileResult".
 */
export interface FsReadFileResult {
  /**
   * UTF-8 正文；`encoding == "binary"` 时为 `None`。
   */
  content?: string | null;
  /**
   * 内容编码判定。
   */
  encoding: "utf8" | "binary";
  /**
   * 修改时间（epoch 毫秒）；取不到时为 `None`。
   */
  modified_ms?: number | null;
  /**
   * 规范化后的绝对路径。
   */
  path: string;
  /**
   * 文件实际字节数（不受 `max_bytes` 截断影响）。
   */
  size: number;
  /**
   * 文件超过 `max_bytes`，只回了头部（在字符边界截断）。
   */
  truncated: boolean;
}
/**
 * `fs/rename` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsRenameParams".
 */
export interface FsRenameParams {
  /**
   * 源路径：绝对路径或以 `~` 开头。
   */
  from: string;
  /**
   * 目标路径：绝对路径或以 `~` 开头；已存在时报错。
   */
  to: string;
}
/**
 * `fs/rename` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsRenameResult".
 */
export interface FsRenameResult {
  /**
   * 目标的规范化绝对路径。
   */
  path: string;
}
/**
 * `fs/write_file` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsWriteFileParams".
 */
export interface FsWriteFileParams {
  /**
   * 新内容（UTF-8）。
   */
  content: string;
  /**
   * 乐观锁：给定时要求磁盘当前 mtime 相等，否则回 `-32016`。
   */
  expected_modified_ms?: number | null;
  /**
   * 绝对路径或以 `~` 开头；必须是已存在的普通文件。
   */
  path: string;
}
/**
 * `fs/write_file` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "FsWriteFileResult".
 */
export interface FsWriteFileResult {
  /**
   * 写入后的修改时间（epoch 毫秒）。
   */
  modified_ms?: number | null;
  /**
   * 规范化后的绝对路径。
   */
  path: string;
  /**
   * 写入后的字节数。
   */
  size: number;
}
/**
 * 机器可路由 + 人类可读的 blocked 原因。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "GoalBlockReason".
 */
export interface GoalBlockReason {
  /**
   * 稳定分类码（如 `model-reported`），lower-kebab-case。
   */
  code: string;
  /**
   * 给人与模型看的说明。
   */
  message: string;
}
/**
 * 一轮 goal 续跑的落库标记：写进该轮用户消息条目，回放时按它累计
 * [`GoalView::rounds_started`]。与 [`super::agent::AgentMessageMeta`] 同层，
 * 都是「这条用户消息不是人直接敲的」的来源标注。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "GoalRoundMeta".
 */
export interface GoalRoundMeta {
  /**
   * 本轮归属的目标 ID（CAS 身份的一半）。
   */
  goal_id: string;
  /**
   * 预约本轮时的目标 revision；目标被编辑后旧轮即失效。
   */
  revision: number;
  /**
   * 本轮序号（从 1 起；无上限，只作展示与门槛判定）。
   */
  round: number;
}
/**
 * 当前目标投影：持久快照 + 回放计数 + 进程内 armed。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "GoalView".
 */
export interface GoalView {
  /**
   * 本进程是否允许自动续跑；不落库，会话重新加载后恒为 `false`。
   */
  armed: boolean;
  /**
   * 仅 `phase == blocked` 时存在。
   */
  blocked_reason?: GoalBlockReason | null;
  created_at_ms: number;
  /**
   * 目标 ID（同一会话内不复用）。
   */
  id: string;
  /**
   * 目标原文。
   */
  objective: string;
  phase: GoalPhase;
  /**
   * 乐观锁修订号：每次变更 +1；`update_goal` / RPC 写操作按它做 CAS。
   */
  revision: number;
  /**
   * 已落库的 goal round 数；不设上限。
   */
  rounds_started: number;
  updated_at_ms: number;
}
/**
 * `initialize` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "InitializeParams".
 */
export interface InitializeParams {
  capabilities?: ClientCapabilities1;
  client: ClientInfo1;
  /**
   * 前端声明的协议版本，形状 `major.minor`；major 参与协商。
   */
  protocol_version: string;
}
/**
 * 前端能力；缺省视为「无任何能力」（core 不会向它发权限询问）。
 */
export interface ClientCapabilities1 {
  /**
   * 能否处理 [`crate::methods::ask::AskParams`] 反向请求。
   */
  ask_requests?: boolean;
  /**
   * 能否处理 [`crate::methods::permission::PermissionRequestParams`]。
   */
  permission_requests?: boolean;
}
/**
 * 前端自报家门。
 */
export interface ClientInfo1 {
  /**
   * 前端形态，约定值 `tui` / `webui` / `desktop`；其它取值自由。
   */
  frontend: string;
  /**
   * 前端程序名，如 `moc-tui`。
   */
  name: string;
  /**
   * 前端版本。
   */
  version: string;
}
/**
 * `initialize` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "InitializeResult".
 */
export interface InitializeResult {
  capabilities?: ServerCapabilities;
  /**
   * 服务端协议版本；major 与前端不一致时前端必须放弃连接。
   */
  protocol_version: string;
  server: ServerInfo;
  /**
   * 前端空闲时随机展示的一句话提示；反引号片段按代码样式渲染。老 core 不下发时为空。
   */
  tips?: string[];
}
/**
 * 服务端能力；缺省视为「无任何能力」（老 core 不报能力时前端按最小能力渲染）。
 */
export interface ServerCapabilities {
  /**
   * 是否会向前端发起 [`crate::methods::ask::AskParams`]。
   */
  ask_requests?: boolean;
  /**
   * 是否会发起 [`crate::methods::permission::PermissionRequestParams`] 反向请求。
   */
  permission_requests?: boolean;
  /**
   * 是否实现跨进程会话锁语义。
   */
  session_locking?: boolean;
}
/**
 * 服务端自报家门。
 */
export interface ServerInfo {
  /**
   * 程序名，如 `moc-core`。
   */
  name: string;
  /**
   * 版本。
   */
  version: string;
}
/**
 * 一个任务的快照。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobInfo".
 */
export interface JobInfo {
  command: string;
  /**
   * 模型给 bash 调用写的一句话说明。
   */
  description?: string | null;
  /**
   * 结束时的总耗时；运行中为 `None`。
   */
  duration_ms?: number | null;
  /**
   * 仅 `exited` 时可能有值；被信号杀死时为 `None`。
   */
  exit_code?: number | null;
  job_id: string;
  lifecycle: JobLifecycle;
  /**
   * 完整日志的 `artifact://jobs/...` URI。
   */
  log_uri: string;
  origin: JobOrigin;
  /**
   * 输出超过日志上限，中段被丢弃（保留开头与结尾）。
   */
  output_elided: boolean;
  /**
   * monitored 后台任务超过 `tools.bash.stall_after_secs` 没有新输出。
   */
  stalled: boolean;
  started_at_ms: number;
  status: JobStatus;
  /**
   * 发起它的 bash 工具调用；前端用它把任务挂到对应的工具卡片上。
   */
  tool_call_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobListParams".
 */
export interface JobListParams {
  session_id: string;
  /**
   * 为每个未结束的任务附带最后 N 行输出；省略时 `outputs` 为空。
   */
  tail_lines?: number | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobListResult".
 */
export interface JobListResult {
  /**
   * 运行中与最近结束的任务，按 `job_id` 升序。
   */
  jobs: JobInfo[];
  /**
   * `tail_lines` 非空时各未结束任务的末尾输出（`job_id` → 文本），
   * 读取失败的任务不出现。
   */
  outputs?: {
    [k: string]: string;
  };
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobOutputParams".
 */
export interface JobOutputParams {
  job_id: string;
  session_id: string;
  /**
   * 返回最后几行；默认 200，上限 2000。
   */
  tail_lines?: number | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobOutputResult".
 */
export interface JobOutputResult {
  job: JobInfo;
  text: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobResult".
 */
export interface JobResult {
  job: JobInfo;
}
/**
 * `job/kill` / `job/background` 共用的定位参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "JobTarget".
 */
export interface JobTarget {
  job_id: string;
  session_id: string;
}
/**
 * 一次模型切换检查出的容量缺口。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ModelSwitchOverflow".
 */
export interface ModelSwitchOverflow {
  /**
   * 目标模型的上下文窗口。
   */
  context_window: number;
  /**
   * 压缩后的目标占用。
   */
  target_tokens: number;
  /**
   * 目标模型的压缩触发线。
   */
  threshold: number;
  /**
   * 当前上下文按目标模型口径的估算 token 数。
   */
  tokens: number;
}
/**
 * 一个模型在当前会话中的累计用量。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ModelUsageSummary".
 */
export interface ModelUsageSummary {
  /**
   * 每次模型请求从开始到结束的耗时之和，单位为毫秒。
   */
  active_ms: number;
  /**
   * 缓存写入的输入 token；上游不提供时为 `None`。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 缓存命中的输入 token；上游不提供时为 `None`。
   */
  cached_input_tokens?: number | null;
  /**
   * 模型请求次数。
   */
  calls: number;
  /**
   * 首 token 之后的生成总时长，单位为毫秒；只统计上报了首 token 的请求。
   */
  gen_ms?: number;
  /**
   * 输入 token 总数，包含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 模型选择器。
   */
  model: string;
  /**
   * 输出 token 总数。
   */
  output_tokens: number;
  /**
   * 思考 token；上游不提供时为 `None`。
   */
  reasoning_tokens?: number | null;
  /**
   * 这行用量属于会话自身还是后代会话；自身用量序列化时省略。
   */
  source?: "session" | "sidekick" | "subagent";
  /**
   * 上报了首 token 的请求数；平均 TTFT 用它做分母，失败请求与非流式请求不计入。
   */
  ttft_calls?: number;
  /**
   * 首 token 等待（TTFT）总时长，单位为毫秒；只统计上报了首 token 的请求。
   */
  ttft_ms?: number;
}
/**
 * 工作区路径变化。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PathChange".
 */
export interface PathChange {
  kind: string;
  path: string;
}
/**
 * 一个可选答案。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PermissionOption".
 */
export interface PermissionOption {
  /**
   * 选项 ID，由 core 定义（如 `allow_once`）。
   */
  id: string;
  /**
   * 选项语义分类，前端可据此决定默认高亮与颜色。
   */
  kind: "allow_once" | "allow_session" | "reject_once";
  /**
   * 展示文案。
   */
  label: string;
}
/**
 * `session/request_permission` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PermissionRequestParams".
 */
export interface PermissionRequestParams {
  /**
   * 详细内容（命令全文等）；`None` 表示无可展示的细节。
   */
  body?: string | null;
  /**
   * `edit` / `write` 的结构化 diff 预览（工具尚未执行，展示的是拟议变更）；
   * `None` 表示无 diff 可展示。
   *
   * 该字段在 v1.1 加入。字段在旧前端的 `deny_unknown_fields` 下无法解析，
   * 反向请求会失败并被 core 按拒绝处理——core 与前端同仓库同步升级。
   */
  diff?: ToolDiff | null;
  /**
   * 可选答案；前端必须回其中一个 `id`。
   */
  options: PermissionOption[];
  /**
   * prompt ID。
   */
  prompt_id: string;
  /**
   * 本次询问的 ID；与 [`super::update::SessionUpdate::PermissionRequested`] 一致。
   */
  request_id: string;
  /**
   * 会话 ID。
   */
  session_id: string;
  /**
   * 一行标题，如「执行命令：cargo test」。
   */
  title: string;
  /**
   * 需要授权的调用 ID。
   */
  tool_call_id: string;
  /**
   * 工具名。
   */
  tool_name: string;
}
/**
 * 一次文件变更的结构化 diff。
 *
 * 通常只含一个文件；`edit` 同时携带 `move` 时是两个（源 + 目标）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolDiff".
 */
export interface ToolDiff {
  /**
   * 涉及的文件，按变更顺序排列。
   */
  files: ToolFileDiff[];
  /**
   * 变更超出上限被截断时为 `true`；此时 hunks 只覆盖前缀。
   */
  truncated: boolean;
}
/**
 * 单个文件的 diff。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolFileDiff".
 */
export interface ToolFileDiff {
  /**
   * 连续变更段（含上下文行），按文件顺序排列。
   */
  hunks: DiffHunk[];
  /**
   * 目标文件路径（绝对路径）。
   */
  path: string;
}
/**
 * `session/request_permission` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PermissionResult".
 */
export interface PermissionResult {
  /**
   * 前端选中的 `PermissionOption::id`。
   */
  option_id: string;
}
/**
 * `ping` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PingResult".
 */
export interface PingResult {
  /**
   * 服务端当前时间的 UNIX 毫秒时间戳。
   */
  server_time_ms: number;
}
/**
 * 一条计划批注。
 *
 * 批注锚定在某个计划版本的行区间上：`body` 是用户写的整段正文，允许在其中
 * 使用 ```` ```suggestion ```` 围栏直接给出「把这几行替换成……」，语义与
 * GitHub PR suggestion 一致；`quote` 只是用户实际选中的文本，供界面回显。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PlanAnnotation".
 */
export interface PlanAnnotation {
  /**
   * 批注正文。
   */
  body: string;
  /**
   * 锚定区间结束行（1-based，含）。
   */
  end_line: number;
  /**
   * 前端生成的稳定 id。
   */
  id: string;
  /**
   * 用户选中的文本；按块级锚定时为空。
   */
  quote?: string;
  /**
   * 锚定区间起始行（1-based，含）。
   */
  start_line: number;
}
/**
 * 会话当前已知的计划版本。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PlanInfo".
 */
export interface PlanInfo {
  /**
   * 用户针对这一版提交过的批注；未提交过为空。
   */
  annotations?: PlanAnnotation[];
  markdown: string;
  path: string;
  plan_id: string;
  /**
   * 本次修订基于的上一版；首次提交为 `None`。
   */
  previous_plan_id?: string | null;
  status: PlanStatus;
  title: string;
  /**
   * 审阅版本号：从 1 开始，起草中的计划为 0。
   */
  version?: number;
}
/**
 * 计划版本按创建顺序分页；游标是当前计划列表的排他上界，追加版本不移动旧下标。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PlanPageParams".
 */
export interface PlanPageParams {
  before?: number | null;
  /**
   * 0 表示本次不读取计划；非零值受 core 的条数和序列化字节预算限制。
   */
  max_entries: number;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginCommandInfo".
 */
export interface PluginCommandInfo {
  description: string;
  name: string;
  usage?: string;
}
/**
 * 一个插件的可见状态。`error` 非空表示它加载失败或已在本进程内被禁用。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginInfo".
 */
export interface PluginInfo {
  commands?: PluginCommandInfo[];
  description?: string | null;
  dir: string;
  error?: string | null;
  name: string;
  /**
   * 发现来源（项目/用户/配置目录）。
   */
  source: string;
  tools?: PluginToolInfo[];
  version?: string | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginToolInfo".
 */
export interface PluginToolInfo {
  description: string;
  /**
   * 该工具可见的执行模式（`default`/`ask`/`plan`）。
   */
  modes: string[];
  name: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginListParams".
 */
export interface PluginListParams {
  /**
   * 查询哪个工作目录的插件；`None` 表示用会话自己的 cwd。
   */
  session_id?: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginListResult".
 */
export interface PluginListResult {
  plugins: PluginInfo[];
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginReloadParams".
 */
export interface PluginReloadParams {
  /**
   * 重载哪个工作目录的插件；`None` 表示用会话自己的 cwd。
   */
  session_id?: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginReloadResult".
 */
export interface PluginReloadResult {
  /**
   * 重载后的完整插件清单（与 `plugin/list` 相同）。
   */
  plugins: PluginInfo[];
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginRunCommandParams".
 */
export interface PluginRunCommandParams {
  args?: string;
  /**
   * 命令名，不含前导斜杠。
   */
  name: string;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PluginRunCommandResult".
 */
export interface PluginRunCommandResult {
  /**
   * 插件返回的文本；前端按普通输出展示。
   */
  text: string;
}
/**
 * `session/prompt` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PromptParams".
 */
export interface PromptParams {
  /**
   * 本条输入是代理间消息：携带来源元信息，落库为 `Entry::Message.agent`。
   */
  agent_message?: AgentMessageMeta | null;
  /**
   * 本条输入是一轮 goal 续跑：携带来源元信息，落库为 `Entry::Message.goal`。
   * 只由 core 内部的续轮驱动设置，前端不直接传。
   */
  goal_round?: GoalRoundMeta | null;
  /**
   * 本条用户消息的输入；多模态按顺序排列。
   */
  input: InputPart[];
  /**
   * 仅本次提示生效的执行模式；`None` 沿用会话当前模式（plan 模式照常生效）。
   *
   * 供 `/ask` 这类「只让这一条消息只读」的临时意图使用：core 以它收紧本次
   * `session/prompt`（含其全部轮）的工具许可，结束后不持久化、不推送事件。
   */
  mode?: PromptMode | null;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * 一次提示的终态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "PromptResult".
 */
export interface PromptResult {
  /**
   * 结束原因。
   */
  finish_reason:
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
    | {
        Other: string;
      };
  message: Message1;
  /**
   * 提示 ID；由 core 生成，对协议层不透明。
   */
  prompt_id: string;
  /**
   * 会话 ID。
   */
  session_id: string;
  usage: Usage2;
}
/**
 * 一条对话消息。
 *
 * 角色与内容块合法组合（编码器在历史完整性校验中强制执行）：
 *
 * | 角色 | 允许的内容块 |
 * |---|---|
 * | `System` / `Developer` | `Text` |
 * | `User` | `Text`、`Image` |
 * | `Assistant` | `Text`、`Reasoning`、`ToolCall` |
 * | `Tool` | 恰好一个 `ToolResult` |
 * | `Custom` | `Text` |
 */
export interface Message1 {
  /**
   * 内容块，顺序即线上顺序（Anthropic 下与线上块顺序一致）。
   */
  content: ContentPart[];
  /**
   * 消息角色。
   */
  role: "System" | "Developer" | "User" | "Assistant" | "Tool" | "Custom";
}
/**
 * token 用量。
 */
export interface Usage2 {
  /**
   * 写入提示缓存的输入 token 数（Anthropic 专有）。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 命中提示缓存的输入 token 数。
   */
  cached_input_tokens?: number | null;
  /**
   * 完整输入 token 数，含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 输出 token 数。
   */
  output_tokens: number;
  /**
   * 思维链 token 数。
   */
  reasoning_tokens?: number | null;
  /**
   * 上游给出的总量；缺失时为 `None`。
   */
  total_tokens?: number | null;
}
/**
 * 前滚候选列表的独立分页；正文保持完整，超预算的单条候选独占一页。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "RedoPageParams".
 */
export interface RedoPageParams {
  before_entry_id?: string | null;
  max_entries: number;
}
/**
 * 一条运行时提醒里的一条目。
 *
 * 只记摘要不记正文：正文就是消息本身，摘要只用来判断「这个条目是否已经在历史里」。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ReminderItem".
 */
export interface ReminderItem {
  /**
   * 该条目正文的摘要；正文变了才重新注入。
   */
  digest: string;
  /**
   * 稳定条目键，例如 `sandbox`、`mode`、`mcp-index`。
   */
  key: string;
}
/**
 * 挂在一条 `Custom` 消息上的提醒标记。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ReminderStamp".
 */
export interface ReminderStamp {
  /**
   * 本条消息合并了哪些条目。
   */
  items: ReminderItem[];
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceCandidate".
 */
export interface ResourceCandidate {
  append_space: boolean;
  description: string;
  kind: string;
  label: string;
  uri: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceCompleteParams".
 */
export interface ResourceCompleteParams {
  prefix: string;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceCompleteResult".
 */
export interface ResourceCompleteResult {
  candidates: ResourceCandidate[];
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceListEntry".
 */
export interface ResourceListEntry {
  /**
   * 补充说明（如 skill 的 description）。
   */
  description?: string | null;
  kind: FsEntryKind;
  /**
   * 修改时间（epoch 毫秒）。
   */
  modified_ms?: number | null;
  /**
   * 展示名。
   */
  name: string;
  /**
   * 字节数；仅文件有值。
   */
  size?: number | null;
  /**
   * 子项 URI：`kind == "dir"` 可继续 `resource/list`，`"file"` 可 `resource/read`。
   */
  uri: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceListParams".
 */
export interface ResourceListParams {
  session_id: string;
  /**
   * 要列举的 URI；`""` 是根（列出各命名空间入口）。
   */
  uri: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceListResult".
 */
export interface ResourceListResult {
  entries: ResourceListEntry[];
  truncated: boolean;
  /**
   * 回显请求的 URI。
   */
  uri: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceReadParams".
 */
export interface ResourceReadParams {
  /**
   * 最多读取的字节数；缺省与上限同 `fs/read_file`。
   */
  max_bytes?: number | null;
  session_id: string;
  uri: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ResourceReadResult".
 */
export interface ResourceReadResult {
  /**
   * UTF-8 正文；`encoding == "binary"` 时为 `None`。
   */
  content?: string | null;
  /**
   * 内容编码判定。
   */
  encoding: "utf8" | "binary";
  /**
   * 修改时间（epoch 毫秒）；取不到时为 `None`。
   */
  modified_ms?: number | null;
  /**
   * 资源实际字节数（不受 `max_bytes` 截断影响）。
   */
  size: number;
  /**
   * 资源超过 `max_bytes`，只回了头部（在字符边界截断）。
   */
  truncated: boolean;
  /**
   * 回显请求的 URI。
   */
  uri: string;
}
/**
 * 服务端能力。
 *
 * `session_locking = false` 的 core：`SessionInfo.lock` 恒为 `None`，
 * `session/unlock` 返回 [`crate::RpcErrorCode::NotSupported`]。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ServerCapabilities".
 */
export interface ServerCapabilities1 {
  /**
   * 是否会向前端发起 [`crate::methods::ask::AskParams`]。
   */
  ask_requests?: boolean;
  /**
   * 是否会发起 [`crate::methods::permission::PermissionRequestParams`] 反向请求。
   */
  permission_requests?: boolean;
  /**
   * 是否实现跨进程会话锁语义。
   */
  session_locking?: boolean;
}
/**
 * 服务端自报家门。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ServerInfo".
 */
export interface ServerInfo1 {
  /**
   * 程序名，如 `moc-core`。
   */
  name: string;
  /**
   * 版本。
   */
  version: string;
}
/**
 * `session/cancel` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionCancelParams".
 */
export interface SessionCancelParams {
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/cancel` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionCancelResult".
 */
export interface SessionCancelResult {
  /**
   * 是否真的取消了进行中的提示。
   */
  cancelled: boolean;
}
/**
 * `session/compact` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionCompactParams".
 */
export interface SessionCompactParams {
  instructions?: string | null;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionContextCloseParams".
 */
export interface SessionContextCloseParams {
  session_id: string;
  view_id: string;
}
/**
 * 打开当前上下文；指定 view_id 时只读取已冻结视图的后续目录页。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionContextContentParams".
 */
export interface SessionContextContentParams {
  cursor?: number | null;
  limit?: number | null;
  session_id: string;
  view_id?: string | null;
}
/**
 * `session/context` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionContextParams".
 */
export interface SessionContextParams {
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionContextReadParams".
 */
export interface SessionContextReadParams {
  cursor?: number | null;
  limit?: number | null;
  node_id: string;
  session_id: string;
  view_id: string;
}
/**
 * `session/delete` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionDeleteParams".
 */
export interface SessionDeleteParams {
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/goal` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionGoalParams".
 */
export interface SessionGoalParams {
  action: GoalAction;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/goal` 的结果：变更后的当前目标（clear 或无目标时为 `None`）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionGoalResult".
 */
export interface SessionGoalResult {
  goal?: GoalView | null;
}
/**
 * `session/load_page` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionLoadPageParams".
 */
export interface SessionLoadPageParams {
  /**
   * 只返回该条目之前的历史；省略时返回最新一页。
   */
  before_entry_id?: string | null;
  /**
   * 保留完整正文、图片和关联子工具结果，忽略单条投影预算；页预算按父调用组计量。
   * 单个超大条目独占一页；需要跨帧读取时使用 `session/history`。
   */
  full_messages?: boolean;
  /**
   * 返回条目数；core 会限制到安全范围。
   */
  max_entries: number;
  /**
   * 单条消息展示投影的字节预算；不影响 transcript 原文。
   */
  max_message_bytes?: number | null;
  plan_page?: PlanPageParams | null;
  /**
   * 显式指定时前滚候选受分页预算限制；省略保持旧返回形状。
   */
  redo_page?: RedoPageParams | null;
  session_id: string;
}
/**
 * `session/load_page` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionLoadPageResult".
 */
export interface SessionLoadPageResult {
  child_calls?: TranscriptChildCall[];
  content_truncated?: boolean;
  entries: TranscriptEntry[];
  /**
   * 当前会话目标；无目标或旧 core 为 `None`。
   */
  goal?: GoalView | null;
  has_more: boolean;
  plans?: PlanInfo[];
  /**
   * 还有更早计划时，下一次请求使用的排他上界。
   */
  plans_before?: number | null;
  /**
   * 最近一次回退截掉、仍可前滚接回的 transcript 条目；栈顶以外的更早回退不提供。
   */
  redo_entries?: TranscriptEntry[];
  redo_has_more?: boolean;
  redo_tip_entry_id?: string | null;
  session: SessionInfo;
  todos?: TodoPhase[];
}
/**
 * PTC 中由 JavaScript 发起的嵌套工具调用。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "TranscriptChildCall".
 */
export interface TranscriptChildCall {
  arguments: string;
  /**
   * edit/write 的结构化 diff；旧会话日志没有此字段。
   */
  diff?: ToolDiff | null;
  duration_ms?: number | null;
  id: string;
  name: string;
  parent_tool_call_id: string;
  result: string;
  status: ToolStatus;
}
/**
 * 一条 transcript 条目。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "TranscriptEntry".
 */
export interface TranscriptEntry {
  /**
   * 代理间消息的来源元信息；`None` 表示本地输入。
   */
  agent?: AgentMessageMeta | null;
  /**
   * 记录时间的 UNIX 毫秒时间戳。
   */
  at_ms: number;
  /**
   * 用户手动执行的命令；与 agent 工具调用区分，并控制模型可见性。
   */
  bash?: BashCommand | null;
  /**
   * goal 续跑轮的来源元信息；`None` 表示非续跑消息。
   */
  goal_round?: GoalRoundMeta | null;
  /**
   * 条目 ID，由 core 生成。
   */
  id: string;
  message: Message2;
  /**
   * 运行时提醒的来源标记；`None` 表示普通对话消息。
   */
  reminder?: ReminderStamp | null;
  /**
   * 该消息对应的工具执行状态；`None` 表示与工具无关。
   */
  tool?: ToolExecution | null;
}
/**
 * 一条对话消息。
 *
 * 角色与内容块合法组合（编码器在历史完整性校验中强制执行）：
 *
 * | 角色 | 允许的内容块 |
 * |---|---|
 * | `System` / `Developer` | `Text` |
 * | `User` | `Text`、`Image` |
 * | `Assistant` | `Text`、`Reasoning`、`ToolCall` |
 * | `Tool` | 恰好一个 `ToolResult` |
 * | `Custom` | `Text` |
 */
export interface Message2 {
  /**
   * 内容块，顺序即线上顺序（Anthropic 下与线上块顺序一致）。
   */
  content: ContentPart[];
  /**
   * 消息角色。
   */
  role: "System" | "Developer" | "User" | "Assistant" | "Tool" | "Custom";
}
/**
 * 一次工具执行的终态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolExecution".
 */
export interface ToolExecution {
  /**
   * 工具产出的结构化 diff；随 transcript 持久化，历史回放据此显示改动。
   */
  diff?: ToolDiff | null;
  /**
   * 执行耗时毫秒；`None` 表示不计时（拒绝、取消等）。
   */
  duration_ms?: number | null;
  /**
   * 工具执行终态。
   */
  status: "completed" | "error" | "denied" | "cancelled";
  /**
   * 对应的 [`crate::ToolCall::id`]。
   */
  tool_call_id: string;
}
/**
 * 一条会话的元信息。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionInfo".
 */
export interface SessionInfo {
  /**
   * spawn 时选定的 agent preset 名；普通子代理与普通会话为 `None`。
   */
  agent_preset?: string | null;
  /**
   * 当前自主程度。
   */
  autonomy?: "default" | "dont_ask" | "unattended";
  /**
   * 创建时间的 UNIX 毫秒时间戳。
   */
  created_at_ms: number;
  /**
   * 工作目录。
   */
  cwd: string;
  /**
   * 有效委托深度：顶层会话为 0，直接子代理为 1。旧 header 未记录深度的
   * 子代理按 1 处理。
   */
  delegation_depth?: number;
  /**
   * 当前会话引擎。
   */
  engine?: "standard" | "ptc" | "clm_experimental";
  /**
   * 引擎是否已锁定：会话跑过第一条用户提示后为真，此后 `session/set_engine` 会被拒绝。
   */
  engine_locked?: boolean;
  /**
   * Fast Mode 开关；当前模型支持时请求带 `service_tier = "fast"`。
   */
  fast_mode?: boolean;
  /**
   * 消息回退分叉的来源会话 ID。**纯用户侧出身信息**：分叉在模型侧与普通会话
   * 完全一致，模型永远不会看到它。
   */
  forked_from?: string | null;
  /**
   * 会话 ID，由 core 生成（如 `ses_7f3a`）。
   */
  id: string;
  /**
   * 锁投影；`None` 表示当前没有其它进程在该会话里跑提示。
   */
  lock?: SessionLockInfo | null;
  /**
   * 当前模型 ID。
   */
  model: string;
  /**
   * 父会话 ID：只有 `spawn_agent` 出来的子代理会话有，其余会话为 `None`。
   */
  parent_session_id?: string | null;
  /**
   * 当前命令执行权限。
   */
  permission?: ("full" | "workspace") | "workspace_only";
  /**
   * plan 模式是否激活；激活时本次各次提示按计划模式运行。
   */
  plan_active?: boolean;
  /**
   * 标题。
   */
  title: string;
  /**
   * 最近一次更新的 UNIX 毫秒时间戳。
   */
  updated_at_ms: number;
}
/**
 * 会话锁的投影。
 *
 * 只用于**展示**：锁的判定与持有在 core，协议层不参与互斥。前端看到
 * `Some(_)` 时应当把该会话渲染为只读，并提示持有者（进程名与 pid）。
 *
 * 内核文件锁没有安全的跨进程抢占方式，core 不会因为 `unlock` 而清除别人的锁：
 * 要接管会话，请让持有者 `session/unload` 或退出进程。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionLockInfo".
 */
export interface SessionLockInfo {
  /**
   * 上锁时间的 UNIX 毫秒时间戳。
   */
  acquired_at_ms: number;
  /**
   * 持锁前端形态（`tui` / `webui` / `desktop`）。
   */
  frontend: string;
  /**
   * 持锁进程 PID。
   */
  pid: number;
  /**
   * 进程名；`None` 表示 core 未取到。
   */
  process_name?: string | null;
}
/**
 * 一个 todo 阶段。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "TodoPhase".
 */
export interface TodoPhase {
  items: TodoItem[];
  phase: string;
}
/**
 * 一个 todo 任务。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "TodoItem".
 */
export interface TodoItem {
  reason?: string | null;
  status: TodoStatus;
  task: string;
}
/**
 * `session/list` 的参数：空对象。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionListParams".
 */
export interface SessionListParams {}
/**
 * `session/list` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionListResult".
 */
export interface SessionListResult {
  /**
   * 会话列表；core 按 `updated_at_ms` 降序排列。
   */
  sessions: SessionInfo[];
}
/**
 * `session/load` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionLoadParams".
 */
export interface SessionLoadParams {
  /**
   * 最多返回多少条 transcript 条目（从最新往前数）；`None` 表示全部。
   * `Some(0)` 只取会话元信息、todo 和计划，不带消息及子工具结果。
   */
  max_entries?: number | null;
  /**
   * 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
   */
  omit_auxiliary?: boolean;
  /**
   * 只读打开：不取跨进程写租约，写操作会被 core 拒绝。
   *
   * 另一个 core 进程持有写租约时，只有只读打开能成功。
   */
  read_only?: boolean;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/load` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionLoadResult".
 */
export interface SessionLoadResult {
  /**
   * PTC 子工具调用；不进入模型 transcript。
   */
  child_calls?: TranscriptChildCall[];
  /**
   * transcript 条目；`truncated` 为真时只含最新的 `max_entries` 条。
   */
  entries: TranscriptEntry[];
  /**
   * 当前会话目标；无目标或旧 core 为 `None`。
   */
  goal?: GoalView | null;
  /**
   * 当前会话计划；空值兼容旧 core。
   */
  plans?: PlanInfo[];
  session: SessionInfo1;
  /**
   * 当前 todo 阶段；空值兼容旧 core。
   */
  todos?: TodoPhase[];
  /**
   * 是否因 `max_entries` 截断。
   */
  truncated: boolean;
}
/**
 * 一条会话的元信息。
 */
export interface SessionInfo1 {
  /**
   * spawn 时选定的 agent preset 名；普通子代理与普通会话为 `None`。
   */
  agent_preset?: string | null;
  /**
   * 当前自主程度。
   */
  autonomy?: "default" | "dont_ask" | "unattended";
  /**
   * 创建时间的 UNIX 毫秒时间戳。
   */
  created_at_ms: number;
  /**
   * 工作目录。
   */
  cwd: string;
  /**
   * 有效委托深度：顶层会话为 0，直接子代理为 1。旧 header 未记录深度的
   * 子代理按 1 处理。
   */
  delegation_depth?: number;
  /**
   * 当前会话引擎。
   */
  engine?: "standard" | "ptc" | "clm_experimental";
  /**
   * 引擎是否已锁定：会话跑过第一条用户提示后为真，此后 `session/set_engine` 会被拒绝。
   */
  engine_locked?: boolean;
  /**
   * Fast Mode 开关；当前模型支持时请求带 `service_tier = "fast"`。
   */
  fast_mode?: boolean;
  /**
   * 消息回退分叉的来源会话 ID。**纯用户侧出身信息**：分叉在模型侧与普通会话
   * 完全一致，模型永远不会看到它。
   */
  forked_from?: string | null;
  /**
   * 会话 ID，由 core 生成（如 `ses_7f3a`）。
   */
  id: string;
  /**
   * 锁投影；`None` 表示当前没有其它进程在该会话里跑提示。
   */
  lock?: SessionLockInfo | null;
  /**
   * 当前模型 ID。
   */
  model: string;
  /**
   * 父会话 ID：只有 `spawn_agent` 出来的子代理会话有，其余会话为 `None`。
   */
  parent_session_id?: string | null;
  /**
   * 当前命令执行权限。
   */
  permission?: ("full" | "workspace") | "workspace_only";
  /**
   * plan 模式是否激活；激活时本次各次提示按计划模式运行。
   */
  plan_active?: boolean;
  /**
   * 标题。
   */
  title: string;
  /**
   * 最近一次更新的 UNIX 毫秒时间戳。
   */
  updated_at_ms: number;
}
/**
 * `session/new` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionNewParams".
 */
export interface SessionNewParams {
  /**
   * 工作目录；`None` 由 core 取自己的当前目录。
   */
  cwd?: string | null;
  /**
   * 新会话初始引擎；创建后即为会话引擎，跑过首条提示后锁定。
   */
  engine?: "standard" | "ptc" | "clm_experimental";
  /**
   * 新会话初始 Fast Mode 开关（通常沿用上一个会话）。
   */
  fast_mode?: boolean;
  /**
   * 模型 ID；`None` 用 core 的默认模型。
   */
  model?: string | null;
  /**
   * 新会话直接进入 plan 模式。
   */
  plan_active?: boolean;
  /**
   * 标题；`None` 由 core 生成（如取首条用户输入）。
   */
  title?: string | null;
}
/**
 * 计划批准参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanApproveParams".
 */
export interface SessionPlanApproveParams {
  action: PlanAction;
  /**
   * `execute_new_session` 的目标会话引擎；缺省沿用源会话引擎。
   * 其他动作下忽略。
   */
  execution_engine?: SessionEngine | null;
  plan_id: string;
  session_id: string;
}
/**
 * 计划批准后的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanApproveResult".
 */
export interface SessionPlanApproveResult {
  auto_prompt?: string | null;
  /**
   * 会话引擎：模型与工具的交互形态。
   *
   * 引擎是会话级属性：`session/new` 或空白期（没有任何用户提示）的
   * `session/set_engine` 决定一次会话的引擎；会话跑过第一条用户提示后
   * [`SessionInfo::engine_locked`] 为真，引擎不能再改。
   *
   * plan 与 ask 不是引擎：plan 是叠加在任意引擎上的持久模式
   * （[`SessionInfo::plan_active`]，`session/plan_mode` 进出），ask 是
   * [`super::prompt::PromptParams::mode`] 声明的单次提示只读模式。
   */
  engine: "standard" | "ptc" | "clm_experimental";
  /**
   * 批准后 `session_id` 所指会话的 plan 状态。
   */
  plan_active: boolean;
  plan_markdown?: string | null;
  session_id: string;
}
/**
 * 计划版本 diff 参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanDiffParams".
 */
export interface SessionPlanDiffParams {
  plan_id: string;
  session_id: string;
}
/**
 * 计划版本 diff 结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanDiffResult".
 */
export interface SessionPlanDiffResult {
  diff: ToolDiff1;
  plan_id: string;
  /**
   * 作为对比基准的上一版；`None` 表示这是首版。
   */
  previous_plan_id?: string | null;
}
/**
 * 一次文件变更的结构化 diff。
 *
 * 通常只含一个文件；`edit` 同时携带 `move` 时是两个（源 + 目标）。
 */
export interface ToolDiff1 {
  /**
   * 涉及的文件，按变更顺序排列。
   */
  files: ToolFileDiff[];
  /**
   * 变更超出上限被截断时为 `true`；此时 hunks 只覆盖前缀。
   */
  truncated: boolean;
}
/**
 * plan 模式切换参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanModeParams".
 */
export interface SessionPlanModeParams {
  /**
   * `true` 进入 plan，`false` 退出。
   */
  active: boolean;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * 计划修订请求参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanReviseParams".
 */
export interface SessionPlanReviseParams {
  /**
   * 逐条批注；行号锚定在该计划版本的 markdown 上。
   */
  annotations: PlanAnnotation[];
  /**
   * 整体意见；与逐条批注一起发给模型。
   */
  note?: string | null;
  plan_id: string;
  session_id: string;
}
/**
 * 计划修订请求的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionPlanReviseResult".
 */
export interface SessionPlanReviseResult {
  /**
   * 会话引擎：模型与工具的交互形态。
   *
   * 引擎是会话级属性：`session/new` 或空白期（没有任何用户提示）的
   * `session/set_engine` 决定一次会话的引擎；会话跑过第一条用户提示后
   * [`SessionInfo::engine_locked`] 为真，引擎不能再改。
   *
   * plan 与 ask 不是引擎：plan 是叠加在任意引擎上的持久模式
   * （[`SessionInfo::plan_active`]，`session/plan_mode` 进出），ask 是
   * [`super::prompt::PromptParams::mode`] 声明的单次提示只读模式。
   */
  engine: "standard" | "ptc" | "clm_experimental";
  /**
   * 会话 plan 状态；修订要求它保持 `true`。
   */
  plan_active: boolean;
  /**
   * 由 core 生成的修订提示；前端按普通用户消息发送。
   */
  prompt: string;
  session_id: string;
}
/**
 * `session/reask` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionReaskParams".
 */
export interface SessionReaskParams {
  /**
   * 返回的历史条数；`Some(0)` 只取元信息，省略保持完整返回。
   */
  max_entries?: number | null;
  /**
   * 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
   */
  omit_auxiliary?: boolean;
  /**
   * 目标会话。
   */
  session_id: string;
  /**
   * 要重新作答的 `ask` 工具调用 ID。
   */
  tool_call_id: string;
}
/**
 * `session/redo` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionRedoParams".
 */
export interface SessionRedoParams {
  /**
   * 前滚目标条目 ID（含该条目）；只接受最近一次回退截掉的尾部里的条目。
   */
  entry_id: string;
  /**
   * 返回的历史条数；`Some(0)` 只取元信息，省略保持完整返回。
   */
  max_entries?: number | null;
  /**
   * 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
   */
  omit_auxiliary?: boolean;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/rename` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionRenameParams".
 */
export interface SessionRenameParams {
  /**
   * 目标会话。
   */
  session_id: string;
  /**
   * 新标题。
   */
  title: string;
}
/**
 * `session/retry` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionRetryParams".
 */
export interface SessionRetryParams {
  /**
   * 目标会话；必须已经加载且当前没有进行中的提示。
   */
  session_id: string;
}
/**
 * `session/rewind` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionRewindParams".
 */
export interface SessionRewindParams {
  /**
   * 目标 transcript 消息 ID；用户消息本身不保留，会恢复到输入框。
   */
  entry_id: string;
  /**
   * 返回的历史条数；`Some(0)` 用于随后按页重载的界面，省略保持完整返回。
   */
  max_entries?: number | null;
  /**
   * 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
   */
  omit_auxiliary?: boolean;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * 一个会话的命中：取它最近的几条匹配。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSearchHit".
 */
export interface SessionSearchHit {
  /**
   * 会话内命中，按条目时间从新到旧，最多 3 条。
   */
  matches: SessionSearchMatch[];
  session: SessionInfo2;
}
/**
 * 会话内的一条命中。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSearchMatch".
 */
export interface SessionSearchMatch {
  /**
   * 条目时间戳（毫秒）。
   */
  at_ms: number;
  /**
   * 命中条目的 transcript ID，可用于历史定位。
   */
  entry_id: string;
  /**
   * 命中消息的角色（`user`/`assistant` 等线上角色名）。
   */
  role: string;
  /**
   * 命中点前后的摘录（空白已折叠）。
   */
  snippet: string;
}
/**
 * 一条会话的元信息。
 */
export interface SessionInfo2 {
  /**
   * spawn 时选定的 agent preset 名；普通子代理与普通会话为 `None`。
   */
  agent_preset?: string | null;
  /**
   * 当前自主程度。
   */
  autonomy?: "default" | "dont_ask" | "unattended";
  /**
   * 创建时间的 UNIX 毫秒时间戳。
   */
  created_at_ms: number;
  /**
   * 工作目录。
   */
  cwd: string;
  /**
   * 有效委托深度：顶层会话为 0，直接子代理为 1。旧 header 未记录深度的
   * 子代理按 1 处理。
   */
  delegation_depth?: number;
  /**
   * 当前会话引擎。
   */
  engine?: "standard" | "ptc" | "clm_experimental";
  /**
   * 引擎是否已锁定：会话跑过第一条用户提示后为真，此后 `session/set_engine` 会被拒绝。
   */
  engine_locked?: boolean;
  /**
   * Fast Mode 开关；当前模型支持时请求带 `service_tier = "fast"`。
   */
  fast_mode?: boolean;
  /**
   * 消息回退分叉的来源会话 ID。**纯用户侧出身信息**：分叉在模型侧与普通会话
   * 完全一致，模型永远不会看到它。
   */
  forked_from?: string | null;
  /**
   * 会话 ID，由 core 生成（如 `ses_7f3a`）。
   */
  id: string;
  /**
   * 锁投影；`None` 表示当前没有其它进程在该会话里跑提示。
   */
  lock?: SessionLockInfo | null;
  /**
   * 当前模型 ID。
   */
  model: string;
  /**
   * 父会话 ID：只有 `spawn_agent` 出来的子代理会话有，其余会话为 `None`。
   */
  parent_session_id?: string | null;
  /**
   * 当前命令执行权限。
   */
  permission?: ("full" | "workspace") | "workspace_only";
  /**
   * plan 模式是否激活；激活时本次各次提示按计划模式运行。
   */
  plan_active?: boolean;
  /**
   * 标题。
   */
  title: string;
  /**
   * 最近一次更新的 UNIX 毫秒时间戳。
   */
  updated_at_ms: number;
}
/**
 * `session/search` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSearchParams".
 */
export interface SessionSearchParams {
  /**
   * 只搜某个工作目录下的会话；省略时搜全部。
   */
  cwd?: string | null;
  /**
   * 最多返回多少个会话；默认 20，上限 100。
   */
  limit?: number | null;
  /**
   * 搜索词；对消息 `Text` 块做大小写不敏感的子串匹配。
   */
  query: string;
}
/**
 * `session/search` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSearchResult".
 */
export interface SessionSearchResult {
  /**
   * 命中的会话，按 `session/list` 的 `updated_at_ms` 降序。
   */
  matches: SessionSearchHit[];
  /**
   * 命中数到达上限或扫描预算耗尽、更早的会话未扫描时为真。
   */
  truncated?: boolean;
}
/**
 * `session/set_autonomy` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSetAutonomyParams".
 */
export interface SessionSetAutonomyParams {
  autonomy: SessionAutonomy;
  session_id: string;
}
/**
 * `session/set_engine` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSetEngineParams".
 */
export interface SessionSetEngineParams {
  engine: SessionEngine;
  session_id: string;
}
/**
 * `session/set_fast_mode` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSetFastModeParams".
 */
export interface SessionSetFastModeParams {
  enabled: boolean;
  session_id: string;
}
/**
 * `session/set_model` 的参数：按选择器切换会话模型。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSetModelParams".
 */
export interface SessionSetModelParams {
  /**
   * 目标模型容不下当前上下文时执行的动作；缺省 [`SessionModelOverflowAction::Switch`]。
   */
  overflow?: "switch" | "compact" | "ask";
  /**
   * 模型选择器（`config/get` 返回的 `ModelInfo::id`，或 core 接受的别名）。
   */
  selector: string;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/set_model` 的结果。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSetModelResult".
 */
export interface SessionSetModelResult {
  /**
   * 已切换但压缩失败的原因；`None` 表示没压缩或压缩成功。
   */
  compaction_error?: string | null;
  /**
   * 本次检查出的容量缺口；`None` 表示目标模型装得下。
   */
  overflow?: ModelSwitchOverflow | null;
  session: SessionInfo3;
}
/**
 * 一条会话的元信息。
 */
export interface SessionInfo3 {
  /**
   * spawn 时选定的 agent preset 名；普通子代理与普通会话为 `None`。
   */
  agent_preset?: string | null;
  /**
   * 当前自主程度。
   */
  autonomy?: "default" | "dont_ask" | "unattended";
  /**
   * 创建时间的 UNIX 毫秒时间戳。
   */
  created_at_ms: number;
  /**
   * 工作目录。
   */
  cwd: string;
  /**
   * 有效委托深度：顶层会话为 0，直接子代理为 1。旧 header 未记录深度的
   * 子代理按 1 处理。
   */
  delegation_depth?: number;
  /**
   * 当前会话引擎。
   */
  engine?: "standard" | "ptc" | "clm_experimental";
  /**
   * 引擎是否已锁定：会话跑过第一条用户提示后为真，此后 `session/set_engine` 会被拒绝。
   */
  engine_locked?: boolean;
  /**
   * Fast Mode 开关；当前模型支持时请求带 `service_tier = "fast"`。
   */
  fast_mode?: boolean;
  /**
   * 消息回退分叉的来源会话 ID。**纯用户侧出身信息**：分叉在模型侧与普通会话
   * 完全一致，模型永远不会看到它。
   */
  forked_from?: string | null;
  /**
   * 会话 ID，由 core 生成（如 `ses_7f3a`）。
   */
  id: string;
  /**
   * 锁投影；`None` 表示当前没有其它进程在该会话里跑提示。
   */
  lock?: SessionLockInfo | null;
  /**
   * 当前模型 ID。
   */
  model: string;
  /**
   * 父会话 ID：只有 `spawn_agent` 出来的子代理会话有，其余会话为 `None`。
   */
  parent_session_id?: string | null;
  /**
   * 当前命令执行权限。
   */
  permission?: ("full" | "workspace") | "workspace_only";
  /**
   * plan 模式是否激活；激活时本次各次提示按计划模式运行。
   */
  plan_active?: boolean;
  /**
   * 标题。
   */
  title: string;
  /**
   * 最近一次更新的 UNIX 毫秒时间戳。
   */
  updated_at_ms: number;
}
/**
 * `session/set_permission` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionSetPermissionParams".
 */
export interface SessionSetPermissionParams {
  /**
   * 清空本会话已积累的沙盒路径/命令授权；模式变化时 core 也会自动清空。
   */
  clear_grants?: boolean;
  permission: SessionPermission;
  session_id: string;
}
/**
 * `session/tools`：返回当前会话下一轮请求的工具定义。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionToolsParams".
 */
export interface SessionToolsParams {
  session_id: string;
}
/**
 * `session/unload` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionUnloadParams".
 */
export interface SessionUnloadParams {
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * `session/unlock` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionUnlockParams".
 */
export interface SessionUnlockParams {
  /**
   * 兼容字段：旧前端曾用它强夺他人锁。新语义下 core 只释放本进程持有的租约，
   * 传 `true` 也不会跨进程抢占；别人持有时返回 `-32015`。
   */
  force: boolean;
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * 一条代理间消息的来源元信息；随消息条目持久化。
 */
export interface AgentMessageMeta1 {
  /**
   * 落库条目只可能是 `Injected` 或 `Woken`。
   */
  delivery: "injected" | "woken" | "failed";
  from_id: string;
  from_title: string;
}
/**
 * 运行中 `wait_agent` 的进度行（`session/update` 的 `tool_progress` 使用）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolProgressRow".
 */
export interface ToolProgressRow {
  /**
   * 最近一次工具调用的一行摘要（工具名 + 关键参数首行）；仅在代理运行期间提供。
   */
  activity?: string | null;
  agent_id: string;
  /**
   * 自进入当前运行起的毫秒数；非运行状态为 0（渲染时不显示）。
   */
  elapsed_ms: number;
  status: AgentStatus;
  title: string;
}
/**
 * 同一模型响应返回的一批工具调用。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolBatchCall".
 */
export interface ToolBatchCall {
  /**
   * 工具名。
   */
  name: string;
  /**
   * 调用 ID。
   */
  tool_call_id: string;
}
/**
 * 一条工具执行结果。
 */
export interface ToolResult1 {
  /**
   * 结果文本；多模态结果由 core 转成文本描述后传入。
   */
  content: string;
  /**
   * 结果携带的图片。Anthropic 原生随 tool_result 传递；Chat / Responses 的
   * tool 角色只收文本，编码器会把图片转成紧随其后的 user 消息。
   * 仅允许 [`ImageSource::Base64`]：URL 来源对 tool 结果无意义。
   */
  images?: ImageSource[];
  /**
   * 是否为失败结果（Anthropic 有对应字段，其余协议由 core 拼进文本）。
   */
  is_error: boolean;
  /**
   * 对应的 [`ToolCall::id`]。
   */
  tool_call_id: string;
}
/**
 * 一条对话消息。
 *
 * 角色与内容块合法组合（编码器在历史完整性校验中强制执行）：
 *
 * | 角色 | 允许的内容块 |
 * |---|---|
 * | `System` / `Developer` | `Text` |
 * | `User` | `Text`、`Image` |
 * | `Assistant` | `Text`、`Reasoning`、`ToolCall` |
 * | `Tool` | 恰好一个 `ToolResult` |
 * | `Custom` | `Text` |
 */
export interface Message3 {
  /**
   * 内容块，顺序即线上顺序（Anthropic 下与线上块顺序一致）。
   */
  content: ContentPart[];
  /**
   * 消息角色。
   */
  role: "System" | "Developer" | "User" | "Assistant" | "Tool" | "Custom";
}
/**
 * token 用量。
 */
export interface Usage3 {
  /**
   * 写入提示缓存的输入 token 数（Anthropic 专有）。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 命中提示缓存的输入 token 数。
   */
  cached_input_tokens?: number | null;
  /**
   * 完整输入 token 数，含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 输出 token 数。
   */
  output_tokens: number;
  /**
   * 思维链 token 数。
   */
  reasoning_tokens?: number | null;
  /**
   * 上游给出的总量；缺失时为 `None`。
   */
  total_tokens?: number | null;
}
/**
 * 审批请求载荷（`approval_requested` 事件内容；§8）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowApproval".
 */
export interface WorkflowApproval {
  /**
   * 脚本 `args` 值。
   */
  args?: {
    [k: string]: unknown;
  };
  defaults: WorkflowApprovalDefaults;
  /**
   * launch | amend | hole_fill。
   */
  kind: "launch" | "amend" | "hole_fill";
  max_concurrency?: number;
  /**
   * `hole_fill` 时新增（未批准的）world.run 命令列表。
   */
  new_commands?: string[] | null;
  /**
   * 编译产物 `WorkflowPlan`（sites/phases/edges/roles/model_refs/commands/schemas）。
   */
  plan: {
    [k: string]: unknown;
  };
  /**
   * 审批请求 id：`session/workflow_approve` 用同一 id 回传决策。
   */
  request_id: string;
  /**
   * 已分配的 run id（批准后 run 以该 id 启动）。
   */
  run_id: string;
  /**
   * 脚本全文（前端可查看）。
   */
  script: string;
}
/**
 * 默认选择：role_models（脚本字面量模型否则当前会话模型）+ model_ref_map。
 */
export interface WorkflowApprovalDefaults {
  /**
   * `model()` 声明 ref → 默认模型。
   */
  model_ref_map?: {
    [k: string]: string;
  };
  /**
   * role_id → 默认模型。
   */
  role_models?: {
    [k: string]: string;
  };
}
/**
 * phase 节点运行态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowPhaseState".
 */
export interface WorkflowPhaseState {
  /**
   * 该 phase 下已完成的 ask/world 计数（运行中更新）。
   */
  asks_done?: number;
  entered_at_ms?: number | null;
  id: string;
  name: string;
  /**
   * pending | running | done | failed | cancelled。
   */
  status: string;
  world_done?: number;
}
/**
 * role（actor 实例）运行态。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowRoleState".
 */
export interface WorkflowRoleState {
  asks_done?: number;
  asks_in_flight?: number;
  fan_out?: boolean;
  /**
   * 解析后的实际模型 selector。
   */
  model?: string | null;
  name?: string | null;
  role_id: string;
  spent_tokens?: number;
  /**
   * pending | running | done | failed | cancelled。
   */
  status: string;
}
/**
 * 一轮 goal 续跑的落库标记：写进该轮用户消息条目，回放时按它累计
 * [`GoalView::rounds_started`]。与 [`super::agent::AgentMessageMeta`] 同层，
 * 都是「这条用户消息不是人直接敲的」的来源标注。
 */
export interface GoalRoundMeta1 {
  /**
   * 本轮归属的目标 ID（CAS 身份的一半）。
   */
  goal_id: string;
  /**
   * 预约本轮时的目标 revision；目标被编辑后旧轮即失效。
   */
  revision: number;
  /**
   * 本轮序号（从 1 起；无上限，只作展示与门槛判定）。
   */
  round: number;
}
/**
 * 一处「本地请求值 ≠ 上游回显值」。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "UpstreamFieldMismatch".
 */
export interface UpstreamFieldMismatch {
  /**
   * 被改写的字段。
   */
  field: "model" | "effort";
  /**
   * 上游回显的值。
   */
  reported: string;
  /**
   * 本地写入请求的值。
   */
  requested: string;
}
/**
 * `session/update` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionUpdateParams".
 */
export interface SessionUpdateParams {
  /**
   * 会话 ID。
   */
  session_id: string;
  /**
   * 本次更新。
   */
  update:
    | {
        agent: AgentInfo;
        type: "agent_started";
      }
    | {
        agent_id: string;
        last_message?: string | null;
        status: AgentStatus;
        type: "agent_status_changed";
      }
    | {
        /**
         * 消息正文。
         */
        body: string;
        meta: AgentMessageMeta1;
        type: "agent_message";
      }
    | {
        /**
         * 提示 ID。
         */
        prompt_id: string;
        /**
         * 进度行。
         */
        rows: ToolProgressRow[];
        /**
         * 调用 ID，与 [`SessionUpdate::ToolFinished`] 配对。
         */
        tool_call_id: string;
        type: "tool_progress";
      }
    | {
        /**
         * 批次 ID。
         */
        batch_id: string;
        /**
         * 批次内的调用，保持模型返回顺序。
         */
        calls: ToolBatchCall[];
        /**
         * 提示 ID。
         */
        prompt_id: string;
        type: "tool_batch_started";
      }
    | {
        /**
         * 开始时间的 UNIX 毫秒时间戳。
         */
        at_ms: number;
        /**
         * 实际使用的模型 ID。
         */
        model: string;
        /**
         * prompt ID。
         */
        prompt_id: string;
        type: "prompt_started";
      }
    | {
        /**
         * 增量内容。
         */
        delta:
          | {
              /**
               * 增量文本。
               */
              delta: string;
              type: "text";
            }
          | {
              /**
               * 增量文本。
               */
              delta: string;
              type: "reasoning";
            }
          | {
              type: "usage";
              usage: Usage;
            };
        /**
         * prompt ID。
         */
        prompt_id: string;
        type: "assistant_delta";
      }
    | {
        prompt_id: string;
        type: "generation_started";
      }
    | {
        command: BashCommand;
        tool_call_id: string;
        type: "bash_started";
      }
    | {
        /**
         * 参数 JSON 原文：core 实际执行的那一份，前端直接展示。
         */
        arguments: string;
        /**
         * 工具名。
         */
        name: string;
        /**
         * 父调用 ID；`Some` 表示这是 `execute` 沙盒里发起、以该调用为父的嵌套调用。
         */
        parent_tool_call_id?: string | null;
        /**
         * prompt ID。
         */
        prompt_id: string;
        /**
         * 调用 ID，与后续 [`SessionUpdate::ToolFinished`] 配对。
         */
        tool_call_id: string;
        type: "tool_started";
      }
    | {
        /**
         * 参数 JSON 原文；与 [`SessionUpdate::ToolStarted`] 同源，供前端补建缺席的调用块。
         */
        arguments: string;
        /**
         * 文件变更的结构化 diff；仅 `edit` / `write` 成功时由 core 给出，
         * 授权弹窗与 transcript 的 diff 渲染都以此为准。
         */
        diff?: ToolDiff | null;
        /**
         * 执行耗时毫秒；`None` 表示不计时。
         */
        duration_ms?: number | null;
        /**
         * 工具名；与 [`SessionUpdate::ToolStarted`] 同源，前端补建缺席的调用块时据此命名。
         */
        name: string;
        /**
         * 父调用 ID；与 [`SessionUpdate::ToolStarted`] 同源。
         */
        parent_tool_call_id?: string | null;
        /**
         * prompt ID。
         */
        prompt_id: string;
        result: ToolResult1;
        /**
         * 工具执行终态。
         */
        status: "completed" | "error" | "denied" | "cancelled";
        /**
         * 调用 ID。
         */
        tool_call_id: string;
        type: "tool_finished";
      }
    | {
        /**
         * prompt ID。
         */
        prompt_id: string;
        /**
         * 授权请求 ID。
         */
        request_id: string;
        /**
         * 相关调用 ID。
         */
        tool_call_id: string;
        type: "permission_requested";
      }
    | {
        /**
         * 前端选中的选项；`None` 表示前端没给选择（未注册处理器、选项非法、连接中断），core 按拒绝处理。
         */
        option_id?: string | null;
        /**
         * prompt ID。
         */
        prompt_id: string;
        /**
         * 授权请求 ID。
         */
        request_id: string;
        type: "permission_resolved";
      }
    | {
        /**
         * 结束原因。
         */
        finish_reason:
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
          | {
              Other: string;
            };
        message: Message3;
        /**
         * prompt ID。
         */
        prompt_id: string;
        type: "prompt_finished";
        usage: Usage3;
      }
    | {
        message: string;
        prompt_id: string;
        terminal: boolean;
        type: "guard_notice";
      }
    | {
        /**
         * 稳定错误码（`E_PLUGIN_*`）。
         */
        code: string;
        /**
         * 失败原因。
         */
        message: string;
        /**
         * 插件名。
         */
        plugin: string;
        type: "plugin_notice";
      }
    | {
        /**
         * 新标题。
         */
        title: string;
        type: "title_changed";
      }
    | {
        /**
         * 新的锁投影；`None` 表示锁已释放。
         */
        lock?: SessionLockInfo | null;
        type: "lock_changed";
      }
    | {
        permission: SessionPermission;
        type: "permission_changed";
      }
    | {
        engine: SessionEngine;
        type: "engine_changed";
      }
    | {
        event: WorkflowEvent;
        run_id: string;
        type: "workflow";
      }
    | {
        autonomy: SessionAutonomy;
        type: "autonomy_changed";
      }
    | {
        active: boolean;
        type: "plan_mode_changed";
      }
    | {
        model: string;
        type: "model_changed";
      }
    | {
        enabled: boolean;
        type: "fast_mode_changed";
      }
    | {
        /**
         * 变更后的当前目标投影。
         */
        goal?: GoalView | null;
        type: "goal_changed";
      }
    | {
        /**
         * 写入会话的正文。
         */
        body: string;
        meta: GoalRoundMeta1;
        type: "goal_round_message";
      }
    | {
        phases: TodoPhase[];
        type: "todo_changed";
      }
    | {
        /**
         * 仍未完成的待办数。
         */
        open_items: number;
        /**
         * prompt ID。
         */
        prompt_id: string;
        type: "todo_reminder";
      }
    | {
        plan: PlanInfo;
        type: "plan_submitted";
      }
    | {
        plan: PlanInfo;
        type: "plan_updated";
      }
    | {
        type: "context_compaction_started";
      }
    | {
        /**
         * 本次是否由模型重新生成了摘要；`false` 表示只裁剪了历史（`shake`）。
         */
        full: boolean;
        method: string;
        tokens_after: number;
        tokens_before: number;
        type: "context_compacted";
      }
    | {
        /**
         * 失败原因。
         */
        message: string;
        type: "context_compaction_failed";
      }
    | {
        job: JobInfo;
        type: "job_started";
      }
    | {
        job: JobInfo;
        type: "job_updated";
      }
    | {
        job_id: string;
        /**
         * 最后几行输出（整段替换，不是增量）。
         */
        tail: string;
        tool_call_id: string;
        type: "job_output";
      }
    | {
        job: JobInfo;
        type: "job_finished";
      }
    | {
        changes: PathChange[];
        type: "workspace_changed";
        violation: boolean;
      }
    | {
        /**
         * 每一处不一致；至少一项（空列表没有任何意义，core 不会发送）。
         */
        fields: UpstreamFieldMismatch[];
        /**
         * prompt ID。
         */
        prompt_id: string;
        type: "upstream_mismatch";
      }
    | {
        /**
         * core 分配的 steering ID。
         */
        id: number;
        /**
         * 已写入会话的正文字本；图片消息可能为空。
         */
        text: string;
        type: "steering_applied";
      }
    | {
        message: string;
        prompt_id: string;
        retryable: boolean;
        type: "error";
      }
    | {
        replayed_tools: boolean;
        type: "prompt_rewound";
      };
}
/**
 * `session/usage` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionUsageParams".
 */
export interface SessionUsageParams {
  /**
   * 目标会话。
   */
  session_id: string;
}
/**
 * 当前会话的模型用量与全部模型总和
 * （会话自身 + 未被回退孤立的全部后代会话）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionUsageResult".
 */
export interface SessionUsageResult {
  /**
   * 按来源与模型选择器排序的明细（会话自身、sidekick、普通子代理）。
   */
  models: ModelUsageSummary[];
  /**
   * 工具执行耗时之和，单位为毫秒；手动 bash 与嵌套子调用不计入。
   */
  tool_ms?: number;
  total: ModelUsageSummary1;
}
/**
 * 一个模型在当前会话中的累计用量。
 */
export interface ModelUsageSummary1 {
  /**
   * 每次模型请求从开始到结束的耗时之和，单位为毫秒。
   */
  active_ms: number;
  /**
   * 缓存写入的输入 token；上游不提供时为 `None`。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 缓存命中的输入 token；上游不提供时为 `None`。
   */
  cached_input_tokens?: number | null;
  /**
   * 模型请求次数。
   */
  calls: number;
  /**
   * 首 token 之后的生成总时长，单位为毫秒；只统计上报了首 token 的请求。
   */
  gen_ms?: number;
  /**
   * 输入 token 总数，包含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 模型选择器。
   */
  model: string;
  /**
   * 输出 token 总数。
   */
  output_tokens: number;
  /**
   * 思考 token；上游不提供时为 `None`。
   */
  reasoning_tokens?: number | null;
  /**
   * 这行用量属于会话自身还是后代会话；自身用量序列化时省略。
   */
  source?: "session" | "sidekick" | "subagent";
  /**
   * 上报了首 token 的请求数；平均 TTFT 用它做分母，失败请求与非流式请求不计入。
   */
  ttft_calls?: number;
  /**
   * 首 token 等待（TTFT）总时长，单位为毫秒；只统计上报了首 token 的请求。
   */
  ttft_ms?: number;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowActivateParams".
 */
export interface SessionWorkflowActivateParams {
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowApproveParams".
 */
export interface SessionWorkflowApproveParams {
  decision: WorkflowDecision;
  /**
   * 对应 `approval_requested` 载荷里的 request_id。
   */
  request_id: string;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowArtifactParams".
 */
export interface SessionWorkflowArtifactParams {
  artifact_id: string;
  run_id: string;
  session_id: string;
  version?: number | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowRunParams".
 */
export interface SessionWorkflowRunParams {
  run_id: string;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowRunsParams".
 */
export interface SessionWorkflowRunsParams {
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowRunsResult".
 */
export interface SessionWorkflowRunsResult {
  runs?: WorkflowRunSummary[];
}
/**
 * run 摘要（列表项）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowRunSummary".
 */
export interface WorkflowRunSummary {
  created_at_ms?: number | null;
  error?: string | null;
  finished_at_ms?: number | null;
  result?: unknown;
  resumed_from?: string | null;
  run_id: string;
  status: WorkflowRunStatus;
  title?: string | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SessionWorkflowSetRoleModelParams".
 */
export interface SessionWorkflowSetRoleModelParams {
  /**
   * 模型 selector；`inherit` 表示回到解析链默认。
   */
  model: string;
  role_id: string;
  run_id: string;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SkillsCompactParams".
 */
export interface SkillsCompactParams {
  action?: "missing" | "force" | "cleanup" | "clear";
  cwd?: string | null;
  model?: string | null;
  /**
   * 已有会话使用其模型与目录；留空时只使用草稿参数，不创建会话。
   */
  session_id?: string | null;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SkillsCompactResult".
 */
export interface SkillsCompactResult {
  compressed: number;
  /**
   * 只用于临时命令结果，不追加到 transcript。
   */
  message: string;
  removed: number;
  skipped: number;
  tokens_after: number;
  tokens_before: number;
  warnings: string[];
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SkillsProgressParams".
 */
export interface SkillsProgressParams {
  /**
   * 已完成生成并通过校验的数量，尚未发布缓存。
   */
  completed: number;
  phase: SkillsProgressPhase;
  skipped: number;
  total: number;
}
/**
 * Steering 在下一安全工具边界加入当前提示；队列由前端等待前一次提示完成后提交。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SteerParams".
 */
export interface SteerParams {
  input: InputPart[];
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "SteerResult".
 */
export interface SteerResult {
  accepted: boolean;
  /**
   * 接受后由 core 分配的稳定 ID；前端据此撤回仍在 steering 队列里的消息。
   *
   * 旧核心不返回该字段，新前端仍可正常处理；新核心的 accepted 结果总是带 ID。
   */
  id?: number | null;
}
/**
 * 工具定义。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "ToolDef".
 */
export interface ToolDef {
  /**
   * 工具用途描述。
   */
  description?: string | null;
  /**
   * 工具名。
   */
  name: string;
  /**
   * 参数的 JSON Schema 对象。
   */
  parameters: {
    [k: string]: unknown;
  };
  /**
   * 是否启用工具 schema 严格模式（三协议均支持，未指定时不强制开启）。
   */
  strict?: boolean | null;
}
/**
 * 撤回一条尚未被 core 写入会话的 steering 消息。
 *
 * 消息只有在下一轮请求前 drain 时才算真正写入；写入之后撤回返回
 * `removed: false`，前端应保留该消息在转写中的位置。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "UnsteerParams".
 */
export interface UnsteerParams {
  id: number;
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "UnsteerResult".
 */
export interface UnsteerResult {
  removed: boolean;
}
/**
 * token 用量。
 *
 * `input_tokens` 已做跨协议归一化，恒为完整输入量（含缓存命中与缓存写入部分）；
 * 其余字段保持上游原始语义：
 *
 * | 字段 | Chat 来源 | Anthropic 来源 | Responses 来源 |
 * |---|---|---|---|
 * | `input_tokens` | `prompt_tokens` | `input_tokens` + 缓存字段折入 | `input_tokens` |
 * | `output_tokens` | `completion_tokens` | `output_tokens` | `output_tokens` |
 * | `total_tokens` | `total_tokens` | 无对应字段 | `total_tokens` |
 * | `cached_input_tokens` | `prompt_tokens_details.cached_tokens` | `cache_read_input_tokens` | `input_tokens_details.cached_tokens` |
 * | `cache_creation_input_tokens` | 无 | `cache_creation_input_tokens` | 无 |
 * | `reasoning_tokens` | `completion_tokens_details.reasoning_tokens` | `output_tokens_details.thinking_tokens` | `output_tokens_details.reasoning_tokens` |
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "Usage".
 */
export interface Usage4 {
  /**
   * 写入提示缓存的输入 token 数（Anthropic 专有）。
   */
  cache_creation_input_tokens?: number | null;
  /**
   * 命中提示缓存的输入 token 数。
   */
  cached_input_tokens?: number | null;
  /**
   * 完整输入 token 数，含缓存命中与缓存写入部分。
   */
  input_tokens: number;
  /**
   * 输出 token 数。
   */
  output_tokens: number;
  /**
   * 思维链 token 数。
   */
  reasoning_tokens?: number | null;
  /**
   * 上游给出的总量；缺失时为 `None`。
   */
  total_tokens?: number | null;
}
/**
 * `web.toml` 的内容：字段名与 `tui.toml` 的对应项保持同一套词汇。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebState".
 */
export interface WebState {
  /**
   * 会话运行中按 Enter：`steering` 并入当前提示，`interrupt` 先停下再提问。
   */
  active_submit?: "steering" | "interrupt";
  /**
   * 内联 diff 的展示方式。
   */
  diff_mode?: "unified" | "split";
  /**
   * 后台中弹出需要处理的授权/提问时，发一条系统通知。
   */
  notify?: boolean;
  /**
   * 界面主题；`system` 跟随系统明暗。
   */
  theme?: "system" | "light" | "dark";
  /**
   * 思考块的显示档位：隐藏只留标题行，简略显示末尾预览，完整显示全文。
   */
  thinking_detail?: "hidden" | "brief" | "full";
  /**
   * 工具块的显示档位：隐藏只留标题行，简略按工具给少量预览，完整显示全部输出。
   */
  tool_detail?: "hidden" | "brief" | "full";
  /**
   * 侧栏「工作区」里用户手动添加的目录，按添加先后排列。
   */
  workspaces?: string[];
}
/**
 * 一份带版本戳的状态文档。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebStateDocument".
 */
export interface WebStateDocument {
  /**
   * `web.toml` 是否已存在；`false` 时前端可以把旧版 localStorage 数据迁上来。
   */
  exists: boolean;
  /**
   * 文件内容的版本戳；`web/state_save` 必须带上最新读到的值。
   */
  revision: string;
  state: WebState;
}
/**
 * `web/state` 的参数：空对象。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebStateGetParams".
 */
export interface WebStateGetParams {}
/**
 * `web/state_save` 的参数。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WebStateSaveParams".
 */
export interface WebStateSaveParams {
  /**
   * 最近一次 `web/state` 返回的 `revision`；不一致时 core 拒绝写入。
   */
  revision: string;
  state: WebState;
}
/**
 * 简单确认结果（写操作的通用回执）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowAck".
 */
export interface WorkflowAck {
  session_id: string;
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowApprovalDefaults".
 */
export interface WorkflowApprovalDefaults1 {
  /**
   * `model()` 声明 ref → 默认模型。
   */
  model_ref_map?: {
    [k: string]: string;
  };
  /**
   * role_id → 默认模型。
   */
  role_models?: {
    [k: string]: string;
  };
}
/**
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowArtifactResult".
 */
export interface WorkflowArtifactResult {
  artifact_id: string;
  /**
   * file/markdown 为文本；chart/table/metrics/board 为结构化 JSON。
   */
  content: {
    [k: string]: unknown;
  };
  version: number;
}
/**
 * run 完整快照（审批图与运行面板共用）。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkflowRunSnapshot".
 */
export interface WorkflowRunSnapshot {
  /**
   * 已发布 artifact 条目（id/member/version/spec）。
   */
  artifacts?: unknown[];
  /**
   * 已批准的 world.run 命令集。
   */
  commands?: string[];
  created_at_ms?: number | null;
  error?: string | null;
  finished_at_ms?: number | null;
  /**
   * 打开的 hole（name/prompt/schema/site）。
   */
  holes?: unknown[];
  phases?: WorkflowPhaseState[];
  /**
   * 编译出的 plan（sites/phases/edges/roles/model_refs/commands/schemas）。
   */
  plan?: {
    [k: string]: unknown;
  };
  reports?: unknown[];
  result?: unknown;
  resumed_from?: string | null;
  roles?: WorkflowRoleState[];
  run_id: string;
  /**
   * 草稿脚本落盘路径。
   */
  script_path?: string | null;
  status: WorkflowRunStatus;
  title?: string | null;
  usage?: {
    [k: string]: unknown;
  };
}
/**
 * 当前工作目录。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkspaceGitBranchParams".
 */
export interface WorkspaceGitBranchParams {
  cwd: string;
}
/**
 * 目录是否处于 Git 仓库中及当前分支。
 *
 * This interface was referenced by `ProtocolTypes`'s JSON-Schema
 * via the `definition` "WorkspaceGitBranchResult".
 */
export interface WorkspaceGitBranchResult {
  branch?: string | null;
}
