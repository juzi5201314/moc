// 工具块使用隐藏／简略两个全局预设，完整结果由单块点击展开；这里定义展示策略，
// 并把工具结果文本解析成可渲染的结构。规则与 TUI（crates/moc-tui/src/render/scrollback.rs）
// 同一套：read 只占一行、grep/find 只报数量、bash 给单行命令 + 末尾 4 行输出、
// edit/write 给最多 6 行 diff。全部是纯函数，组件只负责画。

import { type MsgKey, t } from "@/lib/i18n";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import { activitySnippet } from "@/lib/turn";
import type {
  AgentMessageReceipt,
  AgentWaitResult,
  TodoPhase,
  TodoStatus,
  ToolDiff,
  WebDetailLevel,
} from "@/protocol/types";

export type DetailLevel = WebDetailLevel;

/** 简略档 bash 输出的末尾行数。 */
export const BRIEF_OUTPUT_ROWS = 4;
/** 简略档正文（diff、问答、通用预览）的最多行数。 */
export const BRIEF_BODY_ROWS = 6;
/** 简略档思考块露出的末尾行数。 */
export const BRIEF_THINKING_ROWS = 4;

/**
 * 正文策略：固定摘要与可展开正文分开，不允许完整档绕过固定摘要。
 * - `summary`：不展示原始正文；通知常显，网页给摘要预览，媒体按需展开
 * - `agent-preview`：简略档显示任务／交付报告开头，不展开原始载荷
 * - `title`：简略档只有标题（未知工具）
 * - `command`：单行命令 + 末尾输出（bash）
 * - `script`：execute——简略档只有标题，完整档给高亮脚本 + 结果
 * - `diff`：最多 {@link BRIEF_BODY_ROWS} 行 diff（edit、write）
 * - `todo`：简略保留前三项和后两项，完整显示全部，隐藏不显示任务项
 * - `preview`：结果开头若干行（ask、子代理类）
 */
export type BodyPolicy =
  | "summary"
  | "agent-preview"
  | "title"
  | "command"
  | "script"
  | "diff"
  | "todo"
  | "preview";

const POLICY: Record<string, BodyPolicy> = {
  read: "summary",
  grep: "summary",
  find: "summary",
  skill: "summary",
  clm_context: "summary",
  stop_agent: "summary",
  bash: "command",
  execute: "script",
  edit: "diff",
  write: "diff",
  todo: "todo",
  web_fetch: "summary",
  ask: "preview",
  spawn_agent: "agent-preview",
  yield: "agent-preview",
  message_agent: "preview",
  wait_agent: "preview",
  sidekick: "preview",
};

export function bodyPolicy(name: string): BodyPolicy {
  return POLICY[name] ?? "title";
}

/** 同批次连续同名调用可以合并成一行：只看标题的工具才合并，带正文预览的逐个展示。 */
export function groupable(name: string): boolean {
  const policy = bodyPolicy(name);
  return policy === "title" || (policy === "summary" && name !== "web_fetch");
}

/** 内置工具的展示名（与 TUI 对齐，PascalCase）；MCP 与插件工具保留原始名称。 */
const BUILTIN_TOOL_NAMES: Record<string, string> = {
  bash: "Bash",
  read: "Read",
  edit: "Edit",
  write: "Write",
  grep: "Grep",
  find: "Find",
  execute: "Execute",
  todo: "Todo",
  web_fetch: "WebFetch",
  ask: "Ask",
  skill: "Skill",
  clm_context: "ClmContext",
  job: "Job",
  plan: "Plan",
  exit_plan: "ExitPlan",
  TodoReminder: "TodoReminder",
  spawn_agent: "SpawnAgent",
  list_agents: "ListAgents",
  message_agent: "IRC",
  stop_agent: "StopAgent",
  wait_agent: "WaitAgent",
  sidekick: "Sidekick",
  yield: "Yield",
  get_goal: "GetGoal",
  create_goal: "CreateGoal",
  update_goal: "UpdateGoal",
  create_workflow: "CreateWorkflow",
  amend_workflow: "AmendWorkflow",
  resume_workflow_run: "ResumeWorkflowRun",
  stop_workflow_run: "StopWorkflowRun",
  fill_workflow_hole: "FillWorkflowHole",
  save_workflow: "SaveWorkflow",
  list_saved_workflows: "ListSavedWorkflows",
  workflow_status: "WorkflowStatus",
  eval_workflow_snippet: "EvalWorkflowSnippet",
};

