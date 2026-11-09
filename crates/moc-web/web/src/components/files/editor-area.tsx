// 编辑区：标签条（预览斜体、未保存 ●、×/中键关闭）+ 面包屑 + 操作 + 内容视图。
// 内容按类型渲染：CodeMirror 文本编辑、Markdown 预览/源码、图片/音视频、
// 二进制卡片、截断只读、artifact 日志 ANSI、计划文件走 PlanReview。

import Ansi from "ansi-to-react";
import {
  ArrowLeftIcon,
  CodeIcon,
  CopyIcon,
  DownloadIcon,
  ExternalLinkIcon,
  EyeIcon,
  FileIcon,
  SaveIcon,
  WrapTextIcon,
  XIcon,
} from "lucide-react";
import { lazy, Suspense, useState } from "react";
import { MessageResponse } from "@/components/ai-elements/message";
import { IconButton } from "@/components/common/icon-button";
import { PlanReview } from "@/components/plan/plan-review";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { rawFileUrl } from "@/lib/web-api";
import type { PlanAction, PlanInfo, SessionEngine } from "@/protocol/types";
import { approvePlan } from "@/store/actions";
import {
  activateTab,
  closeTab,
  type FileRoot,
  type OpenFile,
  openFile,
  pinTab,
  reloadFromDisk,
  saveFile,
  setDraft,
  setStackedView,
  targetKey,
  useFiles,
} from "@/store/files";
import { useTranscript } from "@/store/transcript";

// CodeMirror 整包懒加载，不进主包。
const CodeEditor = lazy(() =>
  import("./codemirror-editor").then((module) => ({ default: module.CodeMirrorEditor })),
);

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function EditorArea({
  sessionId,
  workspaceRoot,
  stacked,
}: {
  sessionId: string;
  workspaceRoot: FileRoot | null;
  stacked: boolean;
}) {
  const files = useFiles((state) => state.bySession[sessionId]);
  const tabs = files?.tabs ?? [];
  const active = tabs.find((tab) => targetKey(tab.target) === files?.active);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col">
      {/* 标签条 */}
      <div className="flex h-9 shrink-0 items-center overflow-x-auto px-1 [scrollbar-width:none]">
        {stacked && (
          <IconButton
            label="返回文件树"
            size="sm"
            onClick={() => setStackedView(sessionId, "tree")}
          >
            <ArrowLeftIcon />
          </IconButton>
        )}
        {tabs.map((tab) => {
          const key = targetKey(tab.target);
          const dirty = tab.draft !== undefined && tab.draft !== tab.content;
          return (
            // biome-ignore lint/a11y/useKeyWithClickEvents: 标签中键关闭在 onAuxClick
            // biome-ignore lint/a11y/noStaticElementInteractions: 标签条用 span 才能放中键/aux 事件
            <span
              key={key}
              onClick={() => activateTab(sessionId, key)}
              onAuxClick={(event) => {
                if (event.button === 1) closeTab(sessionId, key);
              }}
              onDoubleClick={() => pinTab(sessionId, key)}
              className={cn(
                "group/tab flex h-7 shrink-0 cursor-default items-center gap-1.5 rounded-lg px-2.5 text-ui",
                key === files?.active
                  ? "bg-accent text-foreground"
                  : "text-muted-foreground hover:text-foreground",
                tab.preview && "italic",
              )}
            >
              <span className="max-w-40 truncate">{tabTitle(tab)}</span>
              {dirty && <span className="size-1.5 rounded-full bg-brand" />}
              <button
                type="button"
                aria-label="关闭"
                onClick={(event) => {
                  event.stopPropagation();
                  closeTab(sessionId, key);
                }}
                className="flex size-4 items-center justify-center rounded text-text-tertiary opacity-0 transition-opacity group-hover/tab:opacity-100 hover:text-foreground"
              >
                <XIcon className="size-3" />
              </button>
            </span>
          );
        })}
      </div>
      {active ? (
        <FileView sessionId={sessionId} tab={active} workspaceRoot={workspaceRoot} />
      ) : null}
      <CloseConfirm sessionId={sessionId} />
    </div>
  );
}

function tabTitle(tab: OpenFile): string {
  const raw = tab.target.kind === "fs" ? tab.target.path : tab.target.uri;
  return raw.split("/").filter(Boolean).at(-1) ?? raw;
}

