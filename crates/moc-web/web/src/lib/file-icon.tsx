// 文件与引用类型图标：低饱和语言/资源辨识色 + 零外部依赖轻量 SVG，
// 供 RefChip、FileTree、补全菜单与附件条统一复用。

import {
  BookOpenIcon,
  ClipboardListIcon,
  DatabaseIcon,
  FileCode2Icon,
  FileIcon,
  FileTextIcon,
  FolderIcon,
  GlobeIcon,
  HistoryIcon,
  ImageIcon,
  MailIcon,
  PackageIcon,
  PaperclipIcon,
  SettingsIcon,
  SquareSlashIcon,
  TerminalIcon,
  WrenchIcon,
} from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

type IconProps = { className?: string };

/** 全部语言标志均用纯矢量 `<path>` 绘制，避免 `<text>` 节点混入正文选区复制与 textContent。 */

/** Rust 标志：外圈轻齿 + 居中 R，低饱和暖橙。 */
function RustIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#c85a28] dark:text-[#e58454]", className)}
    >
      <circle
        cx="8"
        cy="8"
        r="5.6"
        stroke="currentColor"
        strokeWidth="1.45"
        strokeDasharray="2.3 1.6"
      />
      <path
        d="M6.2 10.8V5.2h2.1a1.45 1.45 0 0 1 0 2.9H6.2m2.1 0 1.6 2.7"
        stroke="currentColor"
        strokeWidth="1.45"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** TypeScript `TS` 矢量徽标。 */
function TsIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#2f6fb8] dark:text-[#6aa3e8]", className)}
    >
      <path
        d="M1.8 4.6h5.4M4.5 4.6v6.9M13.6 5.9c-.4-.9-1.2-1.3-2.2-1.3-1.3 0-2.2.7-2.2 1.7 0 1 .7 1.4 2.1 1.7 1.5.3 2.5.8 2.5 1.9 0 1.1-1 1.8-2.4 1.8-1.2 0-2.1-.5-2.5-1.4"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** JavaScript `JS` 矢量徽标。 */
function JsIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#b58a1b] dark:text-[#e2be52]", className)}
    >
      <path
        d="M6.4 4.6v4.8c0 1.3-.8 2.1-2 2.1-.9 0-1.6-.4-1.9-1.1M13.6 5.9c-.4-.9-1.2-1.3-2.2-1.3-1.3 0-2.2.7-2.2 1.7 0 1 .7 1.4 2.1 1.7 1.5.3 2.5.8 2.5 1.9 0 1.1-1 1.8-2.4 1.8-1.2 0-2.1-.5-2.5-1.4"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** Python `PY` 矢量徽标。 */
function PyIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#3b6ea5] dark:text-[#6fa4dc]", className)}
    >
      <path
        d="M2.5 11.5V4.6h2.2a1.75 1.75 0 0 1 0 3.5H2.5M8.6 4.6l2.5 3.6 2.5-3.6M11.1 8.2v3.3"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** Go `GO` 矢量徽标。 */
function GoIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#0d8ba6] dark:text-[#4fc3dc]", className)}
    >
      <path
        d="M6.8 6c-.4-.9-1.1-1.4-2.1-1.4-1.5 0-2.6 1.5-2.6 3.4s1.1 3.5 2.6 3.5c1.3 0 2.2-.9 2.3-2.3H4.8"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <ellipse cx="11.5" cy="8.05" rx="2.35" ry="3.4" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  );
}

/** React 原子轨道：用于 .tsx / .jsx。 */
function ReactIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#0e8aa3] dark:text-[#5cc8dc]", className)}
    >
      <circle cx="8" cy="8" r="1.35" fill="currentColor" />
      <ellipse cx="8" cy="8" rx="6.2" ry="2.4" stroke="currentColor" strokeWidth="1.15" />
      <ellipse
        cx="8"
        cy="8"
        rx="6.2"
        ry="2.4"
        stroke="currentColor"
        strokeWidth="1.15"
        transform="rotate(60 8 8)"
      />
      <ellipse
        cx="8"
        cy="8"
        rx="6.2"
        ry="2.4"
        stroke="currentColor"
        strokeWidth="1.15"
        transform="rotate(120 8 8)"
      />
    </svg>
  );
}

