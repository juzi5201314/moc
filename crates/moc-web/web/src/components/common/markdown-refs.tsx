// Markdown 引用层：供助手正文（Streamdown）与计划正文（react-markdown）共享。
//
// 1. `markdownRefRewrite`：在 remark 层改写本地路径与内部 URI 链接，并把裸正文里的候选内部 URI
//    标记为 `#moc-auto-res:` 哨兵；
// 2. `createMarkdownAnchor`：把本地文件/目录、内部资源与外部链接渲染成统一胶囊；裸内部 URI
//    必须经 `resource/inspect` 确认为真实存在的会话内容才转胶囊，否则还原纯文本；
// 3. `MarkdownInlineCode`：单 token 内部 URI 行内代码同样经 `resource/inspect` 确认后才转胶囊。

import {
  type ComponentType,
  createContext,
  isValidElement,
  type ReactNode,
  useContext,
  useMemo,
} from "react";
import {
  ProseRefChipInner,
  ProseRefLink,
  ProseRefTooltipWrap,
  proseRefChipClassName,
  shouldWrapProseChip,
  useInRefChip,
} from "@/components/common/ref-chip";
import {
  AUTO_RESOURCE_SENTINEL,
  externalLinkKind,
  FS_SENTINEL,
  formatLineBadge,
  formatTargetDetail,
  isResourceUri,
  labelMentionsLine,
  matchSingleInternalUri,
  type ParsedLink,
  RESOURCE_SENTINEL,
  resolveAnchorTarget,
  resourceKindFromUri,
  scanInternalUris,
  splitLineRef,
} from "@/lib/file-target";
import { useRefLookup } from "@/lib/ref-inspect";
import type { ResourceRefInfo } from "@/protocol/types";
import { openFile, revealDir } from "@/store/files";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";

/** 为内嵌 Markdown（对话正文、计划正文）显式注入所属 sessionId；未注入时回退到当前激活会话。 */
export const MarkdownSessionContext = createContext<string | null>(null);

const EMPTY_TOKENS: readonly string[] = [];

type MarkdownAstNode = {
  type: string;
  value?: string;
  children?: MarkdownAstNode[];
  identifier?: string;
  url?: string;
  title?: string | null;
};

/** 不在以下 AST 节点内部扫描裸内部 URI，避免破坏链接、代码块与反引号原文。 */
const SKIP_AUTO_URI_NODES = new Set([
  "link",
  "linkReference",
  "definition",
  "code",
  "inlineCode",
  "html",
]);

