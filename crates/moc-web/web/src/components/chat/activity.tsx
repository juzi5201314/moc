// 执行轨迹：思考、工具、命令逐行排列；点开一行看详情（终端输出、diff、入参与结果）。

import {
  BotIcon,
  BrainIcon,
  CheckIcon,
  ChevronRightIcon,
  CircleAlertIcon,
  CodeIcon,
  CopyIcon,
  FileTextIcon,
  GlobeIcon,
  ImageIcon,
  InfoIcon,
  ListChecksIcon,
  Loader2Icon,
  MessageCircleQuestionIcon,
  PencilLineIcon,
  SearchIcon,
  SquareTerminalIcon,
  WrenchIcon,
} from "lucide-react";
import { type ReactNode, useState } from "react";
import { MessageResponse } from "@/components/ai-elements/message";
import { DiffBody, DiffStat, diffStats } from "@/components/panels/diff-file";
import { formatDuration } from "@/lib/format";
import type { Block, ToolBlock } from "@/lib/transcript-model";
import { type ActivityKind, activityKind, activitySnippet, activityVerb } from "@/lib/turn";
import { cn } from "@/lib/utils";
import type { ImageSource } from "@/protocol/types";
import { usePrefs } from "@/store/prefs";

const KIND_ICON: Record<ActivityKind, ReactNode> = {
  thinking: <BrainIcon />,
  commands: <SquareTerminalIcon />,
  code: <CodeIcon />,
  read: <FileTextIcon />,
  readImage: <ImageIcon />,
  write: <PencilLineIcon />,
  edit: <PencilLineIcon />,
  search: <SearchIcon />,
  webSearch: <GlobeIcon />,
  webFetch: <GlobeIcon />,
  subagents: <BotIcon />,
  plan: <ListChecksIcon />,
  questions: <MessageCircleQuestionIcon />,
  tools: <WrenchIcon />,
  showcase: <WrenchIcon />,
};

function iconFor(block: Block): ReactNode {
  if (block.kind === "status") return <InfoIcon />;
  const kind = activityKind(block);
  return kind ? KIND_ICON[kind] : <WrenchIcon />;
}

export function imageSrc(source: ImageSource): string {
  return "Url" in source
    ? source.Url
    : `data:${source.Base64.media_type};base64,${source.Base64.data}`;
}