function FileView({
  sessionId,
  tab,
  workspaceRoot,
}: {
  sessionId: string;
  tab: OpenFile;
  workspaceRoot: FileRoot | null;
}) {
  const key = targetKey(tab.target);
  const dirty = tab.draft !== undefined && tab.draft !== tab.content;
  const [wrap, setWrap] = useState(false);
  const [markdownPreview, setMarkdownPreview] = useState(true);
  const isMarkdown = tabTitle(tab).toLowerCase().endsWith(".md");
  const isResource = tab.target.kind === "resource";
  const plans = useTranscript((state) => state.bySession[sessionId]?.plans);

  const breadcrumb =
    tab.target.kind === "fs"
      ? workspaceRoot?.kind === "workspace" && tab.target.path.startsWith(workspaceRoot.cwd)
        ? tab.target.path.slice(workspaceRoot.cwd.length + 1)
        : tab.target.path
      : tab.target.uri;

  const planFile =
    isResource && tab.target.kind === "resource" && tab.target.uri.match(/^local:\/\/(.+)\.md$/);
  const plan = planFile ? plans?.find((item) => item.plan_id === planFile[1]) : undefined;
  const newer = plan ? plans?.find((item) => item.previous_plan_id === plan.plan_id) : undefined;

  const copyPath = () => void navigator.clipboard.writeText(breadcrumb);
  const download = () => {
    if (tab.target.kind === "fs") window.open(rawFileUrl(tab.target.path, true), "_blank");
  };

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      {/* 头部：面包屑 + 操作 */}
      <div className="flex h-8 shrink-0 items-center gap-1 px-3">
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-text-tertiary">
          {breadcrumb}
        </span>
        {newer && (
          <button
            type="button"
            onClick={() =>
              openFile(
                sessionId,
                { kind: "resource", uri: `local://${newer.plan_id}.md` },
                { pin: true },
              )
            }
            className="shrink-0 text-xs text-brand hover:underline"
          >
            有新版本 v{newer.version}
          </button>
        )}
        {dirty && !isResource && (
          <Button size="xs" className="h-6" onClick={() => void saveFile(sessionId, key)}>
            <SaveIcon className="size-3.5" />
            保存
          </Button>
        )}
        <IconButton label="复制路径" size="xs" onClick={copyPath}>
          <CopyIcon />
        </IconButton>
        {!isResource && (
          <IconButton label="下载" size="xs" onClick={download}>
            <DownloadIcon />
          </IconButton>
        )}
        {isMarkdown && (
          <IconButton
            label={markdownPreview ? "查看源码" : "预览 Markdown"}
            size="xs"
            active={!markdownPreview}
            onClick={() => setMarkdownPreview((value) => !value)}
          >
            {markdownPreview ? <CodeIcon /> : <EyeIcon />}
          </IconButton>
        )}
        <IconButton label="自动换行" size="xs" active={wrap} onClick={() => setWrap((v) => !v)}>
          <WrapTextIcon />
        </IconButton>
      </div>

      {tab.diskConflict && (
        <div className="mx-3 mb-1 flex items-center gap-2 rounded-lg bg-warn/10 px-3 py-1.5 text-xs text-warn">
          <span className="min-w-0 flex-1">文件已在磁盘上被修改</span>
          <button
            type="button"
            className="underline"
            onClick={() => void reloadFromDisk(sessionId, key)}
          >
            载入磁盘版本
          </button>
          <button
            type="button"
            className="underline"
            onClick={() => void saveFile(sessionId, key, true)}
          >
            覆盖保存
          </button>
        </div>
      )}

      <div className="min-h-0 min-w-0 flex-1 overflow-auto">
        {plan ? (
          <PlanPane sessionId={sessionId} plan={plan} />
        ) : (
          <ContentView
            sessionId={sessionId}
            tab={tab}
            wrap={wrap}
            markdownPreview={isMarkdown && markdownPreview}
            isResource={isResource}
          />
        )}
      </div>
    </div>
  );
}