/** 工具标题名：内置工具用 PascalCase，手动 bash 与 IRC/Sidekick 与 TUI 对齐，MCP/插件原样展示。 */
export function toolDisplayName(name: string, args?: Record<string, unknown>): string {
  if (name === "bash" && typeof args?.exclude_from_context === "boolean") {
    return args.exclude_from_context ? t("tool.bash_excluded") : "! Bash";
  }
  return BUILTIN_TOOL_NAMES[name] ?? (name || t("tm.tool"));
}

/** 「内置工具优先」拒绝的固定前缀（ToolOutcome 的 `[{code}] ` 格式）。 */
export const PREFER_BUILTIN_TAG = "[E_PREFER_BUILTIN]";

/** 内置工具优先策略重定向：按警告而非失败样式呈现，与 TUI `prefer_builtin_redirect` 一致。 */
export function preferBuiltinRedirect(tool: ToolBlock): boolean {
  return (
    toolFailed(tool) && tool.name === "bash" && Boolean(tool.result?.startsWith(PREFER_BUILTIN_TAG))
  );
}

export function toolFailed(tool: ToolBlock): boolean {
  return tool.isError || tool.status === "error";
}

export function toolExpandable(tool: ToolBlock): boolean {
  if (tool.preview) return true;
  const policy = bodyPolicy(tool.name);
  return (
    toolFailed(tool) ||
    (policy !== "summary" && policy !== "agent-preview") ||
    tool.images.length > 0 ||
    (tool.name === "web_fetch" && tool.result !== undefined)
  );
}

