// 通用行内引用胶囊：输入框、用户消息气泡、助手 Markdown 与计划正文共用同一套外观语言
// （灰底小胶囊 + 类型图标 + 主标签 + 可选 muted 附注 + Tooltip 完整路径/说明）。

import { createContext, type MouseEvent, type ReactNode, useContext } from "react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
  useHasTooltipProvider,
} from "@/components/ui/tooltip";
import { RefKindIcon } from "@/lib/file-icon";
import { cn } from "@/lib/utils";

/** 供嵌套 Markdown 行内代码识别自身是否处于引用胶囊内，避免双重灰底或二次升级。 */
const InRefChipContext = createContext(false);

export function useInRefChip(): boolean {
  return useContext(InRefChipContext);
}

export type RefChipData = {
  /** `command` / `image` / `file-ref` / `external` / `mailto` 或 `resource/inspect` 返回的资源类别。 */
  kind: string;
  label: string;
  /** 用于按扩展名推导文件图标的路径；缺省时回退到 `detail` 或 `label`。 */
  iconPath?: string;
  /** 紧跟主标签展示的次要附注（如命令参数 `["pause"]`、行号范围 `["L10-25"]`）。 */
  args?: string[];
  detail?: string | null;
  /** 图片引用的缩略图地址。 */
  thumb?: string;
  /** 是否可点击交互（命令编辑、用户气泡引用跳转等）。 */
  interactive?: boolean;
  /** 正在打开参数编辑浮层时保持高亮态并抑制悬浮 Tooltip。 */
  active?: boolean;
};

/** 包装可选 Tooltip；独立单测未挂载 TooltipProvider 时自动退回原生 title，不抛异常。 */
function ChipTooltip({
  detail,
  disabled,
  children,
}: {
  detail?: string | null;
  disabled?: boolean;
  children: ReactNode;
}) {
  const hasProvider = useHasTooltipProvider();
  if (!detail || disabled || !hasProvider) return children;
  return (
    <Tooltip>
      <TooltipTrigger asChild>{children}</TooltipTrigger>
      <TooltipContent side="top" className="max-w-[min(36rem,90vw)] break-all">
        {detail}
      </TooltipContent>
    </Tooltip>
  );
}

/** 原子引用胶囊：用于输入框（CodeMirror 替换控件）与用户消息正文。 */
export function RefChip({
  chip,
  raw,
  onClick,
}: {
  chip: RefChipData;
  /** 消息气泡里复制时带出的原始 token；可见短名不参与选择。 */
  raw?: string;
  /** 点击交互回调。 */
  onClick?: () => void;
}) {
  const hasProvider = useHasTooltipProvider();
  const hasArgs = Boolean(chip.args && chip.args.length > 0);
  const clickable = Boolean(chip.interactive && onClick);
  const isCommand = chip.kind === "command";
  const iconPath = chip.iconPath ?? chip.detail ?? chip.label;
  const fallbackTitle = !hasProvider && chip.detail && !chip.active ? chip.detail : undefined;

  // shadcn/AI Elements 无行内语义引用胶囊原语，使用最少量的令牌 Tailwind 组合保持克制外观。
  const className = cn(
    "moc-ref-chip mx-px inline-flex h-5 items-center gap-1 rounded-md bg-accent px-1.5 align-middle font-sans text-[0.92em] leading-5 font-normal text-foreground select-none",
    hasArgs ? "max-w-80" : "max-w-64",
    clickable &&
      "cursor-pointer transition-colors hover:bg-foreground/[0.09] focus-visible:outline-none data-[active=true]:bg-foreground/[0.11]",
  );

  const inner = (
    <>
      {chip.thumb ? (
        <img src={chip.thumb} alt="" className="size-3.5 shrink-0 rounded-[3px] object-cover" />
      ) : (
        <RefKindIcon kind={chip.kind} path={iconPath} />
      )}
      <span className={cn(hasArgs ? "shrink-0" : "truncate")}>{chip.label}</span>
      {hasArgs && (
        <span data-slot="ref-chip-args" className="truncate text-muted-foreground">
          {chip.args?.join(" ")}
        </span>
      )}
    </>
  );

  const body = clickable ? (
    <button
      type="button"
      data-slot="ref-chip"
      data-active={chip.active || undefined}
      aria-haspopup={isCommand ? "dialog" : undefined}
      aria-expanded={isCommand ? Boolean(chip.active) : undefined}
      title={fallbackTitle}
      onMouseDown={isCommand ? (event) => event.preventDefault() : undefined}
      onClick={(event) => {
        event.stopPropagation();
        onClick?.();
      }}
      className={className}
    >
      {inner}
    </button>
  ) : (
    <span
      // 输入框里由 CodeMirror 承担光标与删除，控件本身不可选中、不抢焦点
      contentEditable={false}
      data-slot="ref-chip"
      title={fallbackTitle}
      className={className}
    >
      {inner}
    </span>
  );

  return (
    <>
      {raw !== undefined && <span className="sr-only">{raw}</span>}
      <ChipTooltip detail={chip.detail} disabled={chip.active}>
        {body}
      </ChipTooltip>
    </>
  );
}