function parseArgs(raw: string): Record<string, unknown> | null {
  try {
    const value: unknown = JSON.parse(raw);
    return value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

function CopyButton({ text, label = "复制" }: { text: string; label?: string }) {
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
      className="flex size-6 items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-accent hover:text-foreground"
    >
      {copied ? <CheckIcon className="size-3.5 text-success" /> : <CopyIcon className="size-3.5" />}
    </button>
  );
}

/** 终端卡片：命令行 + 输出，输出超高时内部滚动。 */
function CommandCard({
  command,
  cwd,
  code,
  output,
  isError,
  running,
  durationMs,
}: {
  command: string;
  cwd?: string;
  code?: string;
  output?: string;
  isError?: boolean;
  running?: boolean;
  durationMs?: number;
}) {
  const prompt = cwd?.split(/[\\/]/).filter(Boolean).pop();
  return (
    <div className="overflow-hidden rounded-xl bg-code font-mono text-[12.5px] leading-[19px] ring-1 ring-inset ring-border-subtle">
      <div className="group/cmd flex items-start gap-2 px-3.5 py-2.5">
        <span className="select-none text-text-tertiary">{prompt ? `${prompt} $` : "$"}</span>
        <div className="min-w-0 flex-1">
          <div className="max-h-[120px] overflow-y-auto whitespace-pre-wrap [overflow-wrap:anywhere] text-foreground">
            {command}
          </div>
          {code && (
            <pre className="mt-1 max-h-[200px] overflow-auto whitespace-pre-wrap [overflow-wrap:anywhere] text-muted-foreground">
              {code}
            </pre>
          )}
        </div>
        <span className="opacity-0 transition-opacity group-hover/cmd:opacity-100">
          <CopyButton text={code ?? command} label="复制命令" />
        </span>
      </div>
      {(output || running) && (
        <div className="border-t border-border-subtle">
          <pre
            className={cn(
              "max-h-[260px] overflow-auto px-3 py-2 whitespace-pre-wrap [overflow-wrap:anywhere]",
              isError ? "text-destructive" : "text-muted-foreground",
            )}
          >
            {output || (running ? "…" : "")}
          </pre>
        </div>
      )}
      {!running && (durationMs !== undefined || isError) && (
        <div className="flex items-center gap-2 border-t border-border-subtle px-3 py-1 text-[11px] text-text-tertiary">
          <span
            className={cn("size-1.5 rounded-full", isError ? "bg-destructive" : "bg-success")}
          />
          <span>{isError ? "失败" : "完成"}</span>
          {durationMs !== undefined && <span>· {formatDuration(durationMs)}</span>}
        </div>
      )}
    </div>
  );
}

/** 通用工具卡：入参（JSON 美化）+ 结果。 */
function IoCard({
  input,
  output,
  isError,
}: {
  input?: string;
  output?: string;
  isError?: boolean;
}) {
  return (
    <div className="overflow-hidden rounded-xl bg-code font-mono text-[12.5px] leading-[19px] ring-1 ring-inset ring-border-subtle">
      {input && (
        <div className="group/io relative">
          <div className="flex items-center justify-between px-3 pt-1.5 font-sans text-[11px] text-text-tertiary">
            <span>参数</span>
            <span className="opacity-0 transition-opacity group-hover/io:opacity-100">
              <CopyButton text={input} />
            </span>
          </div>
          <pre className="max-h-[180px] overflow-auto px-3 pb-2 whitespace-pre-wrap [overflow-wrap:anywhere] text-muted-foreground">
            {prettyJson(input)}
          </pre>
        </div>
      )}
      {output && (
        <div className={cn("group/io relative", input && "border-t border-border-subtle")}>
          <div className="flex items-center justify-between px-3 pt-1.5 font-sans text-[11px] text-text-tertiary">
            <span>{isError ? "错误" : "结果"}</span>
            <span className="opacity-0 transition-opacity group-hover/io:opacity-100">
              <CopyButton text={output} />
            </span>
          </div>
          <pre
            className={cn(
              "max-h-[260px] overflow-auto px-3 pb-2 whitespace-pre-wrap [overflow-wrap:anywhere]",
              isError ? "text-destructive" : "text-muted-foreground",
            )}
          >
            {output}
          </pre>
        </div>
      )}
    </div>
  );
}

function ToolDetail({ tool }: { tool: ToolBlock }) {
  const args = parseArgs(tool.arguments);
  const command = typeof args?.command === "string" ? args.command : undefined;
  const code = typeof args?.code === "string" ? args.code : undefined;
  const cwd = typeof args?.cwd === "string" ? args.cwd : undefined;
  const running = tool.status === "running";

  return (
    <div className="space-y-2">
      {tool.diff?.files.length ? (
        tool.diff.files.map((file) => (
          <div
            key={file.path}
            className="overflow-hidden rounded-xl ring-1 ring-inset ring-border-subtle"
          >
            <div className="flex h-9 items-center gap-2 border-b border-border-subtle bg-card px-3.5 text-[12.5px]">
              <span
                className="min-w-0 flex-1 truncate font-mono text-muted-foreground"
                title={file.path}
              >
                {file.path}
              </span>
              <DiffStat {...diffStats(file)} />
            </div>
            <DiffBody file={file} maxHeight={320} mode="unified" />
          </div>
        ))
      ) : command || code ? (
        <CommandCard
          command={command ?? tool.name}
          cwd={cwd}
          code={code}
          output={tool.result}
          isError={tool.isError}
          running={running}
          durationMs={tool.durationMs}
        />
      ) : tool.arguments || tool.result ? (
        <IoCard input={tool.arguments} output={tool.result} isError={tool.isError} />
      ) : null}

      {tool.diff?.files.length && tool.isError && tool.result ? (
        <IoCard output={tool.result} isError />
      ) : null}

      {tool.images.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {tool.images.map((image, index) => (
            <a
              // biome-ignore lint/suspicious/noArrayIndexKey: 工具结果的图片列表只读不重排，且没有稳定 id
              key={index}
              href={imageSrc(image)}
              target="_blank"
              rel="noreferrer"
              className="overflow-hidden rounded-xl ring-1 ring-inset ring-border-subtle"
            >
              <img
                src={imageSrc(image)}
                alt="工具返回的图片"
                className="max-h-48 max-w-[240px] object-contain"
              />
            </a>
          ))}
        </div>
      )}

      {tool.progress && tool.progress.length > 0 && (
        <div className="space-y-1 rounded-xl bg-card px-3.5 py-2.5 text-[12.5px] ring-1 ring-inset ring-border-subtle">
          {tool.progress.map((row) => (
            <div key={row.agent_id} className="flex items-center gap-2">
              {row.status === "running" || row.status === "queued" ? (
                <Loader2Icon className="size-3 animate-spin text-brand" />
              ) : (
                <span
                  className={cn(
                    "size-1.5 rounded-full",
                    row.status === "failed" ? "bg-destructive" : "bg-success",
                  )}
                />
              )}
              <span className="min-w-0 flex-1 truncate">{row.title}</span>
              <span className="font-mono text-[11px] text-text-tertiary">
                {formatDuration(row.elapsed_ms)}
              </span>
            </div>
          ))}
        </div>
      )}

      {tool.children.length > 0 && (
        <div className="border-l border-border pl-3">
          {tool.children.map((child) => (
            <ActivityRow key={child.id} block={child} />
          ))}
        </div>
      )}
    </div>
  );
}

