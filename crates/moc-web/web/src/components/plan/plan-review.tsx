// 计划审阅面板：优先展示「本版改动」，再给完整计划；正文里可选中文字加批注。
//
// 批注只在前端攒草稿，提交时一次交给 core（`session/plan_revise`）；core 生成
// 修订提示后由这里按普通提示发回模型，模型据此产出下一版计划。

import {
  CheckCircle2Icon,
  ChevronRightIcon,
  Loader2Icon,
  MessageSquarePlusIcon,
  PencilIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import { IconButton } from "@/components/common/icon-button";
import { DiffFile } from "@/components/panels/diff-file";
import { PlanMarkdown } from "@/components/plan/plan-markdown";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import {
  appendSuggestion,
  type PlanLineRange,
  sourceLines,
  splitSuggestion,
} from "@/lib/plan-annotation";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  PlanAction,
  PlanAnnotation,
  PlanInfo,
  SessionEngine,
  SessionPlanDiffResult,
  SessionPlanReviseResult,
  ToolDiff,
} from "@/protocol/types";
import { sendPrompt } from "@/store/actions";
import { useTranscript } from "@/store/transcript";

type EditorState = {
  /** 正在编辑的草稿 id；`undefined` 表示新建。 */
  id?: string;
  startLine: number;
  endLine: number;
  quote: string;
  body: string;
};

type Props = {
  sessionId: string;
  plan: PlanInfo;
  busy: boolean;
  actionError: string | null;
  onAction: (plan: PlanInfo, action: PlanAction, executionEngine?: SessionEngine) => void;
};

const EXECUTION_ENGINES: { id: SessionEngine; label: string }[] = [
  { id: "standard", label: "标准" },
  { id: "ptc", label: "PTC" },
  { id: "clm_experimental", label: "CLM 实验" },
];

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

const STATUS_LABEL: Record<PlanInfo["status"], string> = {
  draft: "起草中",
  submitted: "待审阅",
  revised: "等待修订",
  superseded: "已被取代",
  approved: "已批准",
  rejected: "已拒绝",
};

const STATUS_CLASS: Record<PlanInfo["status"], string> = {
  draft: "text-text-tertiary",
  submitted: "text-brand",
  revised: "text-warn",
  superseded: "text-text-tertiary",
  approved: "text-success",
  rejected: "text-destructive",
};

