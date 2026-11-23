// 嵌入式交互 Web 组件卡片：流式生成视图、iframe 预览、代码高亮、导出独立 HTML、
// 深浅主题热同步、全屏弹窗以及运行时报错捕获与一键填回修复。

import {
  AlertTriangleIcon,
  CheckIcon,
  CopyIcon,
  DownloadIcon,
  Loader2Icon,
  Maximize2Icon,
  RotateCcwIcon,
  SparklesIcon,
  XIcon,
} from "lucide-react";
import { memo, useEffect, useId, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { CodeBlock } from "@/components/ai-elements/code-block";
import { IconButton } from "@/components/common/icon-button";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n, useT } from "@/lib/i18n";
import { useIsDark } from "@/lib/use-dark";
import { cn } from "@/lib/utils";
import { useComposer } from "@/store/composer";
import { useTranscript } from "@/store/transcript";
import { requestComposerFocus } from "@/store/ui";
import { buildStandaloneHtml, buildWidgetDocument, readWidgetTokens } from "./widget-document";
import { sanitizeFilename } from "./widget-fence";

/** 轻度折中隔离策略：放行脚本、CDN、外网请求、表单、下载与弹窗；不授予 allow-same-origin，阻断读取 moc-token 与宿主 DOM。 */
const WIDGET_SANDBOX =
  "allow-scripts allow-forms allow-downloads allow-popups allow-modals allow-pointer-lock";

const MIN_HEIGHT = 140;
const DEFAULT_HEIGHT = 360;
const DEFAULT_MAX_HEIGHT = 640;
const HEIGHT_CACHE_LIMIT = 64;

/** 有界高度缓存：虚拟列表卸载重挂时沿用上次测得高度，避免跳动。 */
const heightCache = new Map<string, number>();

function cacheKey(code: string, title?: string): string {
  // 取长度与首尾片段拼键，避免长源码整串常驻内存作键。
  const head = code.slice(0, 64);
  const tail = code.slice(-64);
  return `${title ?? ""}:${code.length}:${head}:${tail}`;
}

function readCachedHeight(key: string): number | undefined {
  return heightCache.get(key);
}

function writeCachedHeight(key: string, height: number): void {
  if (heightCache.has(key)) heightCache.delete(key);
  heightCache.set(key, height);
  if (heightCache.size > HEIGHT_CACHE_LIMIT) {
    const oldest = heightCache.keys().next().value;
    if (oldest !== undefined) heightCache.delete(oldest);
  }
}

function clampHeight(value: number, maxHeight: number): number {
  return Math.max(MIN_HEIGHT, Math.min(maxHeight, Math.ceil(value)));
}

type WidgetCardProps = {
  code: string;
  title?: string;
  height?: number;
  complete: boolean;
  sessionId?: string;
};

type WidgetMode = "preview" | "code";

type WidgetFrameProps = {
  code: string;
  title: string;
  reloadNonce: number;
  maxHeight?: number;
  initialHeight: number;
  fillHeight?: boolean;
  suspended?: boolean;
  onMeasuredHeight?: (height: number) => void;
  onError: (message: string) => void;
};