/** Markdown M↓ 徽标。 */
function MarkdownIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#486a94] dark:text-[#7da4d4]", className)}
    >
      <path
        d="M2 11.5V4.5l2.75 3.3L7.5 4.5v7M12.2 4.5v6.5m0 0-2-2m2 2 2-2"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** JSON `{}` 花括号图标。 */
function JsonIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#a3762a] dark:text-[#d4aa5a]", className)}
    >
      <path
        d="M5.8 3.2C4.6 3.2 4.2 3.9 4.2 4.9v1.5c0 .9-.5 1.4-1.3 1.6.8.2 1.3.7 1.3 1.6v1.5c0 1 .4 1.7 1.6 1.7M10.2 3.2c1.2 0 1.6.7 1.6 1.7v1.5c0 .9.5 1.4 1.3 1.6-.8.2-1.3.7-1.3 1.6v1.5c0 1-.4 1.7-1.6 1.7"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** HTML / XML / SVG `<>` 尖括号图标。 */
function MarkupIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#c05638] dark:text-[#e08266]", className)}
    >
      <path
        d="M5.5 4.5 2.2 8l3.3 3.5M10.5 4.5 13.8 8l-3.3 3.5M9.1 3.6 6.9 12.4"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** CSS `#` 样式图标。 */
function StyleIcon({ className }: IconProps) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="none"
      aria-hidden="true"
      className={cn("size-3.5 shrink-0 text-[#6952b8] dark:text-[#9b87e3]", className)}
    >
      <path
        d="M6.4 2.8 4.8 13.2M11.2 2.8 9.6 13.2M3 6h10.4M2.6 10H13"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
    </svg>
  );
}

const IMAGE_EXTS = new Set(["png", "jpg", "jpeg", "gif", "webp", "ico", "avif", "bmp"]);
const CONFIG_EXTS = new Set(["toml", "yaml", "yml", "ini", "conf", "cfg", "env", "lock"]);
const SHELL_EXTS = new Set(["sh", "bash", "zsh", "fish", "ps1", "bat", "cmd"]);
const MARKUP_EXTS = new Set(["html", "htm", "xml", "svg"]);
const STYLE_EXTS = new Set(["css", "scss", "sass", "less"]);
const DB_EXTS = new Set(["sql", "sqlite", "sqlite3", "db"]);
const CODE_EXTS = new Set([
  "c",
  "h",
  "cc",
  "cpp",
  "cxx",
  "hpp",
  "java",
  "kt",
  "kts",
  "rb",
  "php",
  "swift",
  "zig",
  "lua",
  "nix",
  "vue",
  "svelte",
  "dart",
  "ex",
  "exs",
  "erl",
  "hs",
  "ml",
  "r",
  "scala",
  "proto",
  "graphql",
  "gql",
]);
const DOC_EXTS = new Set(["txt", "log", "csv", "tsv", "pdf", "rst", "adoc"]);

/** 从路径或文件名拆出规范文件名（小写）与扩展名。 */
function fileExt(pathOrName: string): { base: string; ext: string } {
  const clean = pathOrName.replace(/[\\/]+$/, "");
  const base = (clean.split(/[\\/]/).pop() ?? clean).toLowerCase();
  if (base.endsWith(".d.ts") || base.endsWith(".d.mts") || base.endsWith(".d.cts")) {
    return { base, ext: "ts" };
  }
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return { base, ext: "" };
  return { base, ext: base.slice(dot + 1) };
}