export function PlanReview({ sessionId, plan, busy, actionError, onAction }: Props) {
  const annotatable = plan.status === "submitted";
  const [tab, setTab] = useState<"diff" | "plan">(plan.previous_plan_id ? "diff" : "plan");
  const [diff, setDiff] = useState<ToolDiff | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<PlanAnnotation[]>([]);
  const [editor, setEditor] = useState<EditorState | null>(null);
  const [selection, setSelection] = useState<PlanLineRange | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [showSubmitted, setShowSubmitted] = useState(false);
  const sourceEngine = useTranscript(
    (state) => state.bySession[sessionId]?.info.engine ?? "standard",
  );
  const [executionEngine, setExecutionEngine] = useState<SessionEngine>(sourceEngine);

  const lines = useMemo(() => sourceLines(plan.markdown), [plan.markdown]);
  const submitted = plan.annotations ?? [];

  // 换代（v1 → v2）时清掉草稿与提示，默认回到「本版改动」。
  const lastPlanId = useRef<string | null>(null);
  useEffect(() => {
    // 同一版内的状态刷新（批注落库、批准）不重置草稿。
    if (lastPlanId.current === plan.plan_id) return;
    lastPlanId.current = plan.plan_id;
    setTab(plan.previous_plan_id ? "diff" : "plan");
    setDrafts([]);
    setEditor(null);
    setSelection(null);
    setSubmitError(null);
    setNotice(null);
  }, [plan.plan_id, plan.previous_plan_id]);

  // 编辑器打开时把焦点交给正文，避免用户再点一次。
  const bodyRef = useRef<HTMLTextAreaElement | null>(null);
  const editorOpen = editor !== null;
  useEffect(() => {
    if (editorOpen) bodyRef.current?.focus();
  }, [editorOpen]);

  useEffect(() => {
    setDiff(null);
    setDiffError(null);
    if (!plan.previous_plan_id) return;
    let alive = true;
    rpc
      .call<SessionPlanDiffResult>("session/plan_diff", {
        session_id: sessionId,
        plan_id: plan.plan_id,
      })
      .then((result) => {
        if (alive) setDiff(result.diff);
      })
      .catch((error: unknown) => {
        if (alive) setDiffError(errorMessage(error));
      });
    return () => {
      alive = false;
    };
  }, [sessionId, plan.plan_id, plan.previous_plan_id]);

  const openEditor = (range: PlanLineRange, quote?: string) => {
    if (!annotatable) return;
    const existing = drafts.find(
      (draft) => draft.start_line <= range.endLine && draft.end_line >= range.startLine,
    );
    if (existing) {
      setEditor({
        id: existing.id,
        startLine: existing.start_line,
        endLine: existing.end_line,
        quote: existing.quote ?? "",
        body: existing.body,
      });
      return;
    }
    const source = lines.slice(range.startLine - 1, range.endLine).join("\n");
    setEditor({
      startLine: range.startLine,
      endLine: range.endLine,
      quote: quote?.trim() || source,
      body: "",
    });
  };

  const saveEditor = () => {
    if (!editor) return;
    const body = editor.body.trimEnd();
    if (body.trim().length === 0) {
      setSubmitError("批注正文不能为空");
      return;
    }
    const annotation: PlanAnnotation = {
      id: editor.id ?? crypto.randomUUID(),
      start_line: editor.startLine,
      end_line: editor.endLine,
      quote: editor.quote.slice(0, 4096),
      body,
    };
    setDrafts((current) =>
      editor.id
        ? current.map((draft) => (draft.id === editor.id ? annotation : draft))
        : [...current, annotation],
    );
    setEditor(null);
    setSubmitError(null);
  };

  const submit = async () => {
    if (drafts.length === 0 || !annotatable) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      const result = await rpc.call<SessionPlanReviseResult>("session/plan_revise", {
        session_id: sessionId,
        plan_id: plan.plan_id,
        annotations: drafts,
      });
      setDrafts([]);
      setNotice("批注已提交，等待模型修订");
      await sendPrompt(sessionId, [{ type: "text", text: result.prompt }]);
    } catch (error) {
      setSubmitError(errorMessage(error));
    } finally {
      setSubmitting(false);
    }
  };

  const suggestionCount = drafts.filter((draft) => splitSuggestion(draft.body)).length;

  return (
    <div className="space-y-4 p-4 text-ui">
      <header className="space-y-1">
        <div className="flex items-start justify-between gap-3">
          <h3 className="min-w-0 flex-1 text-sm font-medium text-foreground">{plan.title}</h3>
          <span className={cn("shrink-0 text-xs", STATUS_CLASS[plan.status])}>
            v{plan.version} · {STATUS_LABEL[plan.status]}
          </span>
        </div>
        <p className="truncate font-mono text-2xs text-text-tertiary" title={plan.path}>
          {plan.path}
        </p>
        {plan.previous_plan_id && (
          <p className="truncate text-2xs text-text-tertiary">
            对比上一版 <span className="font-mono">{plan.previous_plan_id}</span>
          </p>
        )}
      </header>

      <div className="flex items-center gap-1">
        <TabButton
          active={tab === "diff"}
          disabled={!plan.previous_plan_id}
          onClick={() => setTab("diff")}
        >
          本版改动
        </TabButton>
        <TabButton active={tab === "plan"} onClick={() => setTab("plan")}>
          完整计划
        </TabButton>
        <span className="ml-auto text-xs text-text-tertiary">
          {drafts.length > 0 ? `${drafts.length} 条待提交` : "无未提交批注"}
        </span>
      </div>

      {tab === "diff" ? (
        diffError ? (
          <Notice tone="error">{diffError}</Notice>
        ) : diff === null ? (
          <p className="flex items-center gap-1.5 text-muted-foreground">
            <Loader2Icon className="size-3.5 animate-spin" />
            正在加载本版改动…
          </p>
        ) : diff.files.length === 0 ? (
          <p className="text-muted-foreground">本版没有文本改动。</p>
        ) : (
          <div className="overflow-hidden rounded-xl ring-1 ring-inset ring-border-subtle">
            {diff.files.map((file) => (
              <DiffFile key={file.path} file={file} />
            ))}
          </div>
        )
      ) : (
        <div className="space-y-2">
          {selection && annotatable && (
            <div className="sticky top-0 z-10 flex items-center justify-between gap-2 rounded-lg bg-popover py-1 pr-1 pl-3 shadow-[var(--elevation-popover)]">
              <span className="min-w-0 truncate text-muted-foreground">
                选中第 {selection.startLine}-{selection.endLine} 行
              </span>
              <div className="flex shrink-0 items-center gap-0.5">
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() => {
                    openEditor(selection);
                    setSelection(null);
                  }}
                >
                  <MessageSquarePlusIcon />
                  批注
                </Button>
                <IconButton size="xs" label="取消选区" onClick={() => setSelection(null)}>
                  <XIcon />
                </IconButton>
              </div>
            </div>
          )}
          {/* 左侧留出 pl-8 给悬停「＋」（块内 -left-6），不能用 overflow-hidden 裁掉它 */}
          <div className="overflow-x-auto rounded-xl py-4 pr-5 pl-8 ring-1 ring-inset ring-border-subtle">
            <PlanMarkdown
              markdown={plan.markdown}
              annotations={[...submitted, ...drafts]}
              onAnnotate={annotatable ? (range) => openEditor(range) : undefined}
              onSelectText={annotatable ? (range) => setSelection(range) : undefined}
            />
          </div>
        </div>
      )}

      {editor && (
        <div className="space-y-2.5 rounded-xl bg-card p-3 shadow-[var(--elevation-composer)]">
          <div className="flex items-center justify-between text-xs text-muted-foreground">
            <span>
              批注第 {editor.startLine}-{editor.endLine} 行
            </span>
            <IconButton size="xs" label="关闭批注编辑器" onClick={() => setEditor(null)}>
              <XIcon />
            </IconButton>
          </div>
          <blockquote className="max-h-24 overflow-auto border-l-2 border-border-strong pl-2.5 font-mono text-2xs whitespace-pre-wrap text-muted-foreground">
            {editor.quote}
          </blockquote>
          <Textarea
            ref={bodyRef}
            value={editor.body}
            onChange={(event) => setEditor({ ...editor, body: event.target.value })}
            onKeyDown={(event) => {
              if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
                event.preventDefault();
                saveEditor();
              }
            }}
            rows={4}
            placeholder="写下意见；可用 ```suggestion 围栏给出替换文本"
            className="resize-y rounded-lg px-2.5 py-2 text-ui shadow-none placeholder:text-text-tertiary md:text-ui"
          />
          <div className="flex items-center justify-between gap-2">
            <Button
              size="xs"
              variant="ghost"
              className="text-muted-foreground"
              onClick={() => {
                const replacement = lines.slice(editor.startLine - 1, editor.endLine).join("\n");
                setEditor({ ...editor, body: appendSuggestion(editor.body, replacement) });
              }}
            >
              插入建议改法
            </Button>
            <Button size="xs" onClick={saveEditor}>
              保存批注
            </Button>
          </div>
        </div>
      )}

      {drafts.length > 0 && (
        <ul className="space-y-2">
          {drafts.map((draft) => (
            <AnnotationCard
              key={draft.id}
              annotation={draft}
              onEdit={() =>
                setEditor({
                  id: draft.id,
                  startLine: draft.start_line,
                  endLine: draft.end_line,
                  quote: draft.quote ?? "",
                  body: draft.body,
                })
              }
              onRemove={() =>
                setDrafts((current) => current.filter((item) => item.id !== draft.id))
              }
            />
          ))}
        </ul>
      )}

      {submitted.length > 0 && (
        <div className="space-y-2">
          <button
            type="button"
            onClick={() => setShowSubmitted((value) => !value)}
            className="flex items-center gap-1 text-xs text-text-tertiary transition-colors hover:text-foreground"
          >
            <ChevronRightIcon
              className={cn("size-3.5 transition-transform", showSubmitted && "rotate-90")}
            />
            已提交的 {submitted.length} 条批注
          </button>
          {showSubmitted && (
            <ul className="space-y-2">
              {submitted.map((annotation) => (
                <AnnotationCard key={annotation.id} annotation={annotation} />
              ))}
            </ul>
          )}
        </div>
      )}

      {notice && (
        <Notice tone="success">
          <CheckCircle2Icon className="size-3.5 shrink-0" />
          {notice}
        </Notice>
      )}
      {(submitError || actionError) && <Notice tone="error">{submitError ?? actionError}</Notice>}

      {annotatable && (
        <div className="space-y-2 border-t border-border-subtle pt-3">
          <div className="flex flex-wrap items-center justify-end gap-1.5">
            <Button
              size="sm"
              variant="ghost"
              className="text-ui"
              onClick={() => onAction(plan, "reject")}
              disabled={busy || submitting}
            >
              拒绝
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="text-ui"
              onClick={() => void submit()}
              disabled={busy || submitting || drafts.length === 0}
              title={drafts.length === 0 ? "先写批注再提交" : `提交 ${drafts.length} 条批注`}
            >
              {submitting && <Loader2Icon className="animate-spin" />}
              提交修改意见{drafts.length > 0 ? `（${drafts.length}）` : ""}
            </Button>
            <Button
              size="sm"
              className="text-ui"
              onClick={() => onAction(plan, "execute")}
              disabled={busy || submitting}
            >
              {busy && <Loader2Icon className="animate-spin" />}
              批准并执行
            </Button>
          </div>
          <div className="flex items-center justify-end gap-1.5">
            <Select
              value={executionEngine}
              onValueChange={(value) => setExecutionEngine(value as SessionEngine)}
              disabled={busy || submitting}
            >
              <SelectTrigger
                size="sm"
                title="新会话执行的引擎"
                className="gap-1 px-2.5 text-ui shadow-none"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent align="end">
                {EXECUTION_ENGINES.map((engine) => (
                  <SelectItem key={engine.id} value={engine.id}>
                    {engine.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button
              size="sm"
              variant="outline"
              className="text-ui"
              onClick={() => onAction(plan, "execute_new_session", executionEngine)}
              disabled={busy || submitting}
            >
              在新会话执行
            </Button>
          </div>
          {suggestionCount > 0 && (
            <p className="text-right text-2xs text-text-tertiary">
              其中 {suggestionCount} 条带建议改法
            </p>
          )}
        </div>
      )}
    </div>
  );
}

function Notice({ tone, children }: { tone: "error" | "success"; children: ReactNode }) {
  return (
    <p
      className={cn(
        "flex items-center gap-2 rounded-lg px-3 py-2 ring-1 ring-inset",
        tone === "error"
          ? "bg-destructive/6 text-destructive ring-destructive/20"
          : "bg-success/8 text-success ring-success/20",
      )}
    >
      {children}
    </p>
  );
}

function TabButton({
  active,
  disabled = false,
  onClick,
  children,
}: {
  active: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "h-7 rounded-lg px-2.5 text-ui transition-colors",
        active
          ? "bg-accent font-medium text-foreground"
          : "text-muted-foreground hover:text-foreground",
        disabled && "cursor-not-allowed opacity-40 hover:text-muted-foreground",
      )}
    >
      {children}
    </button>
  );
}

