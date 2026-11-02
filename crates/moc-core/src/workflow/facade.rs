//! Workflow facade：脚本可见 API 的单一事实源。
//!
//! [`FACADE_DTS`] 同时用于三处：编译器静态分析（允许的调用形状与类型名）、
//! skill 文本末节逐字嵌入、模型起草脚本时的 API 文档。改这里三处一起变。
//! 语义与上限逐条对齐 zcode dynamic-workflow；与 zcode 的差异只有
//! `world.run` 的 `output` 选项（spec §2）与无可做全量类型检查。

/// 模型可见的 facade 声明文件全文（`workflow-facade.d.ts`）。
pub const FACADE_DTS: &str = r#"/** The pending result of one ask. Await it, or pass it to Promise.all with others. */
declare interface Node<T> extends PromiseLike<T> {}

/** A model declared with model(). */
declare class ModelRef { private constructor(); private readonly modelRef: never }

/**
 * Declare a model subagents may run on: "modelId" or "providerId/modelId", optional "$level".
 * String literal only (checked at launch); choose among ModelRefs at run time.
 */
declare function model(id: string): ModelRef;

/** Persona of a subagent, fixed when agent() creates it. Every subagent has the same working tools. */
declare interface AgentPersona {
  /** System prompt describing the actor's role. */
  system?: string;
  /** Model id literal or ModelRef; omitted = the run's subagent model. */
  model?: ModelRef | string;
}

/**
 * A subagent: one conversation that keeps its context across asks. Asks issued while it is
 * busy wait their turn, first in, first out.
 */
declare interface Agent {
  /**
   * Give the subagent one task. With a type argument, the answer is a value of type T,
   * validated against a schema built from it: T is an interface you define in this script
   * with a plain "interface" declaration (no "declare" modifier). Without one, the answer is
   * the subagent's final reply text.
   */
  ask<T = string>(instructions: string): Node<T>;
}

/**
 * Create a subagent. Every call starts a new, empty conversation; to share one, share the
 * returned value. A non-empty name is an identity: unique within the run (a literal duplicate
 * is a compile error, a computed one fails the run) and the key AmendWorkflow matches cached
 * work by. Unnamed subagents are allowed and never reuse cached work.
 */
declare function agent(name?: string, persona?: string | AgentPersona): Agent;

/**
 * The run's arguments. For a saved workflow they are validated against its declared
 * arguments, with defaults filled in, before the run starts; an inline script or a snippet
 * gets {}. Always defined; the values are unknown, so narrow them (String(args.target), a
 * typeof guard).
 */
declare const args: Readonly<Record<string, unknown>>;

/** This run: its id, and for an amend the run it revises. */
declare const run: { readonly id: string; readonly resumedFrom?: string };

/** Emit a progress message to the user. */
declare function log(message: string): void;

/**
 * Publish one intermediate result while the run is going; returns nothing. Items are
 * recorded: a resumed run never shows one twice, and they reach the completion notification
 * even when the run fails. The item must be JSON-serializable (plain objects, arrays, strings,
 * numbers, booleans, null); functions, class instances, Date and promises are compile
 * errors. Caps fail the whole run, not the call: 65,536 items per run, 1 MiB per serialized
 * item, 1 GiB per run.
 *
 * artifactId routes the item to a dashboard: a compile-time literal naming a preset
 * (artifact.chart / table / metrics / board) whose declaration has executed by then. The
 * item becomes one point, row, tile value or card there and still lands in the run's
 * Results. A tag naming nothing, or a file/markdown artifact, fails the run.
 */
declare function report(item: unknown, artifactId?: string): void;

