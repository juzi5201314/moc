// moc 插件宿主 API 的类型声明（`crates/moc-plugin/js/moc.d.ts`）。
//
// 用法：把本文件拷到插件目录下的 `moc.d.ts`，插件 `index.js` 顶部写
//   /** @type {import("./moc").Plugin} */
// 编辑器即可对 `export default` 与 `ctx`、`input` 给出提示。运行时不需要本文件。
//
// 这里的每个函数在 moc 里都是**同步**的：宿主在插件专用线程里跑 QuickJS，
// 跨边界调用会阻塞该线程直到返回。`run`/`prompt`/`activate` 可以是 async，
// 但内部的 `kv` 与 `tools.call` 仍然是同步返回。

/** 宿主 API 版本；当前为 `"1"`。 */
export const version: string;

/**
 * 当前会话快照。每次调用都重新读取，不缓存。
 */
export function session(): Session;

/** 插件级持久化键值存储：按插件名共享，跨项目、跨重启保留。 */
export const kv: {
  /** 读取一个键；不存在时返回 `undefined`。 */
  get<T = unknown>(name: string): T | undefined;
  /** 写入一个 JSON 可序列化的值；`undefined` 按 `null` 存。 */
  set(name: string, value: unknown): void;
  /** 删除一个键；返回是否真的删掉了。 */
  delete(name: string): boolean;
  /** 全部键名，升序。 */
  keys(): string[];
  /** 清空本插件的全部键。 */
  clear(): void;
};

/** 写 moc 日志；`RUST_LOG=moc_plugin=debug` 时能看到 debug。 */
export const log: {
  debug(...parts: unknown[]): void;
  info(...parts: unknown[]): void;
  warn(...parts: unknown[]): void;
  error(...parts: unknown[]): void;
};

/**
 * 调用宿主工具。
 *
 * 只在工具的 `run` 里可用，且只能调用只读内置工具（`read`/`grep`/`find` 等）；
 * 在命令、钩子、`prompt`、`activate` 里调用会抛 `E_PLUGIN_TOOLS_UNAVAILABLE`。
 * 失败以异常形式抛出，异常对象带 `code` 与 `message`。
 */
export const tools: {
  call(
    name: string,
    args?: Record<string, unknown>,
  ): {
    content: string;
    structuredContent?: unknown;
    isError: boolean;
  };
};

/** 一次插件调用时的会话上下文。 */
export interface Session {
  /** 会话 ID。 */
  sessionId: string;
  /** 工作目录绝对路径。 */
  cwd: string;
  /** `normal` | `ask` | `plan` | `ptc` | `clm_experimental`。 */
  mode: string;
  /** `full` | `workspace` | `workspace-only`。 */
  permission: string;
  /** 当前模型选择器（`provider/model`）。 */
  model: string;
  /** 是否在子代理会话里。 */
  isSubagent: boolean;
}

/** 工具的风险等级；决定只读模式（问答、计划）是否可见。 */
export type ToolTier = "read" | "write" | "exec";

export interface ToolDefinition {
  /**
   * 工具描述。必填且非空：它会作为工具描述进模型提示，
   * 只写这个工具自己的用法，不要重复通用工具政策。
   */
  description: string;
  /** JSON Schema（`type: "object"`），原样进模型提示。 */
  parameters: {
    type: "object";
    properties?: Record<string, unknown>;
    required?: string[];
    [key: string]: unknown;
  };
  /** 默认 `"read"`。 */
  tier?: ToolTier;
  /** 默认 `false`：作为批次屏障，不与其它工具并发。 */
  parallel?: boolean;
  /**
   * 工具实现。`input` 是符合 `parameters` 的对象。
   *
   * 返回值：`string` 作为正文；`{content, structuredContent?, isError?}` 作为富结果；
   * 其它 JSON 会被 `JSON.stringify`；`undefined`/`null` 表示空正文。
   */
  run(input: any, ctx: Session): ToolResult | Promise<ToolResult>;
}

/** `run` / 命令的返回类型。 */
export type ToolResult =
  | string
  | undefined
  | null
  | {
      content?: string;
      structuredContent?: unknown;
      isError?: boolean;
    }
  | unknown;

export interface CommandDefinition {
  /** 命令描述，前端补全表里显示。 */
  description: string;
  /** 用法提示，例如 `"[称呼]"`；可省略。 */
  usage?: string;
  /** `args` 是命令名之后的原始文本，未分词。 */
  run(args: string, ctx: Session): ToolResult | Promise<ToolResult>;
}

/** `beforeToolCall` 的事件。 */
export interface BeforeToolCallEvent {
  /** 工具名（内置、MCP 或插件工具）。 */
  name: string;
  /** 本次调用的参数。 */
  arguments: Record<string, unknown>;
}

/** `beforeToolCall` 的返回：拒绝、改写，或什么都不做。 */
export type BeforeToolCallResult =
  | undefined
  | null
  | {
      /** 非空时拒绝本次调用；模型会收到 `E_PLUGIN_DENIED`，文本含插件名与该原因。 */
      deny?: string;
      /** 替换参数；改写发生在沙盒判定、bash 规则与授权之前。 */
      arguments?: Record<string, unknown>;
    };

/** `afterToolCall` 的事件；返回值被忽略。 */
export interface AfterToolCallEvent extends BeforeToolCallEvent {
  /** 工具结果的正文。 */
  content: string;
  /** 工具是否以失败结束。 */
  isError: boolean;
}

export interface Hooks {
  beforeToolCall?(
    event: BeforeToolCallEvent,
    ctx: Session,
  ): BeforeToolCallResult | Promise<BeforeToolCallResult>;
  afterToolCall?(
    event: AfterToolCallEvent,
    ctx: Session,
  ): void | Promise<void>;
}

/** `export default` 的形状。 */
export interface Plugin {
  /** 可选；与 `plugin.json` 不一致时以清单为准。 */
  name?: string;
  version?: string;
  description?: string;
  /**
   * 模块求值完成后调用一次；宿主会等 Promise 落定。
   *
   * 加载发生在会话之外，所以这里没有会话上下文：`session()` 与 `tools.call()`
   * 都会抛错，`kv` 与 `log` 可用。
   */
  activate?(): void | Promise<void>;
  /** 进程退出或插件卸载时调用。 */
  deactivate?(): void | Promise<void>;
  /** 工具名 → 定义。名字全局唯一，只允许字母、数字、下划线、连字符。 */
  tools?: Record<string, ToolDefinition>;
  /** 命令名 → 定义，最终是 `/命令名`；与插件名同规则。 */
  commands?: Record<string, CommandDefinition>;
  hooks?: Hooks;
  /**
   * 提示词贡献：返回的文本进系统提示的 `<plugins>` 片段，按插件名排序拼接。
   * 这条片段每轮重建，不要放每次都变的内容。
   */
  prompt?(ctx: Session): string | string[] | undefined | Promise<string | string[] | undefined>;
}