function isRelativeFileLink(url: string) {
  const value = url.trim();
  return value !== "" && !/^(?:[#/?\\]|[a-z][\w+.-]*:)/i.test(value);
}

/** 提取 React children 中的纯文本，用于判断是否已写行号以及是否需要长句折行。 */
function plainChildText(children: ReactNode): string {
  if (typeof children === "string" || typeof children === "number") return String(children);
  if (Array.isArray(children)) return children.map(plainChildText).join("");
  if (isValidElement<{ children?: ReactNode }>(children)) {
    return plainChildText(children.props.children);
  }
  return "";
}

/** 构造内部资源胶囊的 Tooltip 文案：技能/工具附带说明，其余展示干净 URI 与行号。 */
function formatResourceTooltip(
  parsed: ParsedLink,
  cwd: string,
  info?: ResourceRefInfo | null,
): string {
  const base = formatTargetDetail(parsed, cwd);
  if (!info?.detail) return base;
  if (info.kind === "skill" || info.kind === "tool") {
    return `${base} · ${info.detail}`;
  }
  return base;
}

function useMarkdownSession() {
  const ctxSession = useContext(MarkdownSessionContext);
  const activeTab = useTabs((state) => state.active);
  const sessionId = ctxSession ?? activeTab ?? null;
  const inspectSessionId = sessionId && sessionId !== "draft-session" ? sessionId : null;
  const cwd = useTranscript((state) =>
    sessionId ? (state.bySession[sessionId]?.info.cwd ?? "") : "",
  );
  return { sessionId, inspectSessionId, cwd };
}

function openParsedTarget(sessionId: string | null, parsed: ParsedLink) {
  const id = sessionId ?? useTabs.getState().active;
  if (!id) return;
  if (parsed.target.kind === "fs" && parsed.isDir) {
    revealDir(id, parsed.target.path);
    return;
  }
  openFile(id, parsed.target, parsed.line !== undefined ? { line: parsed.line } : {});
}

/**
 * Remark 插件：
 * 1. `file://` → `#moc-fs:<pathname><hash>`（保留编码形态与 `#L…` 行号），资源 URI → `#moc-res:<uri>`，
 *    带行号或相对路径 → `#moc-fs:<url>`，绕过 rehype-sanitize / rehype-harden 协议拦截；
 * 2. 扫描裸正文文本节点里的候选内部 URI（`local://` / `artifact://` / `history://` / `skill://` / `tool://`），
 *    改写为 `#moc-auto-res:<raw>`，由渲染层经 `resource/inspect` 校验真实存在后才转胶囊。
 */
export const markdownRefRewrite = () => (tree: MarkdownAstNode) => {
  const definitions = new Map<string, MarkdownAstNode>();
  const references: MarkdownAstNode[] = [];

  const rewriteUrl = (node: MarkdownAstNode, isLinkNode: boolean) => {
    if (typeof node.url !== "string") return;
    if (/^file:\/\//i.test(node.url)) {
      try {
        // 保持编码形态并保留 `#L10-L25` 片段，由 resolveAnchorTarget 统一解码一次。
        const parsed = new URL(node.url);
        node.url = `${FS_SENTINEL}${parsed.pathname}${parsed.hash}`;
      } catch {
        // 非法 file:// URL 保留原样。
      }
    } else if (isResourceUri(node.url)) {
      node.url = `${RESOURCE_SENTINEL}${node.url}`;
    } else if (
      !node.url.startsWith("#") &&
      !/^[a-zA-Z][\w+.-]*:/.test(node.url) &&
      splitLineRef(node.url).line !== undefined
    ) {
      node.url = `${FS_SENTINEL}${node.url}`;
    } else if (isLinkNode && isRelativeFileLink(node.url)) {
      node.url = `${FS_SENTINEL}${node.url}`;
    }
  };

  const walk = (node: MarkdownAstNode) => {
    if (node.type === "definition" && node.identifier && !definitions.has(node.identifier)) {
      definitions.set(node.identifier, node);
    } else if (node.type === "linkReference") {
      references.push(node);
    }
    rewriteUrl(node, node.type === "link");

    const children = node.children;
    if (!children || children.length === 0) return;

    const allowAutoUri = !SKIP_AUTO_URI_NODES.has(node.type);
    let mutated = false;
    const nextChildren: MarkdownAstNode[] = [];
    for (const child of children) {
      if (allowAutoUri && child.type === "text" && typeof child.value === "string") {
        const spans = scanInternalUris(child.value);
        if (spans.length > 0) {
          mutated = true;
          let cursor = 0;
          for (const span of spans) {
            if (span.from > cursor) {
              nextChildren.push({ type: "text", value: child.value.slice(cursor, span.from) });
            }
            nextChildren.push({
              type: "link",
              url: `${AUTO_RESOURCE_SENTINEL}${span.raw}`,
              children: [{ type: "text", value: span.raw }],
            });
            cursor = span.to;
          }
          if (cursor < child.value.length) {
            nextChildren.push({ type: "text", value: child.value.slice(cursor) });
          }
          continue;
        }
      }
      walk(child);
      nextChildren.push(child);
    }
    if (mutated) node.children = nextChildren;
  };

  walk(tree);

  for (const node of references) {
    const definition = node.identifier ? definitions.get(node.identifier) : undefined;
    if (definition?.url && isRelativeFileLink(definition.url)) {
      node.type = "link";
      node.url = `${FS_SENTINEL}${definition.url}`;
      node.title = definition.title;
    }
  }
};

export type MarkdownAnchorProps = {
  href?: string;
  children?: ReactNode;
  className?: string;
  node?: unknown;
  [key: string]: unknown;
};

/**
 * 构造 Markdown `<a>` 渲染器：
 * - 传入 `BuiltinAnchor`（Streamdown 内置）时，外链胶囊保留其 `linkSafety` 确认弹窗；
 * - 未传时（PlanMarkdown / react-markdown），外链直接渲染为新标签打开的胶囊链接。
 */
export function createMarkdownAnchor(BuiltinAnchor?: ComponentType<MarkdownAnchorProps>) {
  return function MarkdownAnchor({
    href,
    children,
    className,
    node,
    ...props
  }: MarkdownAnchorProps) {
    const { sessionId, inspectSessionId, cwd } = useMarkdownSession();

    const autoRaw = href?.startsWith(AUTO_RESOURCE_SENTINEL)
      ? href.slice(AUTO_RESOURCE_SENTINEL.length)
      : null;
    const autoParts = autoRaw ? splitLineRef(autoRaw) : null;
    const parsed = autoRaw ? null : resolveAnchorTarget(href, cwd);

    const inspectUri = autoParts
      ? autoParts.path
      : parsed?.target.kind === "resource" && !parsed.target.uri.endsWith("://")
        ? parsed.target.uri
        : null;
    const lookupTokens = useMemo(() => (inspectUri ? [inspectUri] : EMPTY_TOKENS), [inspectUri]);
    const lookup = useRefLookup(inspectSessionId, lookupTokens);

    // 1. 裸正文自动扫描到的内部 URI：必须经 core 确认真实存在才转胶囊，否则原样保持纯文本。
    if (autoParts) {
      const state = lookup(autoParts.path);
      if (state.status !== "valid") {
        return <>{children}</>;
      }
      const target: ParsedLink = {
        target: { kind: "resource", uri: autoParts.path },
        line: autoParts.line,
        endLine: autoParts.endLine,
      };
      const lineBadge = formatLineBadge(autoParts.line, autoParts.endLine);
      const tooltip = formatResourceTooltip(target, cwd, state.info);
      return (
        <ProseRefLink
          kind={state.info.kind}
          iconPath={autoParts.path}
          meta={lineBadge}
          detail={tooltip}
          href={autoRaw ?? undefined}
          className={className}
          onClick={(event) => {
            event.preventDefault();
            openParsedTarget(sessionId, target);
          }}
        >
          {state.info.label}
        </ProseRefLink>
      );
    }

    // 2. 显式本地文件/目录与内部资源链接。
    if (parsed) {
      if (parsed.target.kind === "resource" && inspectUri) {
        const state = lookup(inspectUri);
        // core 明确确认该内部资源不存在：不渲染成误导性的引用胶囊，退化为普通文字。
        if (state.status === "invalid") {
          return <span className={className}>{children}</span>;
        }
        const info = state.status === "valid" ? state.info : null;
        const labelText = plainChildText(children);
        const lineBadge =
          parsed.line !== undefined && !labelMentionsLine(labelText, parsed.line, parsed.endLine)
            ? formatLineBadge(parsed.line, parsed.endLine)
            : undefined;
        const kind = info?.kind ?? resourceKindFromUri(parsed.target.uri) ?? "file";
        const tooltip = formatResourceTooltip(parsed, cwd, info);
        return (
          <ProseRefLink
            kind={kind}
            iconPath={parsed.target.uri}
            meta={lineBadge}
            detail={tooltip}
            href={parsed.target.uri}
            wrap={shouldWrapProseChip(labelText)}
            className={className}
            onClick={(event) => {
              event.preventDefault();
              openParsedTarget(sessionId, parsed);
            }}
          >
            {children}
          </ProseRefLink>
        );
      }

      const labelText = plainChildText(children);
      const lineBadge =
        parsed.line !== undefined && !labelMentionsLine(labelText, parsed.line, parsed.endLine)
          ? formatLineBadge(parsed.line, parsed.endLine)
          : undefined;
      const kind =
        parsed.target.kind === "resource"
          ? (resourceKindFromUri(parsed.target.uri) ?? "file")
          : parsed.isDir
            ? "dir"
            : "file";
      const iconPath = parsed.target.kind === "fs" ? parsed.target.path : parsed.target.uri;
      const detail = formatTargetDetail(parsed, cwd);
      return (
        <ProseRefLink
          kind={kind}
          iconPath={iconPath}
          meta={lineBadge}
          detail={detail}
          href={detail}
          wrap={shouldWrapProseChip(labelText)}
          className={className}
          onClick={(event) => {
            event.preventDefault();
            openParsedTarget(sessionId, parsed);
          }}
        >
          {children}
        </ProseRefLink>
      );
    }

    // 3. 外部网页 / mailto 链接：胶囊化呈现，长句支持自然折行；Streamdown 侧保留 linkSafety 弹窗。
    const extKind = externalLinkKind(href);
    if (extKind && href) {
      const labelText = plainChildText(children);
      const wrap = shouldWrapProseChip(labelText);
      if (BuiltinAnchor) {
        return (
          <ProseRefTooltipWrap detail={href}>
            <BuiltinAnchor
              {...props}
              className={proseRefChipClassName(wrap, className)}
              href={href}
              node={node}
            >
              <ProseRefChipInner kind={extKind} wrap={wrap}>
                {children}
              </ProseRefChipInner>
            </BuiltinAnchor>
          </ProseRefTooltipWrap>
        );
      }
      return (
        <ProseRefLink
          kind={extKind}
          detail={href}
          href={href}
          target={extKind === "external" ? "_blank" : undefined}
          rel={extKind === "external" ? "noreferrer" : undefined}
          wrap={wrap}
          className={className}
        >
          {children}
        </ProseRefLink>
      );
    }

    // 4. 文档内锚点（#section）或流式未闭合链接：交回宿主默认渲染。
    if (BuiltinAnchor) {
      return (
        <BuiltinAnchor className={className} href={href} node={node} {...props}>
          {children}
        </BuiltinAnchor>
      );
    }
    return (
      <a data-streamdown="link" className={className} href={href} {...props}>
        {children}
      </a>
    );
  };
}

/**
 * 行内代码内部 URI 升级器：
 * 仅当整段行内代码恰好是一个具体内部 URI（且不嵌套在已有胶囊内），并且 `resource/inspect`
 * 确认为真实存在的资源时，才渲染成可点击胶囊；否则原样输出普通行内代码。
 */
export function ValidatedInlineUriCode({
  children,
  fallback,
}: {
  children: ReactNode;
  fallback: ReactNode;
}) {
  const inChip = useInRefChip();
  const { sessionId, inspectSessionId, cwd } = useMarkdownSession();
  const candidate =
    !inChip && typeof children === "string" ? matchSingleInternalUri(children) : null;
  const lookupTokens = useMemo(() => (candidate ? [candidate.uri] : EMPTY_TOKENS), [candidate]);
  const lookup = useRefLookup(inspectSessionId, lookupTokens);

  if (!candidate) return fallback;
  const state = lookup(candidate.uri);
  if (state.status !== "valid") return fallback;

  const target: ParsedLink = {
    target: { kind: "resource", uri: candidate.uri },
    line: candidate.line,
    endLine: candidate.endLine,
  };
  const lineBadge = formatLineBadge(candidate.line, candidate.endLine);
  const tooltip = formatResourceTooltip(target, cwd, state.info);

  return (
    <ProseRefLink
      kind={state.info.kind}
      iconPath={candidate.uri}
      meta={lineBadge}
      detail={tooltip}
      href={candidate.raw}
      onClick={(event) => {
        event.preventDefault();
        openParsedTarget(sessionId, target);
      }}
    >
      {state.info.label}
    </ProseRefLink>
  );
}