/** A published artifact version: the id it was published under, and which version this call minted. */
declare interface ArtifactRef { id: string; version: number }
/** Card metadata every artifact kind accepts. */
declare interface ArtifactOptions {
  /** Shown as the card title; defaults to the id. In the user's language. */
  title?: string;
  /** A sentence or two, shown beside the title when this artifact is the run's primary. */
  description?: string;
  /** The run's deliverable: the card and the run pane lead with it. At most one id per run; once set it stays set for later versions. */
  primary?: boolean;
}
declare interface ArtifactFileOptions extends ArtifactOptions {
  /** Overrides the type sniffed from the extension ("application/pdf", "text/html", …). */
  contentType?: string;
}
/** One value taken from a reported item: a dot path into the item ("timing.after"). */
declare interface ArtifactField { field: string; label?: string; unit?: string }
declare interface ChartSpec extends ArtifactOptions {
  type?: "line" | "bar" | "scatter";        // default "line"
  x: ArtifactField;
  y: ArtifactField | ArtifactField[];        // several = several series
  scale?: "linear" | "log";                  // y axis, default "linear"
  /** A reference value drawn as a horizontal rule, taken from the first item that has the field. */
  baseline?: ArtifactField;
}
declare interface TableSpec extends ArtifactOptions {
  columns: ArtifactField[];
  /** Field that identifies a row; a later item with the same key replaces the row. Absent = append-only. */
  key?: string;
}
declare interface MetricsSpec extends ArtifactOptions {
  /** Each tile shows the value from the latest item that has the field. */
  metrics: ArtifactField[];
}
declare interface BoardSpec extends ArtifactOptions {
  /** Field identifying a card; a later item with the same key moves/updates the card. */
  key: string;
  /** Field holding the card's column. */
  status: string;
  /** Column order. Items whose status is not listed land in a trailing "other" column. */
  columns: string[];
  /** Field for the card title (default: the key) and extra fields shown on the card. */
  cardTitle?: string;
  detail?: ArtifactField[];
}
/**
 * Publish what the user keeps: cards beside the run, kept after it ends. Every id is a
 * compile-time string literal (non-empty, at most 64 characters of [A-Za-z0-9_.-]) and is
 * used with one member only.
 * - CONTENT (file, markdown) are effects: async, resolve to an ArtifactRef, and reject
 *   catchably (missing file, not a file, outside the workspace, over the cap, or no artifact
 *   store in this host).
 *   Bytes are copied at publish time; republishing an id mints the next version and keeps
 *   the old ones.
 * - PRESET (chart, table, metrics, board) are declarations: synchronous and void, drawn from
 *   the items tagged report(item, "<id>"). Declare each once, at the top level; an identical
 *   re-declaration is a no-op, a different or malformed spec fails the whole run.
 * Caps: 32 ids per run, 16 versions per id, 20 MiB per file, 256 KB per markdown, 120
 * characters of title and 500 of description.
 */
declare const artifact: {
  /** Publish a workspace file, path resolved as in files.read(). The type comes from the extension unless opts.contentType sets it. */
  file(id: string, path: string, opts?: ArtifactFileOptions): Promise<ArtifactRef>;
  /** Publish markdown text the script composed. */
  markdown(id: string, content: string, opts?: ArtifactOptions): Promise<ArtifactRef>;
  /** Declare a chart fed by report(item, id): each tagged item is one point. */
  chart(id: string, spec: ChartSpec): void;
  /** Declare a table fed by report(item, id): each tagged item is one row. */
  table(id: string, spec: TableSpec): void;
  /** Declare a metric tile row fed by report(item, id): each tile shows the newest value it has. */
  metrics(id: string, spec: MetricsSpec): void;
  /** Declare a board fed by report(item, id): each tagged item is a card, placed by its status field. */
  board(id: string, spec: BoardSpec): void;
};

/**
 * Mark the start of a phase: a named group of the steps that follow, drawn as one node on
 * the graph the user approves. Presentation only: it starts, waits for and returns nothing.
 * Required. The name is a non-empty compile-time literal and the call a standalone
 * statement. A marker covers every step from it to the end of the block it stands in
 * (nested blocks and inlined helpers included); two markers with the same name are one
 * phase. Every phase contains at least one ask or world.run.
 */
declare function phase(name: string): void;

/** One matching line found by files.grep. */
declare interface GrepMatch {
  /** Workspace-relative path of the file the match was found in. */
  path: string;
  /** One-based line number of the match. */
  line: number;
  /** The full text of the matching line. */
  text: string;
}