function WidgetFrame({
  code,
  title,
  reloadNonce,
  maxHeight = DEFAULT_MAX_HEIGHT,
  initialHeight,
  fillHeight = false,
  suspended = false,
  onMeasuredHeight,
  onError,
}: WidgetFrameProps) {
  const dark = useIsDark();
  const locale = useI18n((state) => state.locale);
  const lang = locale === "zh" ? "zh-CN" : "en";
  const rawId = useId();
  const codeRef = useRef(code);
  const codeRevRef = useRef(0);
  if (codeRef.current !== code) {
    codeRef.current = code;
    codeRevRef.current += 1;
  }
  const instanceId = useMemo(
    () => `w-${rawId.replace(/[^a-zA-Z0-9_-]/g, "")}-${reloadNonce}-${codeRevRef.current}`,
    [rawId, reloadNonce, code],
  );
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const renderedRef = useRef(false);
  const [frameHeight, setFrameHeight] = useState(initialHeight);

  useEffect(() => {
    setFrameHeight(initialHeight);
  }, [initialHeight]);

  // 保持最新回调与配置供 ready 握手读取，避免主题切换触发 iframe 重载。
  const latestRef = useRef({ code, dark, lang, maxHeight, suspended, onMeasuredHeight, onError });
  latestRef.current = { code, dark, lang, maxHeight, suspended, onMeasuredHeight, onError };

  // 重置或源码变更时重新建立握手渲染。
  // biome-ignore lint/correctness/useExhaustiveDependencies: instanceId/code 变化即新建或重载 iframe
  useEffect(() => {
    renderedRef.current = false;
  }, [instanceId, code]);

  useEffect(() => {
    const handleMessage = (event: MessageEvent) => {
      const targetWindow = iframeRef.current?.contentWindow;
      if (!targetWindow || event.source !== targetWindow) return;
      const data = event.data;
      if (
        !data ||
        typeof data !== "object" ||
        data.channel !== "moc-widget" ||
        data.id !== instanceId
      ) {
        return;
      }

      if (data.type === "ready" && !renderedRef.current) {
        renderedRef.current = true;
        const current = latestRef.current;
        const html = buildWidgetDocument(current.code, {
          id: instanceId,
          dark: current.dark,
          tokens: readWidgetTokens(current.dark),
          lang: current.lang,
        });
        // opaque origin iframe 必须使用 "*"，安全靠 source === contentWindow 与随机 instanceId 保证。
        targetWindow.postMessage({ channel: "moc-widget", id: instanceId, type: "render", html }, "*");
        return;
      }

      if (data.type === "resize" && typeof data.height === "number" && Number.isFinite(data.height)) {
        if (latestRef.current.suspended) return;
        const next = clampHeight(data.height, latestRef.current.maxHeight);
        setFrameHeight(next);
        latestRef.current.onMeasuredHeight?.(next);
        return;
      }

      if (data.type === "error" && typeof data.message === "string") {
        const loc =
          typeof data.line === "number" && data.line > 0
            ? typeof data.column === "number" && data.column > 0
              ? ` (L${data.line}:${data.column})`
              : ` (L${data.line})`
            : "";
        latestRef.current.onError(`${data.message}${loc}`);
      }
    };

    window.addEventListener("message", handleMessage);
    return () => window.removeEventListener("message", handleMessage);
  }, [instanceId]);

  // 宿主浅/深色切换时向已渲染 iframe 热推送变量，不丢当前交互状态。
  useEffect(() => {
    if (!renderedRef.current) return;
    const targetWindow = iframeRef.current?.contentWindow;
    if (!targetWindow) return;
    targetWindow.postMessage(
      {
        channel: "moc-widget",
        id: instanceId,
        type: "theme",
        dark,
        tokens: readWidgetTokens(dark),
      },
      "*",
    );
  }, [dark, instanceId]);

  return (
    <iframe
      ref={iframeRef}
      key={instanceId}
      title={title}
      src={`/widget-host#${instanceId}`}
      sandbox={WIDGET_SANDBOX}
      className={cn("block w-full border-0 bg-card", fillHeight && "h-full flex-1")}
      style={fillHeight ? undefined : { height: frameHeight }}
    />
  );
}

