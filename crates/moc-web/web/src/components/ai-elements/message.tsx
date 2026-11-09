"use client";

import { cjk } from "@streamdown/cjk";
import { createMathPlugin } from "@streamdown/math";
import type { UIMessage } from "ai";
import { ChevronLeftIcon, ChevronRightIcon } from "lucide-react";
import type { ComponentProps, HTMLAttributes, ReactElement } from "react";
import { createContext, memo, useCallback, useContext, useEffect, useMemo, useState } from "react";
import {
  defaultComponents,
  defaultRemarkPlugins,
  Streamdown,
  type Components,
  type MermaidErrorComponentProps,
} from "streamdown";

import { Button } from "@/components/ui/button";
import { ButtonGroup, ButtonGroupText } from "@/components/ui/button-group";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  FS_SENTINEL,
  isResourceUri,
  RESOURCE_SENTINEL,
  resolveAnchorTarget,
  splitLineRef,
} from "@/lib/file-target";
import { codePlugin, mermaidPlugin } from "@/lib/streamdown-plugins";
import { useIsDark } from "@/lib/use-dark";
import { openFile } from "@/store/files";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { cn } from "@/lib/utils";
import { mermaidConfig } from "./mermaid-theme";
import { SVG_ALLOWED_TAGS, liftSvgFences, readFigureTokens } from "./svg-figure";

export type MessageProps = HTMLAttributes<HTMLDivElement> & {
  from: UIMessage["role"];
};

export const Message = ({ className, from, ...props }: MessageProps) => (
  <div
    className={cn(
      "group flex w-full max-w-[95%] flex-col gap-2",
      from === "user" ? "is-user ml-auto justify-end" : "is-assistant",
      className,
    )}
    {...props}
  />
);

export type MessageContentProps = HTMLAttributes<HTMLDivElement>;

export const MessageContent = ({ children, className, ...props }: MessageContentProps) => (
  <div
    className={cn(
      "is-user:dark flex w-fit min-w-0 max-w-full flex-col gap-2 overflow-hidden text-sm",
      "group-[.is-user]:ml-auto group-[.is-user]:rounded-lg group-[.is-user]:bg-secondary group-[.is-user]:px-4 group-[.is-user]:py-3 group-[.is-user]:text-foreground",
      "group-[.is-assistant]:text-foreground",
      className,
    )}
    {...props}
  >
    {children}
  </div>
);

export type MessageActionsProps = ComponentProps<"div">;

export const MessageActions = ({ className, children, ...props }: MessageActionsProps) => (
  <div className={cn("flex items-center gap-1", className)} {...props}>
    {children}
  </div>
);

export type MessageActionProps = ComponentProps<typeof Button> & {
  tooltip?: string;
  label?: string;
};

export const MessageAction = ({
  tooltip,
  children,
  label,
  variant = "ghost",
  size = "icon-sm",
  ...props
}: MessageActionProps) => {
  const button = (
    <Button size={size} type="button" variant={variant} {...props}>
      {children}
      <span className="sr-only">{label || tooltip}</span>
    </Button>
  );

  if (tooltip) {
    return (
      <TooltipProvider>
        <Tooltip>
          <TooltipTrigger asChild>{button}</TooltipTrigger>
          <TooltipContent>
            <p>{tooltip}</p>
          </TooltipContent>
        </Tooltip>
      </TooltipProvider>
    );
  }

  return button;
};

interface MessageBranchContextType {
  currentBranch: number;
  totalBranches: number;
  goToPrevious: () => void;
  goToNext: () => void;
  branches: ReactElement[];
  setBranches: (branches: ReactElement[]) => void;
}

const MessageBranchContext = createContext<MessageBranchContextType | null>(null);

const useMessageBranch = () => {
  const context = useContext(MessageBranchContext);

  if (!context) {
    throw new Error("MessageBranch components must be used within MessageBranch");
  }

  return context;
};

export type MessageBranchProps = HTMLAttributes<HTMLDivElement> & {
  defaultBranch?: number;
  onBranchChange?: (branchIndex: number) => void;
};

