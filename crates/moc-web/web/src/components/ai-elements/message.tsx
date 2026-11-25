"use client";

import { cjk } from "@streamdown/cjk";
import { createMathPlugin } from "@streamdown/math";
import type { UIMessage } from "ai";
import { ChevronLeftIcon, ChevronRightIcon } from "lucide-react";
import type { ComponentProps, ComponentType, HTMLAttributes, ReactElement } from "react";
import { createContext, memo, useCallback, useContext, useEffect, useMemo, useState } from "react";
import {
  defaultComponents,
  defaultRemarkPlugins,
  Streamdown,
  type Components,
  type MermaidErrorComponentProps,
  type StreamdownTranslations,
} from "streamdown";

import {
  createMarkdownAnchor,
  type MarkdownAnchorProps,
  MarkdownSessionContext,
  markdownRefRewrite,
  ValidatedInlineUriCode,
} from "@/components/common/markdown-refs";
import { MarkdownBreak } from "@/components/common/plain-lines";
import { Button } from "@/components/ui/button";
import { ButtonGroup, ButtonGroupText } from "@/components/ui/button-group";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { type Locale, t, useI18n, useT } from "@/lib/i18n";
import { codePlugin, mermaidPlugin } from "@/lib/streamdown-plugins";
import { useIsDark } from "@/lib/use-dark";
import { cn } from "@/lib/utils";
import { mermaidConfig } from "./mermaid-theme";
import { SVG_ALLOWED_TAGS, liftSvgFences, readFigureTokens } from "./svg-figure";
import { WidgetCard } from "./widget-card";
import { splitWidgetParts } from "./widget-fence";

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
  const t = useT();

  return (
    <Button
      aria-label={t("msg.prev_branch")}
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
  const t = useT();

  return (
    <Button
      aria-label={t("msg.next_branch")}
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

// 每种语言一份稳定对象：Streamdown 把文案放进 context，引用不变才不触发子块重渲染。
const streamdownTranslations = new Map<Locale, Partial<StreamdownTranslations>>();

/** streamdown 内置控件的文案随界面语言切换；只覆盖当前配置会露出的几项。 */
export function useStreamdownTranslations(): Partial<StreamdownTranslations> {
  const locale = useI18n((state) => state.locale);
  let translations = streamdownTranslations.get(locale);
  if (!translations) {
    translations = {
      copyCode: t("msg.copy_code"),
      copied: t("th.copied"),
      downloadImage: t("msg.download_image"),
      imageNotAvailable: t("msg.image_unavailable"),
      mermaidAriaLabel: t("mermaid"),
    };
    streamdownTranslations.set(locale, translations);
  }
  return translations;
}

function MermaidFallback({ chart }: MermaidErrorComponentProps) {
  return (
    <pre className="moc-mermaid-fallback">
      <code>{chart}</code>
    </pre>
  );
}

export type MessageResponseProps = ComponentProps<typeof Streamdown> & {
  /** 仅主对话助手正文开启内嵌交互组件渲染，防止文件预览或工具预览执行脚本。 */
  allowWidgets?: boolean;
  /** 所属会话 ID，供组件运行报错「发给助手修复」填入输入框。 */
  sessionId?: string;
};

// remarkPlugins prop 是整体替换默认列表，GFM/代码元信息必须显式保留。
const messageRemarkPlugins = [...Object.values(defaultRemarkPlugins), markdownRefRewrite];

const BuiltinAnchor = defaultComponents.a as ComponentType<MarkdownAnchorProps>;
const BuiltinCode = defaultComponents.code;
const MarkdownAnchor = createMarkdownAnchor(BuiltinAnchor) as NonNullable<Components["a"]>;

const MarkdownCode: NonNullable<Components["code"]> = (props) => {
  if ("data-block" in props) {
    return BuiltinCode ? <BuiltinCode {...props} /> : <code {...props} />;
  }
  const fallback = BuiltinCode ? <BuiltinCode {...props} /> : <code {...props} />;
  return <ValidatedInlineUriCode fallback={fallback}>{props.children}</ValidatedInlineUriCode>;
};

const messageComponents: Components = {
  a: MarkdownAnchor,
  code: MarkdownCode,
  br: MarkdownBreak,
};

export const MessageResponse = memo(
  ({
    className,
    children,
    allowWidgets = false,
    sessionId,
    isAnimating,
    ...props
  }: MessageResponseProps) => {
    const dark = useIsDark();
    const translations = useStreamdownTranslations();
    const tokens = useMemo(() => readFigureTokens(dark), [dark]);
    const mermaid = useMemo(
      () => ({ config: mermaidConfig(tokens, dark), errorComponent: MermaidFallback }),
      [tokens, dark],
    );
    const parts = useMemo(
      () => (allowWidgets && typeof children === "string" ? splitWidgetParts(children) : null),
      [allowWidgets, children],
    );

    const renderMarkdown = (
      content: ComponentProps<typeof Streamdown>["children"],
      animating?: boolean,
      key?: string,
      extraClassName?: string,
    ) => {
      const source = typeof content === "string" ? liftSvgFences(content, tokens) : content;
      return (
        <Streamdown
          key={key}
          allowedTags={SVG_ALLOWED_TAGS}
          className={cn("moc-prose size-full", extraClassName)}
          components={messageComponents}
          controls={streamdownControls}
          isAnimating={animating}
          lineNumbers={false}
          mermaid={mermaid}
          plugins={streamdownPlugins}
          remarkPlugins={messageRemarkPlugins}
          translations={translations}
          {...props}
        >
          {source}
        </Streamdown>
      );
    };

    const content = !parts ? (
      renderMarkdown(children, isAnimating, undefined, className)
    ) : parts.length === 1 && parts[0].kind === "markdown" ? (
      renderMarkdown(parts[0].text, isAnimating, undefined, className)
    ) : (
      <div className={cn("flex w-full min-w-0 flex-col gap-3", className)}>
        {parts.map((part, index) =>
          part.kind === "markdown" ? (
            renderMarkdown(
              part.text,
              Boolean(isAnimating) && index === parts.length - 1,
              `md-${index}`,
            )
          ) : (
            <WidgetCard
              key={`widget-${part.index}`}
              code={part.code}
              title={part.title}
              height={part.height}
              complete={part.complete}
              sessionId={sessionId}
            />
          ),
        )}
      </div>
    );

    return (
      <MarkdownSessionContext.Provider value={sessionId ?? null}>
        {content}
      </MarkdownSessionContext.Provider>
    );
  },
  (prevProps, nextProps) =>
    prevProps.children === nextProps.children &&
    prevProps.isAnimating === nextProps.isAnimating &&
    prevProps.mode === nextProps.mode &&
    prevProps.allowWidgets === nextProps.allowWidgets &&
    prevProps.sessionId === nextProps.sessionId,
);

MessageResponse.displayName = "MessageResponse";

export type MessageToolbarProps = ComponentProps<"div">;

export const MessageToolbar = ({ className, children, ...props }: MessageToolbarProps) => (
  <div className={cn("mt-4 flex w-full items-center justify-between gap-4", className)} {...props}>
    {children}
  </div>
);