const HAS_CJK = /[\u3040-\u30ff\u3400-\u9fff\uf900-\ufaff]/;

/** 长自然语言锚文本允许随段落折行，短文件名/路径/URI 保持单行紧凑胶囊。 */
export function shouldWrapProseChip(text: string): boolean {
  const trimmed = text.trim();
  if (/\s/.test(trimmed) && trimmed.length > 24) return true;
  return HAS_CJK.test(trimmed) && !trimmed.includes("/") && trimmed.length > 18;
}

/** 构造正文 Markdown 引用胶囊的类名（单行紧凑 / 多行可折行两种形态）。 */
export function proseRefChipClassName(wrap: boolean, extra?: string): string {
  return cn(
    "moc-ref-chip mx-px cursor-pointer rounded-md bg-accent px-1.5 py-[0.06em] font-sans text-[0.9em] leading-[1.35] font-normal text-foreground no-underline transition-colors hover:bg-foreground/[0.09] focus-visible:outline-none",
    wrap
      ? "inline [box-decoration-break:clone] [-webkit-box-decoration-break:clone]"
      : "inline-flex max-w-full items-center gap-1 align-[-0.18em]",
    extra,
  );
}

/** 构造正文 Markdown 引用胶囊内部结构（图标 + 标签 + 可选 muted 附注）。 */
export function ProseRefChipInner({
  kind,
  iconPath,
  meta,
  wrap,
  children,
}: {
  kind: string;
  iconPath?: string;
  meta?: string;
  wrap: boolean;
  children: ReactNode;
}) {
  return (
    <InRefChipContext.Provider value={true}>
      {wrap ? (
        <>
          <RefKindIcon kind={kind} path={iconPath} className="mr-1 inline-block align-[-0.16em]" />
          {children}
          {meta && (
            <span data-slot="ref-chip-args" className="ml-1 tabular-nums text-muted-foreground">
              {meta}
            </span>
          )}
        </>
      ) : (
        <>
          <RefKindIcon kind={kind} path={iconPath} />
          <span className="truncate">{children}</span>
          {meta && (
            <span data-slot="ref-chip-args" className="shrink-0 tabular-nums text-muted-foreground">
              {meta}
            </span>
          )}
        </>
      )}
    </InRefChipContext.Provider>
  );
}

/** 正文链接胶囊：支持拖选复制、语义 role="link" 与 Tooltip 详情。 */
export function ProseRefLink({
  kind,
  iconPath,
  meta,
  detail,
  href,
  wrap = false,
  target,
  rel,
  className,
  onClick,
  children,
}: {
  kind: string;
  iconPath?: string;
  meta?: string;
  detail?: string;
  href?: string;
  wrap?: boolean;
  target?: string;
  rel?: string;
  className?: string;
  onClick?: (event: MouseEvent<HTMLAnchorElement>) => void;
  children: ReactNode;
}) {
  const hasProvider = useHasTooltipProvider();
  const anchor = (
    <a
      data-streamdown="link"
      data-slot="ref-chip"
      href={href}
      target={target}
      rel={rel}
      title={!hasProvider ? detail : undefined}
      onClick={onClick}
      className={proseRefChipClassName(wrap, className)}
    >
      <ProseRefChipInner kind={kind} iconPath={iconPath} meta={meta} wrap={wrap}>
        {children}
      </ProseRefChipInner>
    </a>
  );
  return <ChipTooltip detail={detail}>{anchor}</ChipTooltip>;
}

/** 为外部链接等宿主自渲染元素（如 Streamdown linkSafety 按钮）套上统一的胶囊 Tooltip。 */
export function ProseRefTooltipWrap({
  detail,
  children,
}: {
  detail?: string;
  children: ReactNode;
}) {
  const hasProvider = useHasTooltipProvider();
  if (!detail) return children;
  if (!hasProvider) {
    return (
      <span className="inline" title={detail}>
        {children}
      </span>
    );
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline">{children}</span>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-[min(36rem,90vw)] break-all">
        {detail}
      </TooltipContent>
    </Tooltip>
  );
}
