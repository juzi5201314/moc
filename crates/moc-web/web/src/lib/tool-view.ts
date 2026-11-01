// 工具块的呈现规则：每个工具在三档（隐藏 / 简略 / 完整）下露出什么，以及把工具
// 结果文本解析成可渲染的结构。规则与 TUI（crates/moc-tui/src/render/scrollback.rs）
// 同一套：read 只占一行、grep/find 只报数量、bash 给单行命令 + 末尾 4 行输出、
// edit/write 给最多 6 行 diff。全部是纯函数，组件只负责画。

import type { Block, ToolBlock } from "@/lib/transcript-model";
import { activitySnippet } from "@/lib/turn";
import type { TodoPhase, TodoStatus, ToolDiff, WebDetailLevel } from "@/protocol/types";

export type DetailLevel = WebDetailLevel;

/** 简略档 bash 输出的末尾行数。 */
export const BRIEF_OUTPUT_ROWS = 4;
/** 简略档正文（diff、问答、通用预览）的最多行数。 */
export const BRIEF_BODY_ROWS = 6;
/** 简略档思考块露出的末尾行数。 */
export const BRIEF_THINKING_ROWS = 4;

/**
 * 简略档正文策略：
 * - `title`：只有标题行（read、web、todo 与未知工具）
 * - `count`：标题行尾注报数量，不显示结果（grep、find）
 * - `command`：单行命令 + 末尾输出（bash、run_code）
 * - `diff`：最多 {@link BRIEF_BODY_ROWS} 行 diff（edit、write）
 * - `preview`：结果开头若干行（ask、子代理类）
 */
export type BodyPolicy = "title" | "count" | "command" | "diff" | "preview";

const POLICY: Record<string, BodyPolicy> = {
  read: "title",
  grep: "count",
  find: "count",
  bash: "command",
  run_code: "command",
  edit: "diff",
  write: "diff",
  todo: "title",
  web_search: "title",
  web_fetch: "title",
  ask: "preview",
  spawn_agent: "preview",
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
  return policy === "title" || policy === "count";
}

export function toolFailed(tool: ToolBlock): boolean {
  return tool.isError || tool.status === "error";
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
  /** 主体之后的次级范围（grep/find 的搜索根）。 */
  scope?: string;
  /** 主体是代码性质（路径、模式），用等宽字体。 */
  mono: boolean;
  /** 右侧尾注：数量、退出码。 */
  stat?: string;
  /** 尾注是失败信息。 */
  statError?: boolean;
  /** 覆盖默认动作名。 */
  verb?: string;
};

export function toolHeadline(tool: ToolBlock): Headline {
  const args = parseArgs(tool.arguments);
  const result = tool.result;
  const failed = toolFailed(tool);
  switch (tool.name) {
    case "read": {
      const range = readRange(args, result);
      return { target: `${str(args, "path")}${range}`, mono: true };
    }
    case "grep":
      return {
        target: str(args, "pattern"),
        scope: str(args, "path") || undefined,
        mono: true,
        stat: result !== undefined && !failed ? grepCountText(result) : undefined,
      };
    case "find":
      return {
        target: strings(args, "glob").join(" "),
        scope: str(args, "path") || undefined,
        mono: true,
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
        mono: !description,
        stat: exit ?? undefined,
        statError: exit !== null,
      };
    }
    case "run_code":
      return { target: str(args, "description"), mono: false };
    case "edit":
    case "write":
      return { target: str(args, "path"), mono: true };
    case "todo": {
      const op = str(args, "op");
      const phases = result !== undefined && !failed ? parseTodo(result) : [];
      const items = phases.flatMap((phase) => phase.items);
      return {
        verb: op === "view" ? "查看任务" : "更新任务",
        target: "",
        mono: false,
        stat: items.length
          ? `${items.filter((item) => item.status === "completed").length}/${items.length}`
          : undefined,
      };
    }
    case "web_search": {
      const hits = result !== undefined && !failed ? parseSearch(result).hits.length : 0;
      return {
        target: str(args, "query"),
        mono: false,
        stat: hits ? count(hits, "result", "results") : undefined,
      };
    }
    case "web_fetch": {
      const urls = strings(args, "urls");
      return {
        target: urls.map(displayUrl).join(" "),
        mono: true,
        stat: urls.length > 1 ? count(urls.length, "page", "pages") : undefined,
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
        mono: false,
        stat: questions.length > 1 ? `共 ${questions.length} 问` : undefined,
      };
    }
    default:
      return { target: activitySnippet(tool, 200), mono: true };
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

// ---------- read ----------

export type ReadLine = { line?: number; text: string };
export type ReadResult = { path?: string; lines: ReadLine[]; notes: string[] };

const NUMBERED = /^(\d+)│(.*)$/;
/** read 末尾的提示行：分页、快照超限、编码、不可编辑说明。超长行占位 `[Line N is …]` 是正文。 */
const NOTICE = /^\[(Showing lines |File exceeds |Invalid UTF-8 )|^Lines shown as a size notice/;

/**
 * read 结果：`[path#TAG]` 标题行、`N│text` 正文行（目录与资源没有行号），
 * 末尾的分页 / 尺寸提示进 notes（crates/moc-core/src/tools/read.rs）。
 */
export function parseRead(result: string): ReadResult {
  const rows = result.split("\n");
  let path: string | undefined;
  const header = /^\[([^\]]+)\]$/.exec(rows[0] ?? "");
  if (header) {
    path = header[1].replace(/#[0-9a-fA-F]{4}$/, "");
    rows.shift();
  }
  const notes: string[] = [];
  while (rows.length > 0) {
    const last = rows.at(-1) as string;
    if (last === "") rows.pop();
    else if (NOTICE.test(last)) notes.unshift(rows.pop() as string);
    else break;
  }
  const lines = rows.map((row): ReadLine => {
    const numbered = NUMBERED.exec(row);
    if (numbered) return { line: Number(numbered[1]), text: numbered[2] };
    const placeholder = /^\[Line (\d+) is /.exec(row);
    return placeholder ? { line: Number(placeholder[1]), text: row } : { text: row };
  });
  return { path, lines, notes };
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

// ---------- web_search ----------

export type SearchHit = { title: string; url: string; snippet?: string };

/** web_search 结果（crates/moc-core/src/tools/websearch.rs 的 `render_search`）。 */
export function parseSearch(result: string): { answer: string; hits: SearchHit[] } {
  const marker = result.search(/^## Sources \(\d+\)$/m);
  if (marker < 0) return { answer: result.trim(), hits: [] };
  const answer = result.slice(0, marker).trim();
  const hits: SearchHit[] = [];
  for (const line of result.slice(marker).split("\n").slice(1)) {
    const head = /^\[\d+\] (.*)$/.exec(line);
    if (head) {
      hits.push({ title: head[1], url: "" });
      continue;
    }
    const hit = hits.at(-1);
    const body = line.startsWith("    ") ? line.slice(4) : null;
    if (!hit || body === null) continue;
    if (!hit.url) hit.url = body;
    else hit.snippet = hit.snippet ? `${hit.snippet} ${body}` : body;
  }
  return { answer, hits };
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

/** 档位循环：隐藏 → 简略 → 完整，与 TUI 的 Ctrl+O 一致。 */
export const NEXT_LEVEL: Record<DetailLevel, DetailLevel> = {
  hidden: "brief",
  brief: "full",
  full: "hidden",
};

export const LEVEL_LABEL: Record<DetailLevel, string> = {
  hidden: "隐藏",
  brief: "简略",
  full: "完整",
};

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
      groupable(block.name)
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
