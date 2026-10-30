// 计划审阅面板：优先展示「本版改动」，再给完整计划；正文里可选中文字加批注。
//
// 批注只在前端攒草稿，提交时一次交给 core（`session/plan_revise`）；core 生成
// 修订提示后由这里按普通提示发回模型，模型据此产出下一版计划。

import {
  CheckCircle2Icon,
  ChevronDownIcon,
  Loader2Icon,
  MessageSquarePlusIcon,
  PencilIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { DiffFile } from "@/components/panels/diff-file";
import { PlanMarkdown } from "@/components/plan/plan-markdown";
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
  draft: "text-muted-foreground",
  submitted: "text-emerald-600 dark:text-emerald-400",
  revised: "text-amber-600 dark:text-amber-400",
  superseded: "text-muted-foreground",
  approved: "text-emerald-600 dark:text-emerald-400",
  rejected: "text-rose-500",
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
        quote: existing.quote,
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
    <div className="space-y-3 p-3 text-xs">
      <header className="space-y-1">
        <div className="flex items-start justify-between gap-2">
          <h3 className="min-w-0 flex-1 font-medium">{plan.title}</h3>
          <span className={cn("shrink-0 text-[10px]", STATUS_CLASS[plan.status])}>
            v{plan.version} · {STATUS_LABEL[plan.status]}
          </span>
        </div>
        <p className="truncate font-mono text-[10px] text-muted-foreground" title={plan.path}>
          {plan.path}
        </p>
      </header>

      {plan.previous_plan_id && (
        <div className="flex items-center gap-2 rounded-md bg-muted/30 px-2 py-1 text-[10px] text-muted-foreground ring-1 ring-inset ring-border-subtle">
          <span>对比上一版</span>
          <span className="font-mono">{plan.previous_plan_id}</span>
        </div>
      )}

      <div className="flex items-center gap-1 border-b border-border-subtle pb-1.5">
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
        <span className="ml-auto text-[10px] text-muted-foreground">
          {drafts.length > 0 ? `${drafts.length} 条待提交` : "无未提交批注"}
        </span>
      </div>

      {tab === "diff" ? (
        <div className="space-y-2">
          {diffError ? (
            <p className="rounded-md border border-rose-500/40 bg-rose-500/5 px-2 py-1.5 text-rose-600 dark:text-rose-400">
              {diffError}
            </p>
          ) : diff === null ? (
            <p className="flex items-center gap-1.5 text-muted-foreground">
              <Loader2Icon className="size-3.5 animate-spin" />
              正在加载本版改动…
            </p>
          ) : diff.files.length === 0 ? (
            <p className="text-muted-foreground">本版没有文本改动。</p>
          ) : (
            <div className="overflow-hidden rounded-md ring-1 ring-inset ring-border-subtle">
              {diff.files.map((file) => (
                <DiffFile key={file.path} file={file} />
              ))}
            </div>
          )}
        </div>
      ) : (
        <div className="space-y-2">
          {selection && annotatable && (
            <div className="sticky top-0 z-10 flex items-center justify-between gap-2 rounded-md border border-amber-500/40 bg-amber-500/5 px-2 py-1.5">
              <span className="min-w-0 truncate text-[11px]">
                选中第 {selection.startLine}-{selection.endLine} 行
              </span>
              <div className="flex shrink-0 items-center gap-1">
                <button
                  type="button"
                  onClick={() => {
                    openEditor(selection);
                    setSelection(null);
                  }}
                  className="inline-flex h-6 items-center gap-1 rounded-md border border-border px-2 text-[11px] hover:bg-muted"
                >
                  <MessageSquarePlusIcon className="size-3" />
                  批注
                </button>
                <button
                  type="button"
                  aria-label="取消选区"
                  onClick={() => setSelection(null)}
                  className="inline-flex size-6 items-center justify-center rounded-md hover:bg-muted"
                >
                  <XIcon className="size-3" />
                </button>
              </div>
            </div>
          )}
          <div className="overflow-hidden rounded-md px-3 py-2 ring-1 ring-inset ring-border-subtle">
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
        <div className="space-y-2 rounded-md bg-background p-2.5 shadow-sm ring-1 ring-inset ring-border-subtle">
          <div className="flex items-center justify-between text-[11px] text-muted-foreground">
            <span>
              批注第 {editor.startLine}-{editor.endLine} 行
            </span>
            <button
              type="button"
              aria-label="关闭批注编辑器"
              onClick={() => setEditor(null)}
              className="inline-flex size-5 items-center justify-center rounded hover:bg-muted"
            >
              <XIcon className="size-3" />
            </button>
          </div>
          <blockquote className="max-h-24 overflow-auto border-l-2 border-border pl-2 font-mono text-[10px] whitespace-pre-wrap text-muted-foreground">
            {editor.quote}
          </blockquote>
          <textarea
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
            className="w-full resize-y rounded-md border border-border bg-background px-2 py-1.5 text-[11px] outline-none focus:border-primary"
          />
          <div className="flex items-center justify-between gap-2">
            <button
              type="button"
              onClick={() => {
                const replacement = lines.slice(editor.startLine - 1, editor.endLine).join("\n");
                setEditor({ ...editor, body: appendSuggestion(editor.body, replacement) });
              }}
              className="h-6 rounded-md border border-border px-2 text-[11px] hover:bg-muted"
            >
              插入建议改法
            </button>
            <button
              type="button"
              onClick={saveEditor}
              className="h-6 rounded-md bg-primary px-2.5 text-[11px] text-primary-foreground"
            >
              保存批注
            </button>
          </div>
        </div>
      )}

      {drafts.length > 0 && (
        <ul className="space-y-1.5">
          {drafts.map((draft) => (
            <AnnotationCard
              key={draft.id}
              annotation={draft}
              onEdit={() =>
                setEditor({
                  id: draft.id,
                  startLine: draft.start_line,
                  endLine: draft.end_line,
                  quote: draft.quote,
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
        <div className="space-y-1.5">
          <button
            type="button"
            onClick={() => setShowSubmitted((value) => !value)}
            className="flex w-full items-center gap-1 text-[10px] text-muted-foreground hover:text-foreground"
          >
            <ChevronDownIcon
              className={cn("size-3 transition-transform", showSubmitted && "rotate-180")}
            />
            已提交的 {submitted.length} 条批注
          </button>
          {showSubmitted && (
            <ul className="space-y-1.5">
              {submitted.map((annotation) => (
                <AnnotationCard key={annotation.id} annotation={annotation} />
              ))}
            </ul>
          )}
        </div>
      )}

      {notice && (
        <p className="flex items-center gap-1.5 rounded-md border border-emerald-500/40 bg-emerald-500/5 px-2 py-1.5 text-emerald-700 dark:text-emerald-400">
          <CheckCircle2Icon className="size-3.5" />
          {notice}
        </p>
      )}
      {(submitError || actionError) && (
        <p className="rounded-md border border-rose-500/40 bg-rose-500/5 px-2 py-1.5 text-rose-600 dark:text-rose-400">
          {submitError ?? actionError}
        </p>
      )}

      {annotatable && (
        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-border-subtle pt-2">
          <button
            type="button"
            onClick={() => onAction(plan, "reject")}
            disabled={busy || submitting}
            className="h-7 rounded-md border border-border px-2.5 text-[11px] hover:bg-muted disabled:opacity-50"
          >
            拒绝
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy || submitting || drafts.length === 0}
            title={drafts.length === 0 ? "先写批注再提交" : `提交 ${drafts.length} 条批注`}
            className="inline-flex h-7 items-center gap-1.5 rounded-md border border-border px-2.5 text-[11px] hover:bg-muted disabled:opacity-50"
          >
            {submitting && <Loader2Icon className="size-3 animate-spin" />}
            提交修改意见{drafts.length > 0 ? `（${drafts.length}）` : ""}
          </button>
          <button
            type="button"
            onClick={() => onAction(plan, "execute")}
            disabled={busy || submitting}
            className="inline-flex h-7 items-center gap-1.5 rounded-md bg-primary px-2.5 text-[11px] text-primary-foreground disabled:opacity-50"
          >
            {busy && <Loader2Icon className="size-3 animate-spin" />}
            批准并执行
          </button>
          <span className="inline-flex h-7 items-center overflow-hidden rounded-md border border-border text-[11px]">
            <select
              value={executionEngine}
              onChange={(event) => setExecutionEngine(event.target.value as SessionEngine)}
              disabled={busy || submitting}
              title="新会话执行的引擎"
              className="h-full bg-background px-1.5 text-[11px] outline-none disabled:opacity-50"
            >
              {EXECUTION_ENGINES.map((engine) => (
                <option key={engine.id} value={engine.id}>
                  {engine.label}
                </option>
              ))}
            </select>
            <button
              type="button"
              onClick={() => onAction(plan, "execute_new_session", executionEngine)}
              disabled={busy || submitting}
              className="h-full border-l border-border px-2 hover:bg-muted disabled:opacity-50"
            >
              在新会话执行
            </button>
          </span>
        </div>
      )}
      {annotatable && suggestionCount > 0 && (
        <p className="text-right text-[10px] text-muted-foreground">
          其中 {suggestionCount} 条带建议改法
        </p>
      )}
    </div>
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
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "h-6 rounded-md px-2 text-[11px] transition-colors",
        active ? "bg-muted font-medium text-foreground" : "text-muted-foreground hover:bg-muted/60",
        disabled && "cursor-not-allowed opacity-40",
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
    <li className="rounded-md bg-muted/20 p-2 ring-1 ring-inset ring-border-subtle">
      <div className="flex items-center justify-between gap-2 text-[10px] text-muted-foreground">
        <span className="font-mono">
          第 {annotation.start_line}-{annotation.end_line} 行
        </span>
        {(onEdit || onRemove) && (
          <span className="flex items-center gap-1">
            {onEdit && (
              <button
                type="button"
                aria-label="编辑批注"
                onClick={onEdit}
                className="inline-flex size-5 items-center justify-center rounded hover:bg-muted"
              >
                <PencilIcon className="size-3" />
              </button>
            )}
            {onRemove && (
              <button
                type="button"
                aria-label="删除批注"
                onClick={onRemove}
                className="inline-flex size-5 items-center justify-center rounded hover:bg-muted"
              >
                <Trash2Icon className="size-3" />
              </button>
            )}
          </span>
        )}
      </div>
      <p className="mt-1 whitespace-pre-wrap break-words">{parsed?.note || annotation.body}</p>
      {parsed && (
        <pre className="mt-1 overflow-x-auto rounded border border-emerald-500/30 bg-emerald-500/5 p-1.5 font-mono text-[10px] whitespace-pre-wrap">
          {parsed.suggestion}
        </pre>
      )}
    </li>
  );
}