function StreamingSource({ code }: { code: string }) {
  const preRef = useRef<HTMLPreElement>(null);
  // biome-ignore lint/correctness/useExhaustiveDependencies: 流式追加源码时自动滚到底部
  useEffect(() => {
    const el = preRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [code]);

  return (
    <pre
      ref={preRef}
      className="m-0 h-[220px] overflow-auto bg-code px-4 py-3 font-mono text-xs leading-relaxed text-muted-foreground"
    >
      <code>{code}</code>
    </pre>
  );
}

export const WidgetCard = memo(function WidgetCard({
  code,
  title,
  height,
  complete,
  sessionId,
}: WidgetCardProps) {
  const t = useT();
  const dark = useIsDark();
  const locale = useI18n((state) => state.locale);
  const readOnly = useTranscript((state) =>
    sessionId ? Boolean(state.bySession[sessionId]?.readOnly) : false,
  );

  const resolvedTitle = title?.trim() || t("widget.default_title");
  const key = useMemo(() => cacheKey(code, resolvedTitle), [code, resolvedTitle]);
  const maxInlineHeight = Math.max(height ?? DEFAULT_MAX_HEIGHT, DEFAULT_MAX_HEIGHT);
  const initialHeight = useMemo(
    () => readCachedHeight(key) ?? height ?? DEFAULT_HEIGHT,
    [key, height],
  );
  const [inlineHeight, setInlineHeight] = useState(initialHeight);
  useEffect(() => {
    setInlineHeight(initialHeight);
  }, [initialHeight]);

  const [mode, setMode] = useState<WidgetMode>("preview");
  const userPickedTabRef = useRef(false);
  const prevCompleteRef = useRef(complete);
  const [reloadNonce, setReloadNonce] = useState(0);
  const [copied, setCopied] = useState(false);
  const [errorCopied, setErrorCopied] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  const [errors, setErrors] = useState<string[]>([]);

  // 围栏闭合时自动从流式代码视图切入预览（尊重用户在流式期间主动点选「代码」）。
  useEffect(() => {
    if (!prevCompleteRef.current && complete && !userPickedTabRef.current) {
      setMode("preview");
    }
    prevCompleteRef.current = complete;
  }, [complete]);

  // 源码变更或手动重置时清空旧报错。
  // biome-ignore lint/correctness/useExhaustiveDependencies: 源码变化或重置时重置错误列表
  useEffect(() => {
    setErrors([]);
  }, [code, reloadNonce]);

  const lineCount = useMemo(() => (code ? code.split("\n").length : 0), [code]);
  const effectiveMode: WidgetMode = complete ? mode : "code";

  const pushError = (message: string) => {
    setErrors((prev) => {
      if (prev.includes(message)) return prev;
      return [...prev.slice(-9), message];
    });
  };

  const handleCopyCode = () => {
    void navigator.clipboard.writeText(code);
    setCopied(true);
    toast.success(t("widget.copied"));
    window.setTimeout(() => setCopied(false), 1200);
  };

  const handleExportHtml = () => {
    const html = buildStandaloneHtml(code, {
      title: resolvedTitle,
      dark,
      tokens: readWidgetTokens(dark),
      lang: locale === "zh" ? "zh-CN" : "en",
    });
    const blob = new Blob([html], { type: "text/html;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = sanitizeFilename(resolvedTitle);
    document.body.append(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
  };

  const handleReset = () => {
    setErrors([]);
    setReloadNonce((n) => n + 1);
  };

  const handleCopyError = () => {
    void navigator.clipboard.writeText(errors.join("\n"));
    setErrorCopied(true);
    window.setTimeout(() => setErrorCopied(false), 1200);
  };

  const handleFixWithAgent = () => {
    if (!sessionId || readOnly || errors.length === 0) return;
    const prompt = t("widget.fix_prompt", {
      title: resolvedTitle,
      error: errors.join("\n"),
    });
    const composer = useComposer.getState();
    const existing = composer.drafts[sessionId]?.text ?? "";
    const next = existing.trim() ? `${existing.trimEnd()}\n\n${prompt}` : prompt;
    composer.setText(sessionId, next);
    setFullscreen(false);
    requestComposerFocus();
  };

  const renderToolbar = (inDialog: boolean) => (
    <div className="flex h-10 shrink-0 items-center justify-between gap-2 border-b border-border-subtle bg-panel/60 px-3">
      <div className="flex min-w-0 items-center gap-2">
        {!complete && <Loader2Icon className="size-3.5 shrink-0 animate-spin text-text-tertiary" />}
        <span className="truncate text-ui font-medium text-foreground">{resolvedTitle}</span>
        {!complete && (
          <span className="shrink-0 text-xs tabular-nums text-text-tertiary">
            {t("widget.generating_lines", { n: lineCount })}
          </span>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-1">
        <div className="flex items-center gap-0.5">
          {(["preview", "code"] as const).map((tab) => {
            const selected = effectiveMode === tab;
            const disabled = !complete && tab === "preview";
            return (
              <button
                key={tab}
                type="button"
                disabled={disabled}
                onClick={() => {
                  userPickedTabRef.current = true;
                  setMode(tab);
                }}
                className={cn(
                  "rounded-md px-2 py-0.5 text-xs transition-colors",
                  selected
                    ? "bg-accent text-foreground"
                    : "text-muted-foreground hover:text-foreground",
                  disabled && "cursor-not-allowed opacity-45 hover:text-muted-foreground",
                )}
              >
                {t(tab === "preview" ? "widget.preview" : "widget.code")}
              </button>
            );
          })}
        </div>
        {complete && (
          <IconButton label={t("widget.reset")} size="xs" onClick={handleReset}>
            <RotateCcwIcon />
          </IconButton>
        )}
        <IconButton label={t("widget.copy")} size="xs" onClick={handleCopyCode}>
          {copied ? <CheckIcon className="text-success" /> : <CopyIcon />}
        </IconButton>
        {complete && (
          <IconButton label={t("widget.export")} size="xs" onClick={handleExportHtml}>
            <DownloadIcon />
          </IconButton>
        )}
        {inDialog ? (
          <IconButton label={t("widget.close")} size="xs" onClick={() => setFullscreen(false)}>
            <XIcon />
          </IconButton>
        ) : (
          complete && (
            <IconButton label={t("widget.fullscreen")} size="xs" onClick={() => setFullscreen(true)}>
              <Maximize2Icon />
            </IconButton>
          )
        )}
      </div>
    </div>
  );

  const renderErrorBar = () => {
    if (errors.length === 0 || effectiveMode !== "preview") return null;
    const latest = errors[errors.length - 1];
    return (
      <div
        role="alert"
        className="flex shrink-0 items-center justify-between gap-2 border-t border-destructive/20 bg-destructive/6 px-3 py-2 text-xs text-destructive"
      >
        <div className="flex min-w-0 flex-1 items-center gap-1.5">
          <AlertTriangleIcon className="size-3.5 shrink-0" />
          <span className="truncate font-mono">{latest}</span>
          {errors.length > 1 && (
            <span className="shrink-0 text-2xs opacity-80">
              {t("widget.error_more", { n: errors.length - 1 })}
            </span>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={handleCopyError}
            className="h-6 px-2 text-xs text-destructive hover:bg-destructive/10 hover:text-destructive"
          >
            {errorCopied ? t("widget.error_copied") : t("widget.copy_error")}
          </Button>
          {sessionId && !readOnly && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={handleFixWithAgent}
              className="h-6 gap-1 px-2 text-xs text-destructive hover:bg-destructive/10 hover:text-destructive"
            >
              <SparklesIcon className="size-3" />
              {t("widget.fix_with_agent")}
            </Button>
          )}
        </div>
      </div>
    );
  };

  const renderCode = (fillHeight: boolean) => (
    <div className={cn("overflow-auto bg-code", fillHeight ? "min-h-0 flex-1" : "max-h-[560px]")}>
      <CodeBlock
        code={code}
        language="html"
        showLineNumbers
        className="rounded-none border-0 bg-transparent [&_code]:text-xs [&_pre]:!bg-transparent [&_pre]:text-xs"
      />
    </div>
  );

  return (
    <TooltipProvider>
      <div className="my-3 overflow-hidden rounded-xl border border-border bg-card">
        {renderToolbar(false)}
        {!complete ? (
          <StreamingSource code={code} />
        ) : fullscreen ? (
          effectiveMode === "preview" ? (
            <div style={{ height: inlineHeight }} className="bg-card" />
          ) : (
            renderCode(false)
          )
        ) : (
          <>
            <div className={cn(effectiveMode !== "preview" && "hidden")}>
              <WidgetFrame
                code={code}
                title={resolvedTitle}
                reloadNonce={reloadNonce}
                maxHeight={maxInlineHeight}
                initialHeight={inlineHeight}
                suspended={effectiveMode !== "preview"}
                onMeasuredHeight={(measured) => {
                  writeCachedHeight(key, measured);
                  setInlineHeight(measured);
                }}
                onError={pushError}
              />
            </div>
            {effectiveMode === "code" && renderCode(false)}
          </>
        )}
        {renderErrorBar()}
      </div>

      <Dialog open={fullscreen} onOpenChange={setFullscreen}>
        <DialogContent
          showCloseButton={false}
          className="flex h-[88vh] max-w-[min(94vw,1400px)] flex-col gap-0 overflow-hidden p-0 sm:max-w-[min(94vw,1400px)]"
        >
          <DialogTitle className="sr-only">{resolvedTitle}</DialogTitle>
          {renderToolbar(true)}
          <div className="flex min-h-0 flex-1 flex-col">
            <div className={cn("flex min-h-0 flex-1 flex-col", effectiveMode !== "preview" && "hidden")}>
              <WidgetFrame
                code={code}
                title={resolvedTitle}
                reloadNonce={reloadNonce}
                initialHeight={initialHeight}
                fillHeight
                suspended={effectiveMode !== "preview"}
                onError={pushError}
              />
            </div>
            {effectiveMode === "code" && renderCode(true)}
          </div>
          {renderErrorBar()}
        </DialogContent>
      </Dialog>
    </TooltipProvider>
  );
});