/**
 * Read-only views of the workspace. Each result is recorded, so a resumed run gets the same
 * answer back instead of reading again. There is no write: writing is a subagent's task.
 */
declare const files: {
  /** Workspace-relative paths matching a glob, sorted. Over 2000 files the call rejects. */
  glob(pattern: string): Promise<string[]>;
  /** Read one workspace file as UTF-8 text. Size-capped. */
  read(path: string): Promise<string>;
  /**
   * Search contents with a ripgrep-compatible regex, optionally narrowed by a glob over
   * paths ("*.ts", "src/**"); one entry per matching line. Over 2000 matches or 256KB of
   * results the call rejects.
   */
  grep(pattern: string, glob?: string): Promise<GrepMatch[]>;
};

/** The working tree's status, as reported by git.status(). */
declare interface GitStatus {
  /** Current branch name; absent when HEAD is detached. */
  branch?: string;
  /** True when nothing is staged, modified, or untracked. */
  clean: boolean;
  /** Workspace-relative paths staged for the next commit. */
  staged: string[];
  /** Workspace-relative paths modified in the working tree but not staged. */
  unstaged: string[];
  /** Workspace-relative paths git does not track (honouring .gitignore). */
  untracked: string[];
}

/** One commit, as reported by git.log(). */
declare interface GitCommit {
  /** Full commit hash. */
  hash: string;
  /** First line of the commit message. */
  subject: string;
  /** Author name. */
  author: string;
  /** Author date, ISO 8601. */
  date: string;
}

/**
 * Read-only git views, recorded like files.* so a resumed run gets the same answer back; no
 * call can write. A base names a single ref (no ".." ranges). Paths are workspace-relative, and
 * changes outside the workspace are not reported; git.log alone is repository-wide. Caps
 * reject rather than truncate: diff at 512KB, log at 100 commits. Outside a git repository,
 * or without git, every call rejects catchably.
 */
declare const git: {
  /**
   * Workspace-relative paths that changed. With no base: files modified against HEAD, plus
   * untracked files. With a base ref: tracked files whose current state differs from that
   * ref, uncommitted edits included, untracked files not.
   */
  changedFiles(base?: string): Promise<string[]>;
  /**
   * Unified diff against base (default HEAD). Covers the whole workspace unless you
   * narrow it to one workspace-relative path.
   */
  diff(base?: string, path?: string): Promise<string>;
  /** The current working-tree status, for the workspace. */
  status(): Promise<GitStatus>;
  /** The most recent commits, newest first. Default 20, maximum 100. */
  log(count?: number): Promise<GitCommit[]>;
};

/** The outcome of one world.run command, including nonzero exits. */
declare interface WorldRunResult {
  /** The process exit code. Nonzero is a normal, returned outcome — branch on it. */
  exitCode: number;
  /** Captured stdout (UTF-8). Output mode decides what arrives here; see opts.output. */
  stdout: string;
  /** Captured stderr (UTF-8). Same handling as stdout. */
  stderr: string;
}

/**
 * Run a command. Each call runs once and its result is recorded: a resumed run reuses the
 * recorded result instead of running the command again. A completed process RESOLVES,
 * nonzero exit included; the promise rejects (catchably) only on spawn failure or timeout
 * (default 300000ms, timeoutMs overrides, no upper cap). cmd is a compile-time string
 * literal, shown to the user at confirmation. Fixed argv, never a shell: no pipes,
 * redirection or expansion. cwd is the workspace.
 *
 * opts.output selects capture: "full" (default) returns both streams, rejecting when either
 * stream exceeds 16 MiB; "discard" drops them; { tail: n } keeps at most n trailing bytes.
 */
declare const world: {
  run(
    cmd: string,
    args?: string[],
    opts?: { timeoutMs?: number; output?: "full" | "discard" | { tail: number } },
  ): Promise<WorldRunResult>;
};

/**
 * A stream of typed items between two stages: one stage send()s, another drains it with
 * for await. Unbounded, first-in-first-out, each item to exactly one receiver.
 */