export const MessageBranch = ({
  defaultBranch = 0,
  onBranchChange,
  className,
  ...props
}: MessageBranchProps) => {
  const [currentBranch, setCurrentBranch] = useState(defaultBranch);
  const [branches, setBranches] = useState<ReactElement[]>([]);

  const handleBranchChange = useCallback(
    (newBranch: number) => {
      setCurrentBranch(newBranch);
      onBranchChange?.(newBranch);
    },
    [onBranchChange],
  );

  const goToPrevious = useCallback(() => {
    const newBranch = currentBranch > 0 ? currentBranch - 1 : branches.length - 1;
    handleBranchChange(newBranch);
  }, [currentBranch, branches.length, handleBranchChange]);

  const goToNext = useCallback(() => {
    const newBranch = currentBranch < branches.length - 1 ? currentBranch + 1 : 0;
    handleBranchChange(newBranch);
  }, [currentBranch, branches.length, handleBranchChange]);

  const contextValue = useMemo<MessageBranchContextType>(
    () => ({
      branches,
      currentBranch,
      goToNext,
      goToPrevious,
      setBranches,
      totalBranches: branches.length,
    }),
    [branches, currentBranch, goToNext, goToPrevious],
  );

  return (
    <MessageBranchContext.Provider value={contextValue}>
      <div className={cn("grid w-full gap-2 [&>div]:pb-0", className)} {...props} />
    </MessageBranchContext.Provider>
  );
};

export type MessageBranchContentProps = HTMLAttributes<HTMLDivElement>;

export const MessageBranchContent = ({ children, ...props }: MessageBranchContentProps) => {
  const { currentBranch, setBranches, branches } = useMessageBranch();
  const childrenArray = useMemo(
    () => (Array.isArray(children) ? children : [children]),
    [children],
  );

  // Use useEffect to update branches when they change
  useEffect(() => {
    if (branches.length !== childrenArray.length) {
      setBranches(childrenArray);
    }
  }, [childrenArray, branches, setBranches]);

  return childrenArray.map((branch, index) => (
    <div
      className={cn(
        "grid gap-2 overflow-hidden [&>div]:pb-0",
        index === currentBranch ? "block" : "hidden",
      )}
      key={branch.key}
      {...props}
    >
      {branch}
    </div>
  ));
};

export type MessageBranchSelectorProps = ComponentProps<typeof ButtonGroup>;

export const MessageBranchSelector = ({ className, ...props }: MessageBranchSelectorProps) => {
  const { totalBranches } = useMessageBranch();

  // Don't render if there's only one branch
  if (totalBranches <= 1) {
    return null;
  }

  return (
    <ButtonGroup
      className={cn(
        "[&>*:not(:first-child)]:rounded-l-md [&>*:not(:last-child)]:rounded-r-md",
        className,
      )}
      orientation="horizontal"
      {...props}
    />
  );
};

export type MessageBranchPreviousProps = ComponentProps<typeof Button>;

export const MessageBranchPrevious = ({ children, ...props }: MessageBranchPreviousProps) => {
  const { goToPrevious, totalBranches } = useMessageBranch();

  return (
    <Button
      aria-label="Previous branch"
      disabled={totalBranches <= 1}
      onClick={goToPrevious}
      size="icon-sm"
      type="button"
      variant="ghost"
      {...props}
    >
      {children ?? <ChevronLeftIcon size={14} />}
    </Button>
  );
};

export type MessageBranchNextProps = ComponentProps<typeof Button>;

export const MessageBranchNext = ({ children, ...props }: MessageBranchNextProps) => {
  const { goToNext, totalBranches } = useMessageBranch();

  return (
    <Button
      aria-label="Next branch"
      disabled={totalBranches <= 1}
      onClick={goToNext}
      size="icon-sm"
      type="button"
      variant="ghost"
      {...props}
    >
      {children ?? <ChevronRightIcon size={14} />}
    </Button>
  );
};

export type MessageBranchPageProps = HTMLAttributes<HTMLSpanElement>;

export const MessageBranchPage = ({ className, ...props }: MessageBranchPageProps) => {
  const { currentBranch, totalBranches } = useMessageBranch();

  return (
    <ButtonGroupText
      className={cn("border-none bg-transparent text-muted-foreground shadow-none", className)}
      {...props}
    >
      {currentBranch + 1} of {totalBranches}
    </ButtonGroupText>
  );
};

// 模型常用 `$…$` 写行内公式；代码高亮用 GitHub 同款浅/深主题，深色由 `.dark` 祖先切换。
export const streamdownPlugins = {
  cjk,
  code: codePlugin,
  math: createMathPlugin({ singleDollarTextMath: true }),
  mermaid: mermaidPlugin,
};

// GitHub 观感：代码块只留复制按钮，表格和 mermaid 不带复制/下载/全屏工具条。
export const streamdownControls = {
  code: { copy: true, download: false },
  table: false,
  mermaid: { download: false, copy: false, fullscreen: false, panZoom: false },
} as const;