function AnnotationCard({
  annotation,
  onEdit,
  onRemove,
}: {
  annotation: PlanAnnotation;
  onEdit?: () => void;
  onRemove?: () => void;
}) {
  const parsed = splitSuggestion(annotation.body);
  return (
    <li className="rounded-xl bg-card p-3 ring-1 ring-inset ring-border-subtle">
      <div className="flex h-5 items-center justify-between gap-2 text-2xs text-text-tertiary">
        <span className="font-mono">
          第 {annotation.start_line}-{annotation.end_line} 行
        </span>
        {(onEdit || onRemove) && (
          <span className="-mr-1 flex items-center">
            {onEdit && (
              <IconButton size="xs" label="编辑批注" onClick={onEdit}>
                <PencilIcon className="size-3" />
              </IconButton>
            )}
            {onRemove && (
              <IconButton size="xs" label="删除批注" onClick={onRemove}>
                <Trash2Icon className="size-3" />
              </IconButton>
            )}
          </span>
        )}
      </div>
      <p className="mt-1.5 whitespace-pre-wrap break-words text-foreground">
        {parsed?.note || annotation.body}
      </p>
      {parsed && (
        <pre className="mt-2 overflow-x-auto rounded-lg bg-diff-insert px-2.5 py-2 font-mono text-2xs whitespace-pre-wrap">
          {parsed.suggestion}
        </pre>
      )}
    </li>
  );
}