/** 按文件名/扩展名渲染对应的文件类型图标。 */
export function FileTypeIcon({ name, className }: { name: string; className?: string }): ReactNode {
  const { base, ext } = fileExt(name);
  const cls = cn("size-3.5 shrink-0", className);

  if (ext === "rs") return <RustIcon className={className} />;
  if (ext === "tsx" || ext === "jsx") return <ReactIcon className={className} />;
  if (ext === "ts" || ext === "mts" || ext === "cts") return <TsIcon className={className} />;
  if (ext === "js" || ext === "mjs" || ext === "cjs") return <JsIcon className={className} />;
  if (ext === "py" || ext === "pyi") return <PyIcon className={className} />;
  if (ext === "go") return <GoIcon className={className} />;
  if (ext === "md" || ext === "mdx") return <MarkdownIcon className={className} />;
  if (ext === "json" || ext === "jsonc" || ext === "jsonl")
    return <JsonIcon className={className} />;
  if (STYLE_EXTS.has(ext)) return <StyleIcon className={className} />;
  if (MARKUP_EXTS.has(ext)) return <MarkupIcon className={className} />;
  if (
    CONFIG_EXTS.has(ext) ||
    base.startsWith(".env") ||
    base === ".gitignore" ||
    base === ".editorconfig" ||
    base === ".npmrc"
  ) {
    return <SettingsIcon className={cn(cls, "text-[#64748b] dark:text-[#94a3b8]")} />;
  }
  if (SHELL_EXTS.has(ext)) {
    return <TerminalIcon className={cn(cls, "text-[#2d8a56] dark:text-[#5bc286]")} />;
  }
  if (DB_EXTS.has(ext)) {
    return <DatabaseIcon className={cn(cls, "text-[#4b7a8a] dark:text-[#78adc0]")} />;
  }
  if (IMAGE_EXTS.has(ext)) {
    return <ImageIcon className={cn(cls, "text-[#7a5fb8] dark:text-[#aa91e3]")} />;
  }
  if (CODE_EXTS.has(ext) || base === "dockerfile" || base === "makefile") {
    return <FileCode2Icon className={cn(cls, "text-muted-foreground")} />;
  }
  if (DOC_EXTS.has(ext)) {
    return <FileTextIcon className={cn(cls, "text-muted-foreground")} />;
  }
  return <FileIcon className={cn(cls, "text-muted-foreground")} />;
}

/** 按引用类别（含文件扩展名、会话空间资源、命令与外链）渲染图标。 */
export function RefKindIcon({
  kind,
  path,
  className,
}: {
  kind: string;
  /** 文件或资源路径，用于推导具体扩展名图标。 */
  path?: string;
  className?: string;
}): ReactNode {
  const cls = cn("size-3.5 shrink-0", className);
  switch (kind) {
    case "command":
      return <SquareSlashIcon className={cn(cls, "text-muted-foreground")} />;
    case "image":
      return <ImageIcon className={cn(cls, "text-[#7a5fb8] dark:text-[#aa91e3]")} />;
    case "dir":
    case "namespace":
      return <FolderIcon className={cn(cls, "text-[#a67c37] dark:text-[#d4aa62]")} />;
    case "skill":
      return <BookOpenIcon className={cn(cls, "text-[#6857b8] dark:text-[#9c8ee0]")} />;
    case "local":
      return <ClipboardListIcon className={cn(cls, "text-[#2c7f72] dark:text-[#57b8a8]")} />;
    case "artifact":
      return <PackageIcon className={cn(cls, "text-[#a36b2c] dark:text-[#d49d5b]")} />;
    case "history":
      return <HistoryIcon className={cn(cls, "text-muted-foreground")} />;
    case "tool":
      return <WrenchIcon className={cn(cls, "text-muted-foreground")} />;
    case "external":
      return <GlobeIcon className={cn(cls, "text-muted-foreground")} />;
    case "mailto":
      return <MailIcon className={cn(cls, "text-muted-foreground")} />;
    case "file":
      return path ? (
        <FileTypeIcon name={path} className={className} />
      ) : (
        <FileTextIcon className={cn(cls, "text-muted-foreground")} />
      );
    case "file-ref":
      return path ? (
        <FileTypeIcon name={path} className={className} />
      ) : (
        <PaperclipIcon className={cn(cls, "text-muted-foreground")} />
      );
    default:
      return path ? (
        <FileTypeIcon name={path} className={className} />
      ) : (
        <FileTextIcon className={cn(cls, "text-muted-foreground")} />
      );
  }
}