function BlockDetail({ block }: { block: Block }) {
  switch (block.kind) {
    case "reasoning":
      return (
        <div className="border-l-2 border-border pl-3 [--moc-prose-color:var(--muted-foreground)] [--moc-prose-leading:1.65] [--moc-prose-size:13.5px]">
          <MessageResponse>{block.text}</MessageResponse>
        </div>
      );
    case "bash":
      return (
        <CommandCard
          command={block.command}
          output={block.output}
          isError={block.isError}
          running={block.output === undefined}
        />
      );
    case "tool":
      return <ToolDetail tool={block} />;
    case "agent-message":
      return (
        <div className="rounded-xl bg-card px-3.5 py-2.5 text-[13.5px] whitespace-pre-wrap text-muted-foreground ring-1 ring-inset ring-border-subtle">
          {block.body}
        </div>
      );
    default:
      return null;
  }
}

function hasDetail(block: Block): boolean {
  switch (block.kind) {
    case "reasoning":
      return block.text.trim().length > 0;
    case "bash":
    case "agent-message":
      return true;
    case "tool":
      return Boolean(
        block.arguments ||
          block.result ||
          block.diff ||
          block.children.length ||
          block.progress?.length,
      );
    default:
      return false;
  }
}

function rowSuffix(block: Block): ReactNode {
  if (block.kind === "tool") {
    if (block.status === "running") {
      return block.awaitingPermission ? (
        <span className="text-[11px] text-warn">等待授权</span>
      ) : (
        <Loader2Icon className="size-3 animate-spin text-text-tertiary" />
      );
    }
    const files = block.diff?.files ?? [];
    if (files.length > 0) {
      const total = files.reduce(
        (sum, file) => {
          const stats = diffStats(file);
          return { added: sum.added + stats.added, removed: sum.removed + stats.removed };
        },
        { added: 0, removed: 0 },
      );
      return <DiffStat {...total} />;
    }
    if (block.status === "denied") return <span className="text-[11px] text-warn">已拒绝</span>;
    if (block.status === "cancelled")
      return <span className="text-[11px] text-text-tertiary">已取消</span>;
    if (block.durationMs !== undefined && block.durationMs >= 1000) {
      return (
        <span className="text-[11px] tabular-nums text-text-tertiary">
          {formatDuration(block.durationMs)}
        </span>
      );
    }
  }
  if (block.kind === "reasoning" && block.durationMs !== undefined && block.durationMs >= 1000) {
    return (
      <span className="text-[11px] tabular-nums text-text-tertiary">
        {formatDuration(block.durationMs)}
      </span>
    );
  }
  if (block.kind === "bash" && block.output === undefined) {
    return <Loader2Icon className="size-3 animate-spin text-text-tertiary" />;
  }
  return null;
}

