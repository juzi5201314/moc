// 工具正文的各类视图：终端、diff、带行号的代码、grep 分组、搜索结果等。
// 取哪种视图、露出多少由 lib/tool-view.ts 的档位规则决定；这里只负责画。
// 视觉遵循 DESIGN.md：灰底圆角卡、不描边、等宽正文 text-xs，元信息用 tertiary。

import AnsiModule from "ansi-to-react";
import { CheckIcon, CopyIcon, Loader2Icon } from "lucide-react";
import { createContext, type ReactNode, useContext, useEffect, useMemo, useState } from "react";
import { MessageResponse } from "@/components/ai-elements/message";
import { TodoPhases } from "@/components/common/todo-phases";
import { DiffBody, DiffStat, diffStats } from "@/components/panels/diff-file";
import { formatDuration } from "@/lib/format";
import { highlight } from "@/lib/highlight";
import { t, useT } from "@/lib/i18n";
import {
  type AskAnswerPair,
  agentPreviews,
  BRIEF_BODY_ROWS,
  BRIEF_OUTPUT_ROWS,
  bodyPolicy,
  type DetailLevel,
  fetchedPages,
  headLines,
  languageOf,
  PREFER_BUILTIN_TAG,
  parseArgs,
  parseAskAnswers,
  parseMessageReceipts,
  parseSidekickResult,
  parseTodo,
  parseWaitResult,
  previewDiff,
  type SidekickParsedResult,
  type SidekickStructuredResult,
  tailLines,
  toolFailed,
  toolHeadline,
  toolNotices,
} from "@/lib/tool-view";
import type { ToolBlock } from "@/lib/transcript-model";
import { useIsDark } from "@/lib/use-dark";
import { cn } from "@/lib/utils";
import type { AgentMessageReceipt, AgentStatus, ToolDiff } from "@/protocol/types";

// ansi-to-react 是带 `__esModule` 的 CJS 包；本包 `type: module`，Vite 8 按 Node 语义把默认导入
// 解析成整个 `module.exports`，组件在其 `.default` 上；vitest 的 SSR 变换则直接给组件本身。
// TS 的 bundler 解析仍按 Babel 语义给类型，故此处转一次。
const ansiExports = AnsiModule as unknown as { default?: typeof AnsiModule };
const Ansi = ansiExports.default ?? AnsiModule;

export function CopyButton({ text, label }: { text: string; label?: string }) {
  const t = useT();
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      aria-label={label ?? t("common.copy")}
      onClick={(event) => {
        event.stopPropagation();
        void navigator.clipboard.writeText(text);
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1200);
      }}
      className="flex size-6 items-center justify-center rounded-md text-text-tertiary transition-colors hover:text-foreground"
    >
      {copied ? <CheckIcon className="size-3.5 text-success" /> : <CopyIcon className="size-3.5" />}
    </button>
  );
}

/** 「⋯ 还有 N 行」：简略档省略提示，点击切到完整档。 */
function MoreLines({ hidden, onExpand }: { hidden: number; onExpand: () => void }) {
  const t = useT();
  return (
    <button
      type="button"
      onClick={onExpand}
      className="block px-3 py-0.5 text-left font-sans text-2xs text-text-tertiary transition-colors hover:text-foreground"
    >
      {t("tv.more", { n: hidden })}
    </button>
  );
}

const CARD = "overflow-hidden rounded-xl bg-code font-mono text-xs leading-5";