function ContentView({
  sessionId,
  tab,
  wrap,
  markdownPreview,
  isResource,
}: {
  sessionId: string;
  tab: OpenFile;
  wrap: boolean;
  markdownPreview: boolean;
  isResource: boolean;
}) {
  const key = targetKey(tab.target);
  if (tab.loading) return <p className="px-4 py-6 text-xs text-text-tertiary">读取中…</p>;
  if (tab.error) return <p className="px-4 py-6 text-xs text-destructive">{tab.error}</p>;

  const target = tab.target;
  const name = tabTitle(tab).toLowerCase();
  const isJobLog = target.kind === "resource" && target.uri.startsWith("artifact://jobs/");

  // 二进制：图片 / 音视频原生标签 / pdf 与其它安静卡片。
  if (tab.encoding === "binary") {
    if (target.kind === "fs") {
      const url = rawFileUrl(target.path);
      if (/\.(png|jpe?g|gif|webp|avif|bmp|svg|ico)$/.test(name)) {
        return (
          <div className="flex h-full flex-col items-center justify-center gap-2 p-4">
            {/* 棋盘格垫背：透明图也能看清边缘 */}
            <div className="moc-checkerboard flex max-h-full items-center justify-center rounded-xl p-2">
              <img src={url} alt={name} className="max-h-full max-w-full object-contain" />
            </div>
            <span className="text-xs text-text-tertiary">{formatBytes(tab.size)}</span>
          </div>
        );
      }
      if (/\.(mp3|wav|ogg)$/.test(name)) {
        return (
          <MediaWrap>
            {/* biome-ignore lint/a11y/useMediaCaption: 工作区素材预览，无字幕轨 */}
            <audio controls src={url} className="w-full" />
          </MediaWrap>
        );
      }
      if (/\.(mp4|webm)$/.test(name)) {
        return (
          <MediaWrap>
            {/* biome-ignore lint/a11y/useMediaCaption: 工作区素材预览，无字幕轨 */}
            <video controls src={url} className="max-h-full max-w-full" />
          </MediaWrap>
        );
      }
      if (/\.pdf$/.test(name)) {
        return <BinaryCard size={tab.size} onOpen={() => window.open(url, "_blank")} pdf />;
      }
    }
    return <BinaryCard size={tab.size} />;
  }

  const text = tab.content ?? "";

  if (isJobLog) {
    return (
      <pre className="p-4 font-mono text-xs leading-5 whitespace-pre-wrap">
        <Ansi>{text}</Ansi>
      </pre>
    );
  }
  if (tab.truncated) {
    return (
      <div className="flex h-full flex-col">
        <div className="mx-3 mt-2 flex items-center gap-2 rounded-lg bg-muted px-3 py-1.5 text-xs text-muted-foreground">
          <span className="min-w-0 flex-1">文件过大，只显示前 1 MiB</span>
          {tab.target.kind === "fs" && (
            <button
              type="button"
              className="underline"
              onClick={() =>
                tab.target.kind === "fs" && window.open(rawFileUrl(tab.target.path, true), "_blank")
              }
            >
              下载
            </button>
          )}
        </div>
        <pre className="min-h-0 flex-1 overflow-auto p-4 font-mono text-xs leading-5 whitespace-pre-wrap">
          {text}
        </pre>
      </div>
    );
  }
  if (markdownPreview) {
    return (
      <div className="p-4">
        <MessageResponse>{text}</MessageResponse>
      </div>
    );
  }
  return (
    <Suspense fallback={<p className="px-4 py-6 text-xs text-text-tertiary">加载编辑器…</p>}>
      <CodeEditor
        fileName={tabTitle(tab)}
        content={tab.draft ?? text}
        editable={!isResource}
        wrap={wrap}
        line={tab.line}
        lineNonce={tab.lineNonce}
        onChange={(draft: string) => setDraft(sessionId, key, draft)}
      />
    </Suspense>
  );
}

function MediaWrap({ children }: { children: React.ReactNode }) {
  return <div className="flex h-full items-center justify-center p-4">{children}</div>;
}

function BinaryCard({ size, onOpen, pdf }: { size: number; onOpen?: () => void; pdf?: boolean }) {
  return (
    <div className="flex h-full items-center justify-center p-6">
      <div className="flex flex-col items-center gap-2 rounded-xl bg-muted px-6 py-5 text-center">
        <FileIcon className="size-5 text-text-tertiary" />
        <p className="text-ui text-muted-foreground">二进制文件 · {formatBytes(size)}</p>
        {pdf && (
          <Button size="xs" variant="outline" className="h-7" onClick={onOpen}>
            <ExternalLinkIcon className="size-3.5" />
            在新标签页打开
          </Button>
        )}
      </div>
    </div>
  );
}

function formatBytes(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / 1024 / 1024).toFixed(1)} MB`;
}

/** 计划文件：复用 PlanReview 的审阅能力（approve / revise 原逻辑）。 */
function PlanPane({ sessionId, plan }: { sessionId: string; plan: PlanInfo }) {
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const act = async (action: PlanAction, executionEngine?: SessionEngine) => {
    setBusy(true);
    setActionError(null);
    try {
      await approvePlan(sessionId, plan.plan_id, action, executionEngine);
    } catch (error) {
      setActionError(errorText(error));
    } finally {
      setBusy(false);
    }
  };
  return (
    <PlanReview
      sessionId={sessionId}
      plan={plan}
      busy={busy}
      actionError={actionError}
      onAction={(_plan, action, executionEngine) => void act(action, executionEngine)}
    />
  );
}

/** 关闭未保存标签的确认框。 */
function CloseConfirm({ sessionId }: { sessionId: string }) {
  const closing = useFiles((state) => state.bySession[sessionId]?.closing);
  const dismiss = () =>
    useFiles.setState((state) => {
      const current = state.bySession[sessionId];
      return current
        ? { bySession: { ...state.bySession, [sessionId]: { ...current, closing: undefined } } }
        : state;
    });
  return (
    <AlertDialog open={closing !== undefined} onOpenChange={(open) => !open && dismiss()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>关闭 {closing?.title}？</AlertDialogTitle>
          <AlertDialogDescription>有未保存的修改。</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction
            onClick={() => {
              const key = closing?.key;
              dismiss();
              if (key) closeTab(sessionId, key, true);
            }}
          >
            不保存
          </AlertDialogAction>
          <AlertDialogAction
            onClick={() => {
              const key = closing?.key;
              dismiss();
              // 保存失败（如磁盘冲突）时留下标签，由 banner 处理。
              if (key) {
                void saveFile(sessionId, key).then((ok) => {
                  if (ok) closeTab(sessionId, key, true);
                });
              }
            }}
          >
            保存
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
