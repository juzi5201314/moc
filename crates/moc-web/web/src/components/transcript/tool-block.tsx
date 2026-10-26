// 工具块：标题行（图标 + 名称 + 摘要 + 状态 + 耗时）+ 按档位展示的正文。
// 点标题在「full ↔ 当前档位的收起形态」之间切换；按工具定制摘要，对齐 docs/tui.md 的显示档位表。

import {
  Ban,
  Check,
  ChevronRight,
  FileText,
  Loader2,
  Search,
  ShieldAlert,
  SquareTerminal,
  Users,
  Wrench,
  X,
} from "lucide-react";
import { type ReactNode, useState } from "react";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { formatDuration } from "@/lib/format";
import type { ToolBlock as ToolBlockData } from "@/lib/transcript-model";
import { cn } from "@/lib/utils";
import type { ImageSource } from "@/protocol/types";
import { type DetailTier, usePrefs } from "@/store/prefs";
import { DiffView, diffStats } from "./diff-view";

const BRIEF_TAIL_LINES = 4;
const BRIEF_DIFF_LINES = 6;
const MAX_IMAGES = 4;

type Args = Record<string, unknown>;

function parseArgs(raw: string): Args {
  try {
    const value: unknown = JSON.parse(raw);
    return value && typeof value === "object" && !Array.isArray(value) ? (value as Args) : {};
  } catch {
    return {};
  }
}

function str(args: Args, ...keys: string[]): string | undefined {
  for (const key of keys) {
    const value = args[key];
    if (typeof value === "string" && value) return value;
    if (typeof value === "number") return String(value);
  }
  return undefined;
}

function tailLines(text: string, count: number): string {
  const lines = text.replace(/\n+$/, "").split("\n");
  return lines.slice(-count).join("\n");
}

function firstLine(text: string): string {
  return text.split("\n", 1)[0] ?? "";
}

/** grep / find 结果计数：`12 matches · 3 files`。 */
export function countMatches(result: string): string {
  const lines = result.split("\n").filter((line) => line.trim() && !line.startsWith("#"));
  const files = new Set<string>();
  for (const line of lines) {
    const match = /^(.+?):\d+[:-]/.exec(line);
    files.add(match ? match[1] : line);
  }
  return files.size > 0 && files.size < lines.length
    ? `${lines.length} matches · ${files.size} files`
    : `${lines.length} results`;
}

export function toolSummary(block: ToolBlockData): string {
  const args = parseArgs(block.arguments);
  switch (block.name) {
    case "bash":
      return str(args, "description") ?? firstLine(str(args, "command") ?? "");
    case "edit":
    case "write": {
      const path = str(args, "path", "file_path") ?? "";
      if (!block.diff) return path;
      const { added, removed } = diffStats(block.diff);
      return `${path} +${added} −${removed}`;
    }
    case "read": {
      const path = str(args, "path", "file_path") ?? "";
      const start = str(args, "offset", "start_line", "start");
      const end = str(args, "end_line", "end");
      const limit = args.limit;
      if (start && end) return `${path}:${start}-${end}`;
      if (start && typeof limit === "number")
        return `${path}:${start}-${Number(start) + limit - 1}`;
      return start ? `${path}:${start}` : path;
    }
    case "grep":
    case "find": {
      const pattern = str(args, "pattern", "query", "glob") ?? "";
      return block.result !== undefined ? `${pattern} · ${countMatches(block.result)}` : pattern;
    }
    case "run_code":
      return str(args, "description") ?? "";
    case "wait_agent":
      return block.progress ? `${block.progress.length} 个子代理` : "";
    default:
      return "";
  }
}

function ToolIcon({ name }: { name: string }) {
  const className = "size-3.5 shrink-0 text-muted-foreground";
  if (name === "bash") return <SquareTerminal className={className} aria-hidden />;
  if (name === "read" || name === "edit" || name === "write")
    return <FileText className={className} aria-hidden />;
  if (name === "grep" || name === "find") return <Search className={className} aria-hidden />;
  if (name === "wait_agent" || name === "spawn_agent")
    return <Users className={className} aria-hidden />;
  return <Wrench className={className} aria-hidden />;
}