/** 带行号槽的代码：shiki 高亮（按需加载语言），高亮好之前先按纯文本显示。 */
function CodeView({
  lines,
  language,
  maxHeight = 360,
}: {
  lines: { line?: number; text: string }[];
  language?: string;
  maxHeight?: number;
}) {
  const dark = useIsDark();
  const code = useMemo(() => lines.map((line) => line.text).join("\n"), [lines]);
  const [html, setHtml] = useState<string | null>(null);
  useEffect(() => {
    setHtml(null);
    if (!language) return;
    let live = true;
    void highlight(code, language, dark).then((value) => {
      if (live) setHtml(value);
    });
    return () => {
      live = false;
    };
  }, [code, language, dark]);
  const numbered = lines.some((line) => line.line !== undefined);
  return (
    <div className={cn(CARD, "flex overflow-auto py-1.5")} style={{ maxHeight }}>
      {numbered && (
        <div className="sticky left-0 shrink-0 select-none bg-code pr-3 pl-3 text-right text-text-tertiary/70">
          {lines.map((line, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: 只读快照，行序即身份
            <div key={index}>{line.line ?? ""}</div>
          ))}
        </div>
      )}
      {html ? (
        <div
          className={cn("min-w-0 flex-1 pr-4 [&_pre]:!bg-transparent", !numbered && "pl-3")}
          // biome-ignore lint/security/noDangerouslySetInnerHtml: shiki 输出已转义源码
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className={cn("min-w-0 flex-1 pr-4 text-muted-foreground", !numbered && "pl-3")}>
          {code}
        </pre>
      )}
    </div>
  );
}

function PlainOutput({
  text,
  error,
  maxHeight = 320,
}: {
  text: string;
  error?: boolean;
  maxHeight?: number;
}) {
  return (
    <pre
      className={cn(
        CARD,
        "overflow-auto px-3 py-2 whitespace-pre-wrap [overflow-wrap:anywhere]",
        error ? "text-destructive" : "text-muted-foreground",
      )}
      style={{ maxHeight }}
    >
      {text}
    </pre>
  );
}

const AncestorNotices = createContext<ReadonlySet<string>>(new Set());

/** 只向子树传播原始通知身份，不改写工具结果；父结果更新会自然刷新子树。 */
export function NoticeScope({ result, children }: { result?: string; children: ReactNode }) {
  const inherited = useContext(AncestorNotices);
  const notices = useMemo(
    () => new Set([...inherited, ...toolNotices(result ?? "")]),
    [inherited, result],
  );
  return <AncestorNotices.Provider value={notices}>{children}</AncestorNotices.Provider>;
}