function MermaidFallback({ chart }: MermaidErrorComponentProps) {
  return (
    <pre className="moc-mermaid-fallback">
      <code>{chart}</code>
    </pre>
  );
}

export type MessageResponseProps = ComponentProps<typeof Streamdown>;

/// file:// 与 local:// 等资源 URI 都不在 rehype-sanitize 的协议白名单里，
/// href 会被剥掉、链接退化成 button。在 remark 层先改写：file:// → 裸路径；
/// 资源 URI → `#moc-res:<uri>` 哨兵（fragment 不是协议，能活到组件层再解开）。
const fileLinkRewrite = () => (tree: { children?: unknown[]; url?: string }) => {
  const walk = (node: { children?: unknown[]; url?: string }) => {
    if (typeof node.url === "string") {
      if (/^file:\/\//i.test(node.url)) {
        try {
          node.url = decodeURIComponent(new URL(node.url).pathname);
        } catch {
          // 非法 file:// URL 保留原样，交给后续管线按既有规则处理。
        }
      } else if (isResourceUri(node.url)) {
        node.url = `${RESOURCE_SENTINEL}${node.url}`;
      } else if (
        !node.url.startsWith("#") &&
        !/^[a-zA-Z][\w+.-]*:/.test(node.url) &&
        splitLineRef(node.url).line !== undefined
      ) {
        // `src/a.ts:9` 这类行号写法含冒号，会被 URL 安全判定拦成按钮；
        // 同样走哨兵 fragment 通道，组件层再拆回路径与行号。
        node.url = `${FS_SENTINEL}${node.url}`;
      }
    }
    for (const child of node.children ?? []) {
      walk(child as typeof node);
    }
  };
  walk(tree);
};

// remarkPlugins prop 是整体替换默认列表，GFM/代码元信息必须显式保留。
const messageRemarkPlugins = [...Object.values(defaultRemarkPlugins), fileLinkRewrite];

const BuiltinAnchor = defaultComponents.a;

/// 本地路径/资源 URI 链接：点击在文件分页打开（`path:12` 与 `path#L12` 带行号）。
const MarkdownAnchor: NonNullable<Components["a"]> = ({
  href,
  children,
  className,
  node,
  ...props
}) => {
  const sessionId = useTabs((state) => state.active);
  const cwd = useTranscript((state) =>
    sessionId ? (state.bySession[sessionId]?.info.cwd ?? "") : "",
  );
  const parsed = resolveAnchorTarget(href, cwd);
  if (parsed) {
    return (
      <a
        {...props}
        className={cn(
          "wrap-anywhere cursor-pointer font-medium text-primary underline",
          className,
        )}
        data-streamdown="link"
        href={href}
        rel={undefined}
        target={undefined}
        onClick={(event) => {
          event.preventDefault();
          const id = useTabs.getState().active;
          if (id) openFile(id, parsed.target, { line: parsed.line });
        }}
        title={href}
      >
        {children}
      </a>
    );
  }
  return (
    <BuiltinAnchor className={className} href={href} node={node} {...props}>
      {children}
    </BuiltinAnchor>
  );
};

const messageComponents: Components = { a: MarkdownAnchor };

export const MessageResponse = memo(
  ({ className, children, ...props }: MessageResponseProps) => {
    const dark = useIsDark();
    const tokens = useMemo(() => readFigureTokens(dark), [dark]);
    const mermaid = useMemo(
      () => ({ config: mermaidConfig(tokens, dark), errorComponent: MermaidFallback }),
      [tokens, dark],
    );
    const source = typeof children === "string" ? liftSvgFences(children, tokens) : children;
    return (
      <Streamdown
        allowedTags={SVG_ALLOWED_TAGS}
        className={cn("moc-prose size-full", className)}
        components={messageComponents}
        controls={streamdownControls}
        lineNumbers={false}
        mermaid={mermaid}
        plugins={streamdownPlugins}
        remarkPlugins={messageRemarkPlugins}
        {...props}
      >
        {source}
      </Streamdown>
    );
  },
  (prevProps, nextProps) =>
    prevProps.children === nextProps.children && nextProps.isAnimating === prevProps.isAnimating,
);

MessageResponse.displayName = "MessageResponse";

export type MessageToolbarProps = ComponentProps<"div">;

export const MessageToolbar = ({ className, children, ...props }: MessageToolbarProps) => (
  <div className={cn("mt-4 flex w-full items-center justify-between gap-4", className)} {...props}>
    {children}
  </div>
);