/** 只识别工具生成的通知，带行号的源码与匹配结果不在这里解包。 */
export function toolNotices(result: string): string[] {
  return result.split("\n").filter((raw) => {
    const line = raw.trimStart();
    return (
      (line.startsWith("[… ") &&
        (line.includes(" bytes omitted; full output: ") || line.includes(" more; "))) ||
      /^\[(Showing lines |File exceeds |Invalid UTF-8 |limit |Scan incomplete|skipped )/.test(
        line,
      ) ||
      (line.startsWith("[Line ") && line.includes("content omitted;")) ||
      line.startsWith("Lines shown as a size notice") ||
      line.startsWith("[Long lines omitted;") ||
      line.startsWith("[Continue with read ") ||
      (line.startsWith("[") &&
        (line.includes(" matches shown; ") ||
          (line.includes(" files exceeded the ") && line.endsWith("were not searched]"))))
    );
  });
}

export type FetchedPage = {
  url: string;
  title?: string;
  tokens?: number;
  truncated?: boolean;
  error?: string;
};

/** 摘要在正文前，由 core 抓取结束时生成；不从 LLM usage 增量猜测页面用量。 */
export function fetchedPages(result: string): FetchedPage[] {
  const prefix = "Web fetch summary: ";
  if (!result.startsWith(prefix)) return [];
  try {
    const pages: unknown = JSON.parse(result.split("\n", 1)[0].slice(prefix.length));
    return Array.isArray(pages)
      ? pages.filter(
          (page): page is FetchedPage =>
            page !== null && typeof page === "object" && typeof page.url === "string",
        )
      : [];
  } catch {
    return [];
  }
}

export function agentPreviews(tool: ToolBlock): { label: string; text: string }[] {
  const args = parseArgs(tool.arguments);
  if (tool.name === "yield") return [{ label: t("tool.report"), text: str(args, "report") }];
  if (tool.name !== "spawn_agent") return [];
  if (Array.isArray(args.tasks))
    return args.tasks.map((task: Record<string, unknown>) => ({
      label: str(task, "title") || t("tool.subagent_task"),
      text: str(task, "task"),
    }));
  return [{ label: str(args, "title") || t("tool.subagent_task"), text: str(args, "task") }];
}

export function parseArgs(raw: string): Record<string, unknown> {
  try {
    const value: unknown = JSON.parse(raw);
    return value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

function str(args: Record<string, unknown>, key: string): string {
  const value = args[key];
  return typeof value === "string" ? value : "";
}

function strings(args: Record<string, unknown>, key: string): string[] {
  const value = args[key];
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

function count(n: number, singular: string, plural: string): string {
  return `${n} ${n === 1 ? singular : plural}`;
}

// ---------- 标题行 ----------

export type Headline = {
  /** 动作之后的主体：路径、命令描述、查询词。 */
  target: string;
  /** 主体之后的次级范围（grep/find 搜索根、read 目录 glob、skill 描述、sidekick 标记）。 */
  scope?: string;
  /** 右侧尾注：数量、退出码、投递结果。 */
  stat?: string;
  /** 尾注是失败信息。 */
  statError?: boolean;
  /** 尾注是警告信息（如 wait_agent 超时）。 */
  statWarn?: boolean;
  /** 覆盖默认动作名。 */
  verb?: string;
};

/** todo 摘要：与 TUI `todo_summary_spans` 同规则。 */
function todoSummary(args: Record<string, unknown>): string {
  const op =
    str(args, "op") ||
    (Array.isArray(args.list) && args.list.length > 0
      ? "init"
      : Array.isArray(args.items) && args.items.length > 0 && str(args, "phase")
        ? "append"
        : "init");
  if (op === "init") {
    const count = Array.isArray(args.list)
      ? args.list.reduce((sum, phase) => {
          const items =
            phase && typeof phase === "object"
              ? (phase as Record<string, unknown>).items
              : undefined;
          return sum + (Array.isArray(items) ? items.length : 0);
        }, 0)
      : Array.isArray(args.items)
        ? args.items.length
        : undefined;
    return count !== undefined ? `${op} ${count} tasks` : op;
  }
  if (op === "append") {
    const phase = str(args, "phase");
    const items = Array.isArray(args.items) ? args.items : undefined;
    const parts = [phase ? `${op} ${phase}` : op];
    if (items !== undefined) parts.push(`${items.length} items`);
    return parts.join(" · ");
  }
  if (op === "rm") {
    const target = str(args, "task") || str(args, "phase");
    return target ? `${op} ${target}` : op;
  }
  return op;
}

export function toolHeadline(tool: ToolBlock): Headline {
  if (tool.preview)
    return {
      target: tool.preview.target || tool.preview.summary,
    };
  const args = parseArgs(tool.arguments);
  const result = tool.result;
  const failed = toolFailed(tool);
  switch (tool.name) {
    case "read": {
      const range = readRange(args, result);
      const globs = strings(args, "glob").join(" ");
      return { target: `${str(args, "path")}${range}`, scope: globs || undefined };
    }
    case "grep":
      return {
        target: str(args, "pattern"),
        scope: str(args, "path") || undefined,
        stat: result !== undefined && !failed ? grepCountText(result) : undefined,
      };
    case "find":
      return {
        target: strings(args, "glob").join(" "),
        scope: str(args, "path") || undefined,
        stat:
          result !== undefined && !failed
            ? count(findFiles(result).paths.length, "file", "files")
            : undefined,
      };
    case "bash": {
      const description = str(args, "description");
      const exit = result === undefined ? null : commandExit(result);
      return {
        target: description || (str(args, "command").trim().split("\n", 1)[0] ?? ""),
        stat: exit ?? undefined,
        statError: exit !== null,
      };
    }
    case "execute":
      return { target: str(args, "description") };
    case "edit":
    case "write":
      return { target: str(args, "path") };
    case "todo": {
      const phases = result !== undefined && !failed ? parseTodo(result) : [];
      const items = phases.flatMap((phase) => phase.items);
      const done = items.filter(
        (item) => item.status === "completed" || item.status === "abandoned",
      ).length;
      return {
        target: todoSummary(args),
        stat: items.length ? `${done}/${items.length}` : undefined,
      };
    }
    case "TodoReminder":
      return { target: t("tool.todo_reminder") };
    case "web_fetch": {
      const urls = strings(args, "urls");
      return {
        target: urls.map(displayUrl).join(" "),
        stat: urls.length > 1 ? count(urls.length, "page", "pages") : undefined,
      };
    }
    case "spawn_agent":
      return {
        target: Array.isArray(args.tasks)
          ? args.tasks.map((task: Record<string, unknown>) => str(task, "title")).join(" · ")
          : str(args, "title"),
        stat: Array.isArray(args.tasks) ? t("tool.tasks_n", { n: args.tasks.length }) : undefined,
      };
    case "yield":
      return { target: t("tool.report") };
    case "skill": {
      const firstLine = result?.split("\n", 1)[0] ?? "";
      const description = firstLine.startsWith("Skill description: ")
        ? firstLine.slice("Skill description: ".length)
        : undefined;
      return { target: str(args, "name"), scope: description || undefined };
    }
    case "clm_context":
      return { target: str(args, "op") };
    case "stop_agent":
      return { target: str(args, "agent_id") };
    case "message_agent": {
      const rawTarget = str(args, "target");
      const receipts = parseMessageReceipts(result);
      const target = rawTarget ? `▶ ${rawTarget}` : "";
      if (!receipts) return { target };
      const failedCount = receipts.filter((receipt) => receipt.outcome === "failed").length;
      const parts: string[] = [];
      if (rawTarget === "all") parts.push(t("tool.broadcast"));
      if (receipts.length === 1) {
        parts.push(receipts[0].outcome);
      } else {
        parts.push(t("tool.delivered", { n: receipts.length - failedCount }));
      }
      if (failedCount > 0) parts.push(t("tool.failed_n", { n: failedCount }));
      return {
        target,
        stat: parts.join(" · "),
        statError: failedCount > 0,
      };
    }
    case "wait_agent": {
      const ids = strings(args, "agent_ids");
      const target = ids.length > 0 ? t("tool.wait_count", { n: ids.length }) : t("tool.wait_all");
      const parsed = parseWaitResult(result);
      if (!parsed) return { target };
      const parts: string[] = [];
      if (parsed.messages_pending) parts.push(t("tool.new_messages"));
      if (parsed.timed_out) parts.push(t("tool.timed_out"));
      return {
        target,
        stat: parts.length ? parts.join(" · ") : undefined,
        statWarn: parsed.timed_out || undefined,
      };
    }
    case "sidekick": {
      const mode = str(args, "mode") || "keep";
      const first = mode === "retry" ? "retry" : (str(args, "message").split("\n", 1)[0] ?? "");
      const flags: string[] = [];
      if (mode === "compact") flags.push(t("tool.flag_compact"));
      else if (mode === "fresh") flags.push(t("tool.flag_fresh"));
      if (args.block === false) flags.push(t("tool.flag_background"));
      return {
        target: first,
        scope: flags.length ? flags.join(" · ") : undefined,
      };
    }
    case "ask": {
      const questions = Array.isArray(args.questions) ? args.questions : [];
      const first = questions[0] as { question?: unknown } | undefined;
      return {
        target:
          typeof first?.question === "string"
            ? (first.question.trim().split("\n", 1)[0] ?? "")
            : "",
        stat: questions.length > 1 ? t("tool.questions_n", { n: questions.length }) : undefined,
      };
    }
    default:
      return { target: activitySnippet(tool, 200) };
  }
}

/** `https://host/path?q` → `host/path`。 */
function displayUrl(url: string): string {
  try {
    const parsed = new URL(url);
    const path = parsed.pathname === "/" ? "" : parsed.pathname;
    return `${parsed.host}${path}`;
  } catch {
    return url;
  }
}

/** read 标题的行号范围：结果分页 footer 优先（真实交付的行），否则按 offset/limit。 */
function readRange(args: Record<string, unknown>, result: string | undefined): string {
  const shown = result ? /\[Showing lines (\d+)-(\d+) of \d+\./.exec(result) : null;
  if (shown) return `:${shown[1]}-${shown[2]}`;
  const offset = typeof args.offset === "number" ? args.offset : undefined;
  const limit = typeof args.limit === "number" ? args.limit : undefined;
  if (offset !== undefined && limit !== undefined) return `:${offset}-${offset + limit - 1}`;
  if (offset !== undefined) return `:${offset}-`;
  return "";
}

// ---------- grep ----------

export type GrepRow = { line: number; matched: boolean; text: string };
export type GrepGroup = { path: string; rows: GrepRow[] };
export type GrepResult = { groups: GrepGroup[]; notes: string[] };

/** 分组结果行：`  12: 文本` 或 `  12- 文本`（行首两个空格）。 */
const GROUPED_ROW = /^ {2}(\d+)([:-])(.*)$/;
/** 单行结果：`路径:12:文本` 或 `路径-12-文本`，分隔符前后一致。 */
const INLINE_ROW = /^(.+?)([:-])(\d+)\2(.*)$/;

/**
 * grep 结果的分组。格式由 core 固定（crates/moc-core/src/tools/grep.rs）：
 * 单行文件写成 `路径:行号:正文`（匹配）或 `路径-行号-正文`（上下文）；多行文件
 * 写成路径标题行加若干缩进行；`[` 开头的是提示行。与 TUI 的 `grep_counts` 同一套判定。
 */
export function parseGrep(result: string): GrepResult {
  const groups: GrepGroup[] = [];
  const notes: string[] = [];
  // 当前分组就是最后一组：分组行只续接它。
  for (const line of result.split("\n")) {
    if (!line) continue;
    if (line.startsWith("[") || line === "No matches found") {
      notes.push(line);
      continue;
    }
    const current = groups.at(-1);
    const grouped = GROUPED_ROW.exec(line);
    if (grouped && current) {
      current.rows.push({
        line: Number(grouped[1]),
        matched: grouped[2] === ":",
        text: grouped[3],
      });
      continue;
    }
    const inline = INLINE_ROW.exec(line);
    if (inline) {
      const row = { line: Number(inline[3]), matched: inline[2] === ":", text: inline[4] };
      if (current?.path === inline[1]) current.rows.push(row);
      else groups.push({ path: inline[1], rows: [row] });
      continue;
    }
    groups.push({ path: line, rows: [] });
  }
  return { groups, notes };
}

export function grepCounts(result: string): { matches: number; files: number } {
  let matches = 0;
  let files = 0;
  for (const group of parseGrep(result).groups) {
    const hits = group.rows.filter((row) => row.matched).length;
    matches += hits;
    if (hits > 0) files++;
  }
  return { matches, files };
}

/** `12 matches · 3 files`；与 TUI 的计数行一致。 */
export function grepCountText(result: string): string {
  const { matches, files } = grepCounts(result);
  return `${count(matches, "match", "matches")} · ${count(files, "file", "files")}`;
}

// ---------- find ----------

/** find 结果：路径列表 + core 追加的提示行（与 TUI 的 `find_file_count` 同一套跳过规则）。 */
export function findFiles(result: string): { paths: string[]; notes: string[] } {
  const paths: string[] = [];
  const notes: string[] = [];
  for (const raw of result.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    if (line === "No files found matching pattern" || line.startsWith("[")) {
      notes.push(line);
    } else {
      paths.push(line);
    }
  }
  return { paths, notes };
}

const EXT_LANG: Record<string, string> = {
  rs: "rust",
  ts: "typescript",
  mts: "typescript",
  cts: "typescript",
  tsx: "tsx",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  jsx: "jsx",
  py: "python",
  go: "go",
  java: "java",
  c: "c",
  h: "c",
  cc: "cpp",
  cpp: "cpp",
  hpp: "cpp",
  css: "css",
  html: "html",
  json: "json",
  jsonc: "json",
  md: "markdown",
  toml: "toml",
  yaml: "yaml",
  yml: "yaml",
  sql: "sql",
  sh: "bash",
  bash: "bash",
  zsh: "bash",
  diff: "diff",
};

/** 按扩展名猜高亮语言；猜不到返回 undefined（按纯文本显示）。 */
export function languageOf(path: string): string | undefined {
  const name = path.split(/[\\/]/).pop() ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? EXT_LANG[name.slice(dot + 1).toLowerCase()] : undefined;
}

// ---------- bash ----------

/** core 在命令输出末尾追加的终态（crates/moc-core/src/tools/bash.rs）。 */
const EXIT_TAIL = /\n?\((exit -?\d+|killed after \d+s timeout|killed after output limit)\)\s*$/;

/** 非零退出 / 被杀时的尾注文本；正常结束返回 null。 */
export function commandExit(result: string): string | null {
  const match = EXIT_TAIL.exec(result);
  if (!match) return null;
  const text = match[1];
  if (text.startsWith("exit ")) return text;
  return text.includes("timeout") ? "timeout" : "killed";
}

/** 去掉末尾空行后的最后 `rows` 行，以及被折掉的行数。 */
export function tailLines(text: string, rows: number): { lines: string[]; hidden: number } {
  const all = text.replace(/\s+$/, "").split("\n");
  if (all.length === 1 && all[0] === "") return { lines: [], hidden: 0 };
  return { lines: all.slice(-rows), hidden: Math.max(0, all.length - rows) };
}

/** 去掉开头 / 末尾空行后的前 `rows` 行，以及被折掉的行数。 */
export function headLines(text: string, rows: number): { lines: string[]; hidden: number } {
  const all = text.trim().split("\n");
  if (all.length === 1 && all[0] === "") return { lines: [], hidden: 0 };
  return { lines: all.slice(0, rows), hidden: Math.max(0, all.length - rows) };
}

// ---------- todo ----------

const TODO_MARK: Record<string, TodoStatus> = {
  "[ ]": "pending",
  "[/]": "in_progress",
  "[x]": "completed",
  "[!]": "blocked",
  "[-]": "abandoned",
};

/** todo 结果文本（crates/moc-core/src/tools/todo.rs 的 `render`）→ 阶段列表。 */
export function parseTodo(result: string): TodoPhase[] {
  const phases: TodoPhase[] = [];
  for (const line of result.split("\n")) {
    const item = /^ {2}(\[[ /x!-]\]) (.*)$/.exec(line);
    if (item) {
      const status = TODO_MARK[item[1]];
      const phase = phases.at(-1);
      if (status && phase) {
        const reason = /^(.*) \(([^()]*)\)$/.exec(item[2]);
        phase.items.push(
          reason ? { status, task: reason[1], reason: reason[2] } : { status, task: item[2] },
        );
      }
    } else if (line.trim() && line !== "No todos.") {
      phases.push({ phase: line, items: [] });
    }
  }
  return phases;
}

// ---------- diff ----------

export type DiffPreview = { diff: ToolDiff; hidden: number };

/** diff 的前 `rows` 行（跨文件、跨 hunk 计数），以及被折掉的行数。 */
export function previewDiff(diff: ToolDiff, rows: number): DiffPreview {
  let budget = rows;
  let hidden = 0;
  const files = diff.files.map((file) => ({
    ...file,
    hunks: file.hunks.flatMap((hunk) => {
      const keep = hunk.lines.slice(0, Math.max(0, budget));
      hidden += hunk.lines.length - keep.length;
      budget -= keep.length;
      return keep.length ? [{ ...hunk, lines: keep }] : [];
    }),
  }));
  return { diff: { ...diff, files: files.filter((file) => file.hunks.length > 0) }, hidden };
}

// ---------- 档位 ----------

/** 单块手动切换后的形态：完整 ↔ 当前档位的收起形态（完整档下收起为隐藏）。 */
export function toggledLevel(axis: DetailLevel, current: DetailLevel): DetailLevel {
  if (current !== "full") return "full";
  return axis === "full" ? "hidden" : axis;
}

/** 工具全局只循环隐藏／简略；完整结果由单条工具手动展开。 */
export const NEXT_LEVEL: Record<DetailLevel, DetailLevel> = {
  hidden: "brief",
  brief: "hidden",
  full: "hidden",
};

export const LEVEL_LABEL: Record<DetailLevel, MsgKey> = {
  hidden: "level.hidden",
  brief: "level.brief",
  full: "level.full",
};

// ---------- 子代理 / ask 结构化解析 ----------

/** 解析 `message_agent` 的投递回执（与 TUI `message_receipts` 同口径）。 */
export function parseMessageReceipts(result?: string): AgentMessageReceipt[] | null {
  if (!result) return null;
  try {
    const parsed: unknown = JSON.parse(result);
    if (
      !parsed ||
      typeof parsed !== "object" ||
      !Array.isArray((parsed as { receipts?: unknown }).receipts)
    ) {
      return null;
    }
    const receipts = (parsed as { receipts: unknown[] }).receipts.filter(
      (item): item is AgentMessageReceipt =>
        item !== null &&
        typeof item === "object" &&
        typeof (item as Record<string, unknown>).title === "string" &&
        typeof (item as Record<string, unknown>).outcome === "string",
    );
    return receipts;
  } catch {
    return null;
  }
}

/** 解析 `wait_agent` 的等待结果（与 TUI `wait_result` 同口径）。 */
export function parseWaitResult(result?: string): AgentWaitResult | null {
  if (!result) return null;
  try {
    const parsed: unknown = JSON.parse(result);
    if (
      !parsed ||
      typeof parsed !== "object" ||
      !Array.isArray((parsed as { agents?: unknown }).agents)
    ) {
      return null;
    }
    return parsed as AgentWaitResult;
  } catch {
    return null;
  }
}

export type SidekickStructuredResult = {
  status: string;
  changesCount: number;
  passed: number;
  failed: number;
  notRun: number;
  openIssues: string[];
};

export type SidekickParsedResult = {
  status?: string;
  delivery?: string;
  model?: string;
  note?: string;
  report?: string;
  structured?: SidekickStructuredResult;
};

/** 解析 `sidekick` 的 JSON 结果（与 TUI `sidekick_tool_rows` 同口径）。 */
export function parseSidekickResult(result?: string): SidekickParsedResult | null {
  if (!result) return null;
  try {
    const raw: unknown = JSON.parse(result);
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
    const obj = raw as Record<string, unknown>;
    if (
      !("status" in obj) &&
      !("delivery" in obj) &&
      !("report" in obj) &&
      !("note" in obj) &&
      !("result" in obj)
    ) {
      return null;
    }
    let structured: SidekickStructuredResult | undefined;
    if (obj.result && typeof obj.result === "object" && !Array.isArray(obj.result)) {
      const res = obj.result as Record<string, unknown>;
      const verification = Array.isArray(res.verification) ? res.verification : [];
      const countOutcome = (outcome: string) =>
        verification.filter(
          (item) =>
            item !== null &&
            typeof item === "object" &&
            (item as Record<string, unknown>).outcome === outcome,
        ).length;
      structured = {
        status: typeof res.status === "string" ? res.status : "",
        changesCount: Array.isArray(res.changes) ? res.changes.length : 0,
        passed: countOutcome("passed"),
        failed: countOutcome("failed"),
        notRun: countOutcome("not_run"),
        openIssues: Array.isArray(res.open_issues)
          ? res.open_issues.filter((item): item is string => typeof item === "string")
          : [],
      };
    }
    return {
      status: typeof obj.status === "string" && obj.status ? obj.status : undefined,
      delivery: typeof obj.delivery === "string" && obj.delivery ? obj.delivery : undefined,
      model: typeof obj.model === "string" && obj.model ? obj.model : undefined,
      note: typeof obj.note === "string" && obj.note ? obj.note : undefined,
      report: typeof obj.report === "string" && obj.report ? obj.report : undefined,
      structured,
    };
  } catch {
    return null;
  }
}

export type AskAnswerPair = { key: string; answer: string; unanswered: boolean };

const ASK_UNANSWERED = "The user did not answer.";
const ASK_TIMEOUT = "The user did not answer within the time limit; continue without an answer.";

/** 解析 `ask` 问答对（与 TUI `ask_answer_pairs` 同规则）。 */
export function parseAskAnswers(tool: ToolBlock): AskAnswerPair[] {
  if (!tool.result) return [];
  const trimmed = tool.result.trimEnd();
  if (!trimmed) return [];
  if (trimmed === ASK_TIMEOUT) {
    return [{ key: "", answer: t("tool.ask_timeout"), unanswered: true }];
  }
  if (trimmed === ASK_UNANSWERED) {
    return [{ key: "", answer: t("tool.ask_unanswered"), unanswered: true }];
  }
  const args = parseArgs(tool.arguments);
  const questions = (Array.isArray(args.questions) ? args.questions : []).map((question) => {
    const obj =
      question && typeof question === "object" ? (question as Record<string, unknown>) : {};
    const id = typeof obj.id === "string" ? obj.id : "";
    const header = typeof obj.header === "string" ? obj.header.trim() : "";
    return { id, key: header || id };
  });
  const lines = trimmed.split("\n");
  const start = trimmed.startsWith("User answers:") ? 1 : 0;
  const pairs: { id: string; answer: string }[] = [];
  for (let index = start; index < lines.length; index++) {
    const line = lines[index];
    const matched = questions.find((q) => q.id && line.startsWith(`${q.id}: `));
    if (matched) {
      pairs.push({ id: matched.id, answer: line.slice(matched.id.length + 2) });
    } else if (pairs.length > 0) {
      pairs[pairs.length - 1].answer += ` ${line.trim()}`;
    } else if (questions.length === 1) {
      pairs.push({ id: questions[0].id, answer: line });
    }
  }
  return pairs.map((pair, index) => {
    const key =
      questions.find((q) => q.id === pair.id)?.key || questions[index]?.key || t("tool.ask_answer");
    const unanswered = pair.answer === ASK_UNANSWERED;
    return {
      key,
      answer: unanswered ? t("tool.ask_unanswered") : pair.answer,
      unanswered,
    };
  });
}

// ---------- 合并 ----------

export type ActivityItem =
  | { kind: "block"; block: Block }
  | { kind: "group"; id: string; name: string; tools: ToolBlock[] };

/** 同一模型响应（batchId 相同）里连续的同名、可合并工具收成一组；单个调用保持原样。 */
export function groupActivity(blocks: Block[]): ActivityItem[] {
  const items: ActivityItem[] = [];
  for (const block of blocks) {
    const last = items.at(-1);
    const previous =
      last?.kind === "group" ? last.tools.at(-1) : last?.block.kind === "tool" ? last.block : null;
    if (
      block.kind === "tool" &&
      previous &&
      block.batchId !== undefined &&
      previous.batchId === block.batchId &&
      previous.name === block.name &&
      groupable(block.name) &&
      !toolFailed(previous) &&
      !toolFailed(block) &&
      previous.images.length === 0 &&
      block.images.length === 0 &&
      toolNotices(previous.result ?? "").length === 0 &&
      toolNotices(block.result ?? "").length === 0
    ) {
      if (last?.kind === "group") last.tools.push(block);
      else
        items[items.length - 1] = {
          kind: "group",
          id: previous.id,
          name: block.name,
          tools: [previous, block],
        };
      continue;
    }
    items.push({ kind: "block", block });
  }
  return items;
}