function Notes({ notes }: { notes: string[] }) {
  const t = useT();
  const inherited = useContext(AncestorNotices);
  const [expanded, setExpanded] = useState(false);
  const visible = [...new Set(notes)].filter((note) => !inherited.has(note));
  const omitted = visible.filter(
    (note) => note.trimStart().startsWith("[Line ") && note.includes("content omitted;"),
  );
  const ordinary = visible.filter(
    (note) =>
      !omitted.includes(note) &&
      !note.startsWith("Lines shown as a size notice") &&
      !note.startsWith("[Long lines omitted;"),
  );
  if (!omitted.length && !ordinary.length) return null;
  return (
    <div
      role="status"
      className="mt-1 flex flex-col gap-0.5 px-1 text-xs whitespace-pre-wrap text-warn [overflow-wrap:anywhere]"
    >
      {omitted.length > 0 && (
        <>
          {/* 复用工具正文的轻量展开按钮，不为只读通知引入整套折叠容器。 */}
          <button
            type="button"
            aria-expanded={expanded}
            className="text-left"
            onClick={() => setExpanded((open) => !open)}
          >
            {expanded
              ? t("tv.omitted_hide", { n: omitted.length })
              : t("tv.omitted_show", { n: omitted.length })}
          </button>
          {expanded && (
            <>
              {omitted.map((note) => (
                <div key={note}>{note.split(";", 1)[0].replace(/^\[/, "")}</div>
              ))}
              <div>{t("tv.read_hint")}</div>
            </>
          )}
        </>
      )}
      {ordinary.map((note) => (
        <div key={note}>{note}</div>
      ))}
    </div>
  );
}

// ---------- 终端 ----------

/** 完整档终端卡：命令（可多行）+ ANSI 着色输出，超高内部滚动。 */
function TerminalCard({
  command,
  output,
  error,
  running,
}: {
  command: string;
  output?: string;
  error?: boolean;
  running?: boolean;
}) {
  const t = useT();
  return (
    <div className={CARD}>
      <div className="group/cmd flex items-start gap-2 px-3 py-2">
        <span className="select-none text-text-tertiary">$</span>
        <pre className="max-h-40 min-w-0 flex-1 overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] text-foreground">
          {command}
        </pre>
        <span className="opacity-0 transition-opacity group-hover/cmd:opacity-100">
          <CopyButton text={command} label={t("tv.copy_cmd")} />
        </span>
      </div>
      {(output || running) && (
        <pre
          className={cn(
            "group/out relative max-h-80 overflow-auto border-t border-border-subtle px-3 py-2 whitespace-pre-wrap [overflow-wrap:anywhere]",
            error ? "text-destructive" : "text-muted-foreground",
          )}
        >
          {output ? <Ansi>{output}</Ansi> : "…"}
        </pre>
      )}
    </div>
  );
}

/** 简略档：单行命令（标题已是命令本身时省略）+ 末尾 {@link BRIEF_OUTPUT_ROWS} 行输出。 */
function CommandBrief({
  command,
  showCommand,
  output,
  error,
  onExpand,
}: {
  command: string;
  showCommand: boolean;
  output?: string;
  error?: boolean;
  onExpand: () => void;
}) {
  const tail = output ? tailLines(output, BRIEF_OUTPUT_ROWS) : { lines: [], hidden: 0 };
  const notices = toolNotices(output ?? "");
  if (!showCommand && tail.lines.length === 0) return null;
  return (
    <div className={cn(CARD, "py-1.5")}>
      {showCommand && (
        <div className="truncate px-3 text-foreground">
          <span className="mr-2 select-none text-text-tertiary">$</span>
          {command.replace(/\s+/g, " ").trim()}
        </div>
      )}
      {tail.hidden > 0 && <MoreLines hidden={tail.hidden} onExpand={onExpand} />}
      {tail.lines
        .filter((line) => !notices.includes(line))
        .map((line, index) => (
          <div
            // biome-ignore lint/suspicious/noArrayIndexKey: 输出行只读，行序即身份
            key={index}
            className={cn(
              "truncate px-3 whitespace-pre",
              error ? "text-destructive" : "text-muted-foreground",
            )}
          >
            <Ansi>{line || " "}</Ansi>
          </div>
        ))}
      <Notes notes={notices} />
    </div>
  );
}

/**
 * 策略拒绝按建议而非失败展示：说明句用警告色，`- <tool> {…}` 建议行拆出工具名，
 * 不用 destructive 红；简略档与完整档都完整保留说明与建议（不能 tail 截断）。
 */
function PreferBuiltinView({
  command,
  showCommand,
  result,
}: {
  command: string;
  showCommand: boolean;
  result: string;
}) {
  const t = useT();
  const lines = result.replace(/^\[E_PREFER_BUILTIN\] ?/, "").split("\n");
  return (
    <div className={CARD}>
      {showCommand && (
        <div className="group/cmd flex items-start gap-2 px-3 py-2">
          <span className="select-none text-text-tertiary">$</span>
          <pre className="max-h-40 min-w-0 flex-1 overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] text-foreground">
            {command}
          </pre>
          <span className="opacity-0 transition-opacity group-hover/cmd:opacity-100">
            <CopyButton text={command} label={t("tv.copy_cmd")} />
          </span>
        </div>
      )}
      <div className={cn("px-3 py-1.5", showCommand && "border-t border-border-subtle")}>
        {lines.map((line, index) => {
          const suggestion = /^- (\S+) (.*)$/.exec(line);
          return (
            <div
              // biome-ignore lint/suspicious/noArrayIndexKey: 只读文本，行序即身份
              key={index}
              className={cn(
                "truncate whitespace-pre",
                index === 0 ? "text-warn" : "text-muted-foreground",
              )}
            >
              {suggestion ? (
                <>
                  <span className="text-text-tertiary">- </span>
                  <span className="text-foreground">{suggestion[1]}</span> {suggestion[2]}
                </>
              ) : (
                line
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ---------- diff ----------

function DiffCard({
  diff,
  level,
  onExpand,
}: {
  diff: ToolDiff;
  level: DetailLevel;
  onExpand: () => void;
}) {
  const t = useT();
  if (level !== "full") {
    const preview = previewDiff(diff, BRIEF_BODY_ROWS);
    return (
      <div className="overflow-hidden rounded-xl bg-code">
        {preview.diff.files.map((file) => (
          <DiffBody key={file.path} file={file} />
        ))}
        {preview.hidden > 0 && (
          <div className="pb-1">
            <MoreLines hidden={preview.hidden} onExpand={onExpand} />
          </div>
        )}
      </div>
    );
  }
  const multi = diff.files.length > 1;
  return (
    <div className="space-y-2">
      {diff.files.map((file) => (
        <div key={file.path} className="overflow-hidden rounded-xl bg-code">
          {multi && (
            <div className="flex h-8 items-center gap-2 px-3 text-xs">
              <span
                className="min-w-0 flex-1 truncate font-mono text-muted-foreground"
                title={file.path}
              >
                {file.path}
              </span>
              <DiffStat {...diffStats(file)} />
            </div>
          )}
          <DiffBody file={file} maxHeight={420} />
        </div>
      ))}
      {diff.truncated && <div className="px-1 text-2xs text-text-tertiary">{t("tv.diff_cap")}</div>}
    </div>
  );
}

// ---------- 搜索类 ----------

/** 未知工具的入参：键值表，长值折行、对象按 JSON 展开。 */
function ArgsTable({ raw }: { raw: string }) {
  const args = parseArgs(raw);
  const entries = Object.entries(args);
  if (entries.length === 0)
    return raw.trim() && raw.trim() !== "{}" ? <PlainOutput text={raw} /> : null;
  return (
    <div className={cn(CARD, "max-h-72 overflow-auto px-3 py-2")}>
      {entries.map(([key, value]) => (
        <div key={key} className="grid grid-cols-[minmax(4rem,auto)_1fr] gap-3">
          <span className="text-text-tertiary">{key}</span>
          <span className="min-w-0 whitespace-pre-wrap [overflow-wrap:anywhere] text-muted-foreground">
            {typeof value === "string" ? value : JSON.stringify(value, null, 2)}
          </span>
        </div>
      ))}
    </div>
  );
}

/** 未知工具的结果：JSON 美化高亮，否则纯文本。 */
function GenericResult({ text, error }: { text: string; error: boolean }) {
  const pretty = useMemo(() => {
    try {
      const value: unknown = JSON.parse(text);
      return typeof value === "object" && value !== null ? JSON.stringify(value, null, 2) : null;
    } catch {
      return null;
    }
  }, [text]);
  if (pretty && !error) {
    return <CodeView lines={pretty.split("\n").map((line) => ({ text: line }))} language="json" />;
  }
  return <PlainOutput text={text} error={error} />;
}

// ---------- 子代理 / ask 专属视图 ----------

type WaitRowItem = {
  id: string;
  title: string;
  status: AgentStatus;
  elapsedMs: number;
  activity?: string | null;
};

function agentStatusTone(status: AgentStatus | string): { dot: string; text: string } {
  switch (status) {
    case "running":
    case "queued":
      return { dot: "bg-brand", text: "text-brand" };
    case "completed":
      return { dot: "bg-success", text: "text-success" };
    case "failed":
      return { dot: "bg-destructive", text: "text-destructive" };
    default:
      return { dot: "bg-text-tertiary", text: "text-text-tertiary" };
  }
}

function AgentStatusRows({
  rows,
  level,
  onExpand,
}: {
  rows: WaitRowItem[];
  level: DetailLevel;
  onExpand: () => void;
}) {
  if (rows.length === 0) return null;
  const shown = level === "full" ? rows : rows.slice(0, BRIEF_BODY_ROWS);
  const hidden = rows.length - shown.length;
  return (
    <div className={cn(CARD, "space-y-1 px-3 py-2 font-sans")}>
      {shown.map((row) => {
        const tone = agentStatusTone(row.status);
        return (
          <div key={row.id} className="flex items-center gap-2 text-xs">
            {row.status === "running" || row.status === "queued" ? (
              <Loader2Icon className="size-3 shrink-0 animate-spin text-brand" />
            ) : (
              <span className={cn("size-1.5 shrink-0 rounded-full", tone.dot)} />
            )}
            <span className="min-w-0 truncate text-foreground">{row.title}</span>
            <span className={cn("shrink-0 text-2xs", tone.text)}>{row.status}</span>
            {row.elapsedMs > 0 && (
              <span className="shrink-0 font-mono text-2xs tabular-nums text-text-tertiary">
                {formatDuration(row.elapsedMs)}
              </span>
            )}
            {row.activity && (
              <span className="min-w-0 flex-1 truncate text-text-tertiary">{row.activity}</span>
            )}
          </div>
        );
      })}
      {hidden > 0 && <MoreLines hidden={hidden} onExpand={onExpand} />}
    </div>
  );
}

function MessageAgentView({
  message,
  receipts,
  level,
  onExpand,
}: {
  message: string;
  receipts: AgentMessageReceipt[] | null;
  level: DetailLevel;
  onExpand: () => void;
}) {
  const showReceipts = Boolean(
    receipts &&
      receipts.length > 0 &&
      (level === "full" ||
        receipts.length > 1 ||
        receipts.some((receipt) => receipt.outcome === "failed")),
  );
  const head = headLines(message, BRIEF_BODY_ROWS);
  const visibleMessage = level === "full" ? message.trimEnd() : head.lines.join("\n");
  const visibleReceipts = showReceipts
    ? level === "full"
      ? (receipts ?? [])
      : (receipts ?? []).slice(0, BRIEF_BODY_ROWS)
    : [];
  const hiddenCount =
    (level === "brief" ? head.hidden : 0) +
    (showReceipts ? (receipts?.length ?? 0) - visibleReceipts.length : 0);
  if (!visibleMessage && visibleReceipts.length === 0) return null;
  return (
    <div className={cn(CARD, "space-y-1.5 px-3 py-2 font-sans")}>
      {visibleMessage && (
        <div className="whitespace-pre-wrap text-xs leading-5 text-foreground [overflow-wrap:anywhere]">
          {visibleMessage}
        </div>
      )}
      {visibleReceipts.length > 0 && (
        <div
          className={cn(
            "space-y-1 text-xs",
            visibleMessage && "border-t border-border-subtle pt-1.5",
          )}
        >
          {visibleReceipts.map((receipt) => {
            const outcomeClass =
              receipt.outcome === "failed"
                ? "text-destructive"
                : receipt.outcome === "injected"
                  ? "text-brand"
                  : "text-text-tertiary";
            return (
              <div key={receipt.agent_id} className="flex items-baseline gap-2">
                <span className="shrink-0 text-muted-foreground">{receipt.title}</span>
                <span className={cn("shrink-0 text-2xs", outcomeClass)}>{receipt.outcome}</span>
                {receipt.error && (
                  <span className="min-w-0 truncate text-2xs text-destructive">
                    {receipt.error}
                  </span>
                )}
              </div>
            );
          })}
        </div>
      )}
      {hiddenCount > 0 && <MoreLines hidden={hiddenCount} onExpand={onExpand} />}
    </div>
  );
}

function sidekickSummaryText(structured: SidekickStructuredResult): string {
  const parts = [t("tool.sk_result", { status: structured.status })];
  if (structured.changesCount > 0) {
    parts.push(t("tool.sk_changes", { n: structured.changesCount }));
  }
  const checks: string[] = [];
  if (structured.passed > 0) checks.push(t("tool.sk_passed", { n: structured.passed }));
  if (structured.failed > 0) checks.push(t("tool.sk_failed", { n: structured.failed }));
  if (structured.notRun > 0) checks.push(t("tool.sk_not_run", { n: structured.notRun }));
  if (checks.length > 0) parts.push(`${t("tool.sk_verify")} ${checks.join(" ")}`);
  return parts.join(" · ");
}

function SidekickView({
  parsed,
  level,
  onExpand,
}: {
  parsed: SidekickParsedResult;
  level: DetailLevel;
  onExpand: () => void;
}) {
  const tone = agentStatusTone(parsed.status ?? "running");
  const issues = parsed.structured?.openIssues ?? [];
  const visibleIssues = level === "full" ? issues : issues.slice(0, BRIEF_BODY_ROWS);
  const reportHead = parsed.report ? headLines(parsed.report, BRIEF_BODY_ROWS) : null;
  const visibleReport =
    parsed.report && (level === "full" ? parsed.report.trimEnd() : reportHead?.lines.join("\n"));
  const hidden =
    issues.length -
    visibleIssues.length +
    (level === "brief" && reportHead ? reportHead.hidden : 0);
  return (
    <div className={cn(CARD, "space-y-1.5 px-3 py-2 font-sans text-xs")}>
      <div className="flex flex-wrap items-center gap-1.5">
        <span className={cn("size-1.5 shrink-0 rounded-full", tone.dot)} />
        <span className="font-medium text-foreground">Sidekick</span>
        {parsed.delivery && <span className="text-text-tertiary">· {parsed.delivery}</span>}
        {parsed.status && <span className={tone.text}>· {parsed.status}</span>}
        {parsed.model && (
          <span className="font-mono text-2xs text-text-tertiary">· {parsed.model}</span>
        )}
      </div>
      {parsed.structured && (
        <div className="text-muted-foreground">{sidekickSummaryText(parsed.structured)}</div>
      )}
      {visibleIssues.length > 0 && (
        <div className="space-y-0.5 text-warn">
          {visibleIssues.map((issue, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: open_issues 是只读快照，顺序即身份
            <div key={index} className="truncate">
              ! {issue}
            </div>
          ))}
        </div>
      )}
      {parsed.note && <div className="text-text-tertiary">{parsed.note}</div>}
      {visibleReport && (
        <div className="whitespace-pre-wrap leading-5 text-foreground [overflow-wrap:anywhere]">
          {visibleReport}
        </div>
      )}
      {hidden > 0 && <MoreLines hidden={hidden} onExpand={onExpand} />}
    </div>
  );
}

function AskAnswersView({
  pairs,
  level,
  onExpand,
}: {
  pairs: AskAnswerPair[];
  level: DetailLevel;
  onExpand: () => void;
}) {
  const shown = level === "full" ? pairs : pairs.slice(0, BRIEF_BODY_ROWS);
  const hidden = pairs.length - shown.length;
  return (
    <div className={cn(CARD, "space-y-1 px-3 py-2 font-sans text-xs")}>
      {shown.map((pair, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: 问答对为只读结果快照
        <div key={index} className="flex items-baseline gap-2">
          {pair.key && <span className="shrink-0 text-text-tertiary">{pair.key}</span>}
          <span
            className={cn(
              "min-w-0 whitespace-pre-wrap [overflow-wrap:anywhere]",
              pair.unanswered ? "text-warn" : "text-foreground",
            )}
          >
            {pair.answer}
          </span>
        </div>
      ))}
      {hidden > 0 && <MoreLines hidden={hidden} onExpand={onExpand} />}
    </div>
  );
}

// ---------- 分发 ----------

/** 简略档显示错误原因；附带通知必须完整保留。 */
function ErrorLine({ text }: { text: string }) {
  const first = text.trim().split("\n", 1)[0];
  const notices = toolNotices(text);
  return (
    <>
      {!notices.includes(first) && (
        <div className="truncate font-mono text-xs text-destructive/90">{first}</div>
      )}
      <Notes notes={notices} />
    </>
  );
}

function PreviewLines({ text, onExpand }: { text: string; onExpand: () => void }) {
  const head = headLines(text, BRIEF_BODY_ROWS);
  const notices = toolNotices(text);
  if (head.lines.length === 0) return null;
  return (
    <div className={cn(CARD, "py-1.5")}>
      {head.lines
        .filter((line) => !notices.includes(line))
        .map((line, index) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: 只读预览，行序即身份
          <div key={index} className="truncate px-3 whitespace-pre text-muted-foreground">
            {line || " "}
          </div>
        ))}
      {head.hidden > 0 && <MoreLines hidden={head.hidden} onExpand={onExpand} />}
      <Notes notes={notices} />
    </div>
  );
}

/** 固定摘要不展示原始正文；隐藏档仍保留通知与逐页警告。 */
function fixedSummary(tool: ToolBlock, warningsOnly = false): ReactNode {
  const pages = fetchedPages(tool.result ?? "").filter(
    (page) => !warningsOnly || page.truncated || page.error,
  );
  const previews = warningsOnly ? [] : agentPreviews(tool);
  const notices = toolNotices(tool.result ?? "");
  if (!pages.length && !previews.length && !notices.length) return null;
  return (
    <div className="space-y-2">
      {pages.map((page) => (
        <div key={page.url} className="text-xs leading-5 [overflow-wrap:anywhere]">
          <span className="text-muted-foreground">{page.title || page.url}</span>
          {page.tokens !== undefined && (
            <span className="ml-2 tabular-nums text-text-tertiary">
              {t("tv.tokens", { n: page.tokens.toLocaleString() })}
            </span>
          )}
          {page.truncated && (
            <div role="status" className="text-warn">
              {t("tv.truncated")}
            </div>
          )}
          {page.error && (
            <div role="status" className="text-destructive">
              {t("tv.fetch_failed", { error: page.error })}
            </div>
          )}
        </div>
      ))}
      {previews.map((preview, index) => {
        const head = headLines(preview.text, BRIEF_BODY_ROWS);
        return (
          // biome-ignore lint/suspicious/noArrayIndexKey: 任务列表是只读调用快照
          <div key={index} className="border-l-2 border-border pl-3">
            <div className="mb-0.5 text-xs font-medium text-muted-foreground">{preview.label}</div>
            <div className="moc-tool-preview max-h-[3.75rem] overflow-hidden text-xs [--moc-prose-size:12px] [--moc-prose-leading:20px] [--moc-prose-color:var(--muted-foreground)]">
              <MessageResponse>{head.lines.join("\n")}</MessageResponse>
            </div>
            {preview.text && (
              <div className="mt-0.5 text-2xs text-text-tertiary">{t("tv.head_only")}</div>
            )}
          </div>
        );
      })}
      <Notes notes={notices} />
    </div>
  );
}

/**
 * 工具正文：按工具策略与档位选视图。返回 null 表示这一档没有正文（只剩标题行）。
 * 子调用、进度行与图片由调用方另行追加。
 */
export function toolBody(tool: ToolBlock, level: DetailLevel, onExpand: () => void): ReactNode {
  if (tool.preview) {
    if (level === "hidden") return null;
    const preview = tool.preview;
    const all = preview.body.split("\n");
    const limit = level === "full" ? all.length : tool.name === "bash" ? 4 : 6;
    const lines = all.slice(-limit).map((text) => ({ text }));
    return (
      <div>
        {(preview.truncated || all.length > limit) && (
          <div className="px-3 py-0.5 text-2xs text-text-tertiary">{t("tv.tail_only")}</div>
        )}
        <CodeView lines={lines} language={preview.language ?? languageOf(preview.target)} />
      </div>
    );
  }
  if (level === "hidden") {
    const result = tool.result ?? "";
    if (tool.name === "bash" && result.startsWith(PREFER_BUILTIN_TAG)) {
      return <PreferBuiltinView command="" showCommand={false} result={result} />;
    }
    return fixedSummary(tool, true);
  }
  const args = parseArgs(tool.arguments);
  const result = tool.result ?? tool.liveOutput;
  const failed = toolFailed(tool);
  const running = tool.status === "running";
  const arg = (key: string) => (typeof args[key] === "string" ? (args[key] as string) : "");

  if (tool.name === "bash") {
    const command = arg("command");
    if (result?.startsWith(PREFER_BUILTIN_TAG)) {
      return (
        <PreferBuiltinView
          command={command}
          showCommand={
            level === "full" ||
            toolHeadline(tool).target !== command.trim() ||
            command.includes("\n")
          }
          result={result}
        />
      );
    }
    if (level === "full") {
      return <TerminalCard command={command} output={result} error={failed} running={running} />;
    }
    const headline = toolHeadline(tool).target;
    return (
      <CommandBrief
        command={command}
        showCommand={headline !== command.trim() || command.includes("\n")}
        output={result}
        error={failed}
        onExpand={onExpand}
      />
    );
  }

  if (tool.name === "execute") {
    if (failed && result) {
      return level === "full" ? <PlainOutput text={result} error /> : <ErrorLine text={result} />;
    }
    if (level === "brief") return <Notes notes={toolNotices(result ?? "")} />;
    const code = arg("code");
    return (
      <div className="space-y-2">
        {code && (
          <CodeView
            lines={code.split("\n").map((text, index) => ({ line: index + 1, text }))}
            language="javascript"
          />
        )}
        {result && <GenericResult text={result} error={false} />}
      </div>
    );
  }

  if (tool.name === "todo") {
    if (failed && result) {
      return level === "full" ? <PlainOutput text={result} error /> : <ErrorLine text={result} />;
    }
    const phases = result ? parseTodo(result) : [];
    return phases.length ? (
      <TodoPhases
        phases={phases}
        brief={level === "brief"}
        className="rounded-xl bg-code px-1 py-2"
      />
    ) : result ? (
      <PlainOutput text={result} />
    ) : null;
  }

  const policy = bodyPolicy(tool.name);
  if (level === "brief" && !failed && policy === "summary" && tool.name !== "web_fetch")
    return fixedSummary(tool, true);
  if (failed && result) {
    return level === "full" ? <PlainOutput text={result} error /> : <ErrorLine text={result} />;
  }

  if (policy === "summary" || policy === "agent-preview") return fixedSummary(tool);

  if (tool.name === "edit" || tool.name === "write") {
    if (tool.diff?.files.length)
      return <DiffCard diff={tool.diff} level={level} onExpand={onExpand} />;
    // 旧版会话的 write 没有持久化 diff：按写入内容显示，edit 只剩入参。
    if (level === "brief") return <Notes notes={toolNotices(result ?? "")} />;
    const content = arg("content");
    if (tool.name === "write" && content) {
      return (
        <CodeView
          lines={content.split("\n").map((text, index) => ({ line: index + 1, text }))}
          language={languageOf(arg("path"))}
        />
      );
    }
    return <ArgsTable raw={tool.arguments} />;
  }

  if (tool.name === "message_agent") {
    const receipts = parseMessageReceipts(result);
    const message = arg("message");
    if (message || receipts) {
      return (
        <MessageAgentView message={message} receipts={receipts} level={level} onExpand={onExpand} />
      );
    }
  }

  if (tool.name === "wait_agent") {
    const rows: WaitRowItem[] = tool.progress?.length
      ? tool.progress.map((row) => ({
          id: row.agent_id,
          title: row.title,
          status: row.status,
          elapsedMs: row.elapsed_ms,
          activity: row.activity,
        }))
      : (parseWaitResult(result)?.agents ?? []).map((entry) => ({
          id: entry.id,
          title: entry.title,
          status: entry.status,
          elapsedMs: 0,
        }));
    if (rows.length > 0) {
      return <AgentStatusRows rows={rows} level={level} onExpand={onExpand} />;
    }
  }

  if (tool.name === "sidekick") {
    if (result === undefined) {
      if (tool.progress?.length) {
        const rows: WaitRowItem[] = tool.progress.map((row) => ({
          id: row.agent_id,
          title: row.title,
          status: row.status,
          elapsedMs: row.elapsed_ms,
          activity: row.activity,
        }));
        return <AgentStatusRows rows={rows} level={level} onExpand={onExpand} />;
      }
      return null;
    }
    const parsed = parseSidekickResult(result);
    if (parsed) return <SidekickView parsed={parsed} level={level} onExpand={onExpand} />;
  }

  if (tool.name === "ask") {
    const pairs = parseAskAnswers(tool);
    if (pairs.length > 0) {
      return <AskAnswersView pairs={pairs} level={level} onExpand={onExpand} />;
    }
  }

  if (level !== "full") {
    if (level === "brief" && bodyPolicy(tool.name) === "preview") {
      return result ? <PreviewLines text={result} onExpand={onExpand} /> : null;
    }
    return <Notes notes={toolNotices(result ?? "")} />;
  }

  if (result === undefined) return tool.arguments ? <ArgsTable raw={tool.arguments} /> : null;
  switch (tool.name) {
    case "ask":
    case "message_agent":
    case "wait_agent":
    case "sidekick":
      return <PlainOutput text={result} />;
    default:
      return (
        <div className="space-y-2">
          <ArgsTable raw={tool.arguments} />
          {result && <GenericResult text={result} error={failed} />}
        </div>
      );
  }
}