function StatusMark({ block }: { block: ToolBlockData }) {
  if (block.awaitingPermission) {
    return (
      <span className="inline-flex items-center gap-1 text-warn text-xs">
        <ShieldAlert className="size-3.5" aria-hidden />
        等待授权
      </span>
    );
  }
  switch (block.status) {
    case "running":
      return (
        <span className="inline-flex text-brand">
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
          <span className="sr-only">运行中</span>
        </span>
      );
    case "completed":
      return (
        <span className="inline-flex text-success">
          <Check className="size-3.5" aria-hidden />
          <span className="sr-only">完成</span>
        </span>
      );
    case "error":
      return (
        <span className="inline-flex items-center gap-1 text-destructive text-xs">
          <X className="size-3.5" aria-hidden />
          错误
        </span>
      );
    case "denied":
      return (
        <span className="inline-flex items-center gap-1 text-destructive text-xs">
          <Ban className="size-3.5" aria-hidden />
          已拒绝
        </span>
      );
    case "cancelled":
      return <span className="text-muted-foreground text-xs">已取消</span>;
  }
}

function Pre({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <pre
      className={cn(
        "max-h-80 overflow-auto whitespace-pre-wrap break-words rounded-lg bg-muted/60 px-3 py-2 font-mono text-xs leading-5",
        className,
      )}
    >
      {children}
    </pre>
  );
}

export function imageUrl(source: ImageSource): string {
  return "Url" in source
    ? source.Url
    : `data:${source.Base64.media_type};base64,${source.Base64.data}`;
}

export function ImageStrip({ images }: { images: ImageSource[] }) {
  const [zoom, setZoom] = useState<string | null>(null);
  if (images.length === 0) return null;
  const shown = images.slice(0, MAX_IMAGES);
  return (
    <>
      <div className="flex flex-wrap gap-2">
        {shown.map((image, index) => {
          const url = imageUrl(image);
          return (
            <button
              // biome-ignore lint/suspicious/noArrayIndexKey: 图片没有稳定 id，顺序固定
              key={index}
              type="button"
              aria-label={`放大第 ${index + 1} 张图片`}
              onClick={() => setZoom(url)}
              className="overflow-hidden rounded-md border"
            >
              <img src={url} alt="" className="h-24 w-auto object-cover" />
            </button>
          );
        })}
        {images.length > MAX_IMAGES ? (
          <span className="self-end text-muted-foreground text-xs">
            另有 {images.length - MAX_IMAGES} 张
          </span>
        ) : null}
      </div>
      <Dialog open={zoom !== null} onOpenChange={(open) => !open && setZoom(null)}>
        <DialogContent className="max-w-[90vw] p-2 sm:max-w-[90vw]">
          <DialogTitle className="sr-only">图片预览</DialogTitle>
          {zoom ? <img src={zoom} alt="" className="max-h-[85vh] w-full object-contain" /> : null}
        </DialogContent>
      </Dialog>
    </>
  );
}

function ToolBody({ block, tier }: { block: ToolBlockData; tier: DetailTier }) {
  const diffMode = usePrefs((state) => state.diffMode);
  const args = parseArgs(block.arguments);
  const result = block.result ?? "";
  const full = tier === "full";

  switch (block.name) {
    case "bash": {
      const command = str(args, "command") ?? "";
      return (
        <div className="space-y-1.5">
          <Pre>$ {command}</Pre>
          {result ? <Pre>{full ? result : tailLines(result, BRIEF_TAIL_LINES)}</Pre> : null}
        </div>
      );
    }
    case "edit":
    case "write":
      if (block.diff) {
        return (
          <DiffView
            diff={block.diff}
            mode={diffMode}
            maxLines={full ? undefined : BRIEF_DIFF_LINES}
          />
        );
      }
      return result && (full || block.isError) ? <Pre>{result}</Pre> : null;
    case "read":
    case "grep":
    case "find":
      return full && result ? <Pre>{result}</Pre> : null;
    case "wait_agent":
      return block.progress?.length ? (
        <ul className="space-y-0.5 text-xs">
          {block.progress.map((row) => (
            <li key={row.agent_id} className="flex gap-2">
              <span className="truncate">{row.title}</span>
              <span className="text-muted-foreground">{row.status}</span>
              <span className="ml-auto text-muted-foreground">
                {formatDuration(row.elapsed_ms)}
              </span>
            </li>
          ))}
        </ul>
      ) : full && result ? (
        <Pre>{result}</Pre>
      ) : null;
    case "run_code":
      return full ? (
        <div className="space-y-1.5">
          {str(args, "code") ? <Pre>{str(args, "code")}</Pre> : null}
          {result ? <Pre>{result}</Pre> : null}
        </div>
      ) : null;
    default: {
      const pretty = Object.keys(args).length > 0 ? JSON.stringify(args, null, 2) : block.arguments;
      return (
        <div className="space-y-1.5">
          {full && pretty ? <Pre>{pretty}</Pre> : null}
          {result ? <Pre>{full ? result : tailLines(result, BRIEF_TAIL_LINES)}</Pre> : null}
        </div>
      );
    }
  }
}

