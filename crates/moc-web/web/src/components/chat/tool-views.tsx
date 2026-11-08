// 工具正文的各类视图：终端、diff、带行号的代码、grep 分组、搜索结果等。
// 取哪种视图、露出多少由 lib/tool-view.ts 的档位规则决定；这里只负责画。
// 视觉遵循 DESIGN.md：灰底圆角卡、不描边、等宽正文 text-xs，元信息用 tertiary。

import AnsiModule from "ansi-to-react";
import { CheckIcon, CopyIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { MessageResponse } from "@/components/ai-elements/message";
import { TodoPhases } from "@/components/common/todo-phases";
import { DiffBody, DiffStat, diffStats } from "@/components/panels/diff-file";
import { highlight } from "@/lib/highlight";
import {
  agentPreviews,
  BRIEF_BODY_ROWS,
  BRIEF_OUTPUT_ROWS,
  bodyPolicy,
  type DetailLevel,
  fetchedPages,
  headLines,
  languageOf,
  parseArgs,
  parseSearch,
  parseTodo,
  previewDiff,
  tailLines,
  toolFailed,
  toolHeadline,
  toolNotices,
} from "@/lib/tool-view";
import type { ToolBlock } from "@/lib/transcript-model";
import { useIsDark } from "@/lib/use-dark";
import { cn } from "@/lib/utils";
import type { ToolDiff } from "@/protocol/types";

// ansi-to-react 是带 `__esModule` 的 CJS 包；本包 `type: module`，Vite 8 按 Node 语义把默认导入
// 解析成整个 `module.exports`，组件在其 `.default` 上。TS 的 bundler 解析仍按 Babel 语义给类型，故此处转一次。
const ansiExports = AnsiModule as unknown as { default: typeof AnsiModule };
const Ansi = ansiExports.default;

export function CopyButton({ text, label = "复制" }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      aria-label={label}
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
  return (
    <button
      type="button"
      onClick={onExpand}
      className="block px-3 py-0.5 text-left font-sans text-2xs text-text-tertiary transition-colors hover:text-foreground"
    >
      ⋯ 还有 {hidden} 行
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

function Notes({ notes }: { notes: string[] }) {
  if (notes.length === 0) return null;
  return (
    <div
      role="status"
      className="mt-1 space-y-0.5 px-1 text-xs whitespace-pre-wrap text-warn [overflow-wrap:anywhere]"
    >
      {notes.map((note) => (
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
  return (
    <div className={CARD}>
      <div className="group/cmd flex items-start gap-2 px-3 py-2">
        <span className="select-none text-text-tertiary">$</span>
        <pre className="max-h-40 min-w-0 flex-1 overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] text-foreground">
          {command}
        </pre>
        <span className="opacity-0 transition-opacity group-hover/cmd:opacity-100">
          <CopyButton text={command} label="复制命令" />
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
      {tail.lines.map((line, index) => (
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
  if (level !== "full") {
    const preview = previewDiff(diff, BRIEF_BODY_ROWS);
    return (
      <div className="overflow-hidden rounded-xl bg-code">
        {preview.diff.files.map((file) => (
          <DiffBody key={file.path} file={file} mode="unified" />
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
      {diff.truncated && (
        <div className="px-1 text-2xs text-text-tertiary">⋯ 其余改动超出 diff 上限，未显示</div>
      )}
    </div>
  );
}

// ---------- 搜索类 ----------

function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

function SearchView({ result }: { result: string }) {
  const { answer, hits } = useMemo(() => parseSearch(result), [result]);
  return (
    <div className="space-y-2">
      {answer && (
        <div className="rounded-xl bg-code px-3.5 py-2.5 [--moc-prose-color:var(--muted-foreground)] [--moc-prose-size:13px]">
          <MessageResponse>{answer}</MessageResponse>
        </div>
      )}
      {hits.length > 0 && (
        <div className="space-y-0.5">
          {hits.map((hit) => (
            <a
              key={hit.url || hit.title}
              href={hit.url}
              target="_blank"
              rel="noreferrer"
              className="group/hit block rounded-lg px-2 py-1.5 transition-colors"
            >
              <div className="flex min-w-0 items-baseline gap-2">
                <span className="min-w-0 truncate text-ui text-foreground group-hover/hit:text-brand">
                  {hit.title}
                </span>
                <span className="shrink-0 text-2xs text-text-tertiary">{hostOf(hit.url)}</span>
              </div>
              {hit.snippet && (
                <p className="line-clamp-2 text-xs text-muted-foreground">{hit.snippet}</p>
              )}
            </a>
          ))}
        </div>
      )}
    </div>
  );
}

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

// ---------- 分发 ----------

/** 失败时露出的一行原因；隐藏 / 简略档也给，失败不能被藏起来。 */
function ErrorLine({ text }: { text: string }) {
  return (
    <div className="truncate font-mono text-xs text-destructive/90">
      {text.trim().split("\n", 1)[0]}
    </div>
  );
}

function PreviewLines({ text, onExpand }: { text: string; onExpand: () => void }) {
  const head = headLines(text, BRIEF_BODY_ROWS);
  if (head.lines.length === 0) return null;
  return (
    <div className={cn(CARD, "py-1.5")}>
      {head.lines.map((line, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: 只读预览，行序即身份
        <div key={index} className="truncate px-3 whitespace-pre text-muted-foreground">
          {line || " "}
        </div>
      ))}
      {head.hidden > 0 && <MoreLines hidden={head.hidden} onExpand={onExpand} />}
    </div>
  );
}

/** 固定摘要没有展开按钮；Markdown 只渲染开头，视觉上最多三行。 */
function fixedSummary(tool: ToolBlock): ReactNode {
  const pages = fetchedPages(tool.result ?? "");
  const previews = agentPreviews(tool);
  const notices = toolNotices(tool.result ?? "");
  if (!pages.length && !previews.length && !notices.length) return null;
  return (
    <div className="space-y-2">
      {pages.map((page) => (
        <div key={page.url} className="text-xs leading-5 [overflow-wrap:anywhere]">
          <span className="text-muted-foreground">{page.title || page.url}</span>
          {page.tokens !== undefined && (
            <span className="ml-2 tabular-nums text-text-tertiary">
              约 {page.tokens.toLocaleString()} tokens
            </span>
          )}
          {page.truncated && (
            <div role="status" className="text-warn">
              警告：内容已截断
            </div>
          )}
          {page.error && (
            <div role="status" className="text-destructive">
              抓取失败：{page.error}
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
            {preview.text && <div className="mt-0.5 text-2xs text-text-tertiary">仅显示开头</div>}
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
  const args = parseArgs(tool.arguments);
  const result = tool.result ?? tool.liveOutput;
  const failed = toolFailed(tool);
  const running = tool.status === "running";
  const arg = (key: string) => (typeof args[key] === "string" ? (args[key] as string) : "");

  if (tool.name === "bash") {
    const command = arg("command");
    if (level === "hidden") return null;
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

  if (tool.name === "run_code") {
    if (level === "hidden") return null;
    if (failed && result) {
      return level === "full" ? <PlainOutput text={result} error /> : <ErrorLine text={result} />;
    }
    if (level === "brief") return null;
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
    if (level === "hidden") return null;
    if (failed && result) {
      return level === "full" ? <PlainOutput text={result} error /> : <ErrorLine text={result} />;
    }
    const phases = result ? parseTodo(result) : [];
    return phases.length ? (
      <TodoPhases phases={phases} className="rounded-xl bg-code px-1 py-2" />
    ) : result ? (
      <PlainOutput text={result} />
    ) : null;
  }

  if (failed && result) {
    return level === "full" ? <PlainOutput text={result} error /> : <ErrorLine text={result} />;
  }

  const policy = bodyPolicy(tool.name);
  if (policy === "summary" || policy === "agent-preview") return fixedSummary(tool);

  if (tool.name === "edit" || tool.name === "write") {
    if (level === "hidden") return null;
    if (tool.diff?.files.length)
      return <DiffCard diff={tool.diff} level={level} onExpand={onExpand} />;
    // 旧版会话的 write 没有持久化 diff：按写入内容显示，edit 只剩入参。
    if (level === "brief") return null;
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

  if (level !== "full") {
    if (level === "brief" && bodyPolicy(tool.name) === "preview") {
      return result ? <PreviewLines text={result} onExpand={onExpand} /> : null;
    }
    return null;
  }

  if (result === undefined) return tool.arguments ? <ArgsTable raw={tool.arguments} /> : null;
  switch (tool.name) {
    case "web_search":
      return <SearchView result={result} />;
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