declare interface Channel<T> extends AsyncIterable<T> {
  /** Enqueue one item; synchronous, never waits. Throws ChannelClosed after close(). */
  send(item: T): void;
  /**
   * End the stream: receivers get the remaining items, then their loops exit. Idempotent.
   * A channel nobody closes fails the run as ChannelDeadlock, naming it, once nothing in
   * flight could send to it again.
   */
  close(): void;
}

/**
 * Create a channel. The name labels the deadlock message; it is not an identity. A receiver
 * on an empty, open channel waits until a send or the close.
 */
declare function channel<T>(name?: string): Channel<T>;

/**
 * Run an async block as a concurrent stage. It starts immediately, not when awaited, and
 * returns its promise.
 */
declare function future<T>(body: () => Promise<T>): Promise<T>;

/**
 * A typed gap: code you write later, once an earlier step's findings are in. When the run
 * reaches an unfilled hole, that branch waits while you write the body (fill_workflow_hole);
 * the rest of the run keeps going. prompt is the message you will receive, so interpolate
 * the values the decision needs. The body runs in the hole's place, sees the bindings
 * declared before it, and returns the hole's value. The filled form passes the body last.
 */
declare function hole<T>(name: string, prompt?: string): Promise<T>;
declare function hole<T>(name: string, body: () => Promise<T>): Promise<T>;
declare function hole<T>(name: string, prompt: string, body: () => Promise<T>): Promise<T>;
"#;

// —— 上限（spec §2，随工具结果下发给引擎校验；注释写明语义）——

/// report：每 run 条目数上限；超限整 run 失败而非截断。
pub const REPORT_MAX_ITEMS: usize = 65_536;
/// report：单条序列化后上限 1 MiB。
pub const REPORT_MAX_ITEM_BYTES: usize = 1_048_576;
/// report：每 run 序列化总量上限 1 GiB。
pub const REPORT_MAX_TOTAL_BYTES: u64 = 1_073_741_824;
/// artifact：每 run id 数上限。
pub const ARTIFACT_MAX_IDS: usize = 32;
/// artifact：每 id 版本数上限。
pub const ARTIFACT_MAX_VERSIONS: usize = 16;
/// artifact.file：单文件字节上限 20 MiB。
pub const ARTIFACT_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// artifact.markdown：单篇字节上限 256 KiB。
pub const ARTIFACT_MAX_MARKDOWN_BYTES: usize = 256 * 1024;
/// artifact：title/description 长度上限。
pub const ARTIFACT_MAX_TITLE_LEN: usize = 120;
pub const ARTIFACT_MAX_DESCRIPTION_LEN: usize = 500;
/// artifact id 字符集与长度。
pub const ARTIFACT_ID_PATTERN: &str = "^[A-Za-z0-9_.-]{1,64}$";
/// world.run 默认超时 300s；opts.timeoutMs 覆盖、无上限。
pub const WORLD_RUN_DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// world.run "full" 模式单流（stdout/stderr 各自）上限 16 MiB，超限拒绝。
pub const WORLD_RUN_STREAM_MAX_BYTES: usize = 16 * 1024 * 1024;
/// files.glob 结果条数上限，超限拒绝。
pub const FILES_GLOB_MAX: usize = 2_000;
/// files.grep：匹配条数与序列化字节上限，先到先拒。
pub const FILES_GREP_MAX_MATCHES: usize = 2_000;
pub const FILES_GREP_MAX_BYTES: usize = 256 * 1024;
/// files.read 的读入上限。
pub const FILES_READ_MAX_BYTES: usize = 4 * 1024 * 1024;
/// git.diff 输出上限 512 KiB；git.log 默认 20、上限 100。
pub const GIT_DIFF_MAX_BYTES: usize = 512 * 1024;
pub const GIT_LOG_DEFAULT: u32 = 20;
pub const GIT_LOG_MAX: u32 = 100;
/// phase/hole 名长度上限（非空字面量之外的长度约束）。
pub const NAME_MAX_LEN: usize = 128;
/// 在飞 ask 并发上限的硬顶；有效上限另受可用并行度钳制（spec §4）。
pub const MAX_CONCURRENCY_CEILING: usize = 16;