export function ToolBlockView({ block, depth = 0 }: { block: ToolBlockData; depth?: number }) {
  const globalTier = usePrefs((state) => state.toolTier);
  const [expanded, setExpanded] = useState(false);
  const tier: DetailTier = expanded ? "full" : globalTier;
  const summary = toolSummary(block);
  const showBody = tier !== "hidden" || (block.isError && block.status !== "running");

  return (
    <div className={cn("text-sm", depth > 0 && "border-l pl-3")}>
      <button
        type="button"
        aria-expanded={tier === "full"}
        onClick={() => setExpanded((value) => !value)}
        className="flex w-full min-w-0 items-center gap-1.5 rounded-md px-1 py-0.5 text-left hover:bg-accent/60"
      >
        <ChevronRight
          className={cn(
            "size-3 shrink-0 text-muted-foreground transition-transform",
            tier === "full" && "rotate-90",
          )}
          aria-hidden
        />
        <ToolIcon name={block.name} />
        <span className="shrink-0 font-medium">{block.name}</span>
        {summary ? <span className="min-w-0 truncate text-muted-foreground">{summary}</span> : null}
        <span className="ml-auto flex shrink-0 items-center gap-2">
          <StatusMark block={block} />
          {block.durationMs !== undefined ? (
            <span className="text-muted-foreground text-xs tabular-nums">
              {formatDuration(block.durationMs)}
            </span>
          ) : null}
        </span>
      </button>
      {showBody ? (
        <div className="mt-1 space-y-1.5 pl-5">
          <ToolBody block={block} tier={tier} />
          <ImageStrip images={block.images} />
        </div>
      ) : null}
      {block.children.length > 0 ? (
        <div className="mt-1 space-y-1 pl-5">
          {block.children.map((child) => (
            <ToolBlockView key={child.id} block={child} depth={depth + 1} />
          ))}
        </div>
      ) : null}
    </div>
  );
}

/** 同一批次的同名工具合并成一组：组标题 + 树形子项。 */
export function ToolGroupView({ blocks }: { blocks: ToolBlockData[] }) {
  const [open, setOpen] = useState(false);
  const first = blocks[0];
  if (!first) return null;
  if (blocks.length === 1) return <ToolBlockView block={first} />;
  const running = blocks.some((block) => block.status === "running");
  const failed = blocks.filter((block) => block.isError || block.status === "error").length;
  return (
    <div className="text-sm">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="flex w-full items-center gap-1.5 rounded-md px-1 py-0.5 text-left hover:bg-accent/60"
      >
        <ChevronRight
          className={cn(
            "size-3 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-90",
          )}
          aria-hidden
        />
        <ToolIcon name={first.name} />
        <span className="font-medium">{first.name}</span>
        <span className="text-muted-foreground">× {blocks.length}</span>
        <span className="ml-auto flex items-center gap-2 text-xs">
          {running ? (
            <Loader2 className="size-3.5 animate-spin text-brand" aria-label="运行中" />
          ) : null}
          {failed > 0 ? <span className="text-destructive">{failed} 个失败</span> : null}
        </span>
      </button>
      <div className="mt-1 space-y-1 border-l pl-3 ml-2.5">
        {(open ? blocks : blocks.slice(0, 3)).map((block) => (
          <ToolBlockView key={block.id} block={block} depth={1} />
        ))}
        {!open && blocks.length > 3 ? (
          <p className="px-1 text-muted-foreground text-xs">⋯ 还有 {blocks.length - 3} 项</p>
        ) : null}
      </div>
    </div>
  );
}