/** 单行活动：图标 · 动作 · 摘要 · 尾注；有详情时整行可点开。 */
export function ActivityRow({ block, live = false }: { block: Block; live?: boolean }) {
  const expandThinking = usePrefs((state) => state.expandThinking);
  const detail = hasDetail(block);
  const streamingThought = live && block.kind === "reasoning" && block.durationMs === undefined;
  const [manual, setManual] = useState<boolean | null>(null);
  const open =
    detail && (manual ?? (block.kind === "reasoning" && (expandThinking || streamingThought)));
  const failed =
    (block.kind === "tool" && (block.isError || block.status === "error")) ||
    (block.kind === "bash" && block.isError);
  const verb =
    block.kind === "reasoning"
      ? streamingThought
        ? "思考中"
        : "思考"
      : (activityVerb(block) ?? "事件");
  const snippet = block.kind === "reasoning" && open ? "" : activitySnippet(block, 120);

  return (
    <div className="group/row">
      <button
        type="button"
        disabled={!detail}
        onClick={() => setManual(!open)}
        className={cn(
          "flex h-7 w-full min-w-0 items-center gap-2 rounded-md px-1.5 text-left text-[13.5px] text-muted-foreground transition-colors",
          detail ? "hover:bg-surface-hover hover:text-foreground" : "cursor-default",
        )}
      >
        <span
          className={cn(
            "flex size-4 shrink-0 items-center justify-center [&_svg]:size-3.5",
            failed ? "text-destructive" : "text-text-tertiary",
          )}
        >
          {failed ? <CircleAlertIcon /> : iconFor(block)}
        </span>
        <span className={cn("shrink-0 font-medium", streamingThought && "animate-pulse")}>
          {verb}
        </span>
        {snippet && (
          <span
            className={cn(
              "min-w-0 flex-1 truncate",
              block.kind === "reasoning"
                ? "text-text-tertiary"
                : "font-mono text-[12.5px] text-text-tertiary",
              failed && "text-destructive/80",
            )}
          >
            {snippet}
          </span>
        )}
        {!snippet && <span className="flex-1" />}
        <span className="flex shrink-0 items-center gap-1.5">
          {rowSuffix(block)}
          {detail && (
            <ChevronRightIcon
              className={cn(
                "size-3.5 text-text-tertiary opacity-0 transition-all group-hover/row:opacity-100",
                open && "rotate-90 opacity-100",
              )}
            />
          )}
        </span>
      </button>
      {open && (
        <div className="pt-1 pb-2 pl-7">
          <BlockDetail block={block} />
        </div>
      )}
    </div>
  );
}

/** 一组连续的活动行。 */
export function ActivityGroup({ blocks, live }: { blocks: Block[]; live: boolean }) {
  return (
    <div className="-mx-1.5">
      {blocks.map((block) => (
        <ActivityRow key={block.id} block={block} live={live} />
      ))}
    </div>
  );
}
