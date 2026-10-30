// core 发来的反向请求：工具授权与结构化提问。两者都停在对话流末尾，处理完即消失。

import { CheckIcon, ShieldAlertIcon, SparklesIcon } from "lucide-react";
import { useState } from "react";
import {
  Confirmation,
  ConfirmationActions,
  ConfirmationRequest,
} from "@/components/ai-elements/confirmation";
import { Question, QuestionInput, type QuestionValue } from "@/components/ai-elements/question";
import { DiffFile } from "@/components/panels/diff-file";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { AskParams, PermissionOption, PermissionRequestParams } from "@/protocol/types";
import { resolveAsk, resolvePermission } from "@/store/transcript";

const OPTION_ORDER: Record<PermissionOption["kind"], number> = {
  reject_once: 0,
  allow_session: 1,
  allow_once: 2,
};

const OPTION_VARIANT: Record<PermissionOption["kind"], "default" | "outline" | "ghost"> = {
  allow_once: "default",
  allow_session: "outline",
  reject_once: "ghost",
};

export function PermissionCard({
  sessionId,
  request,
}: {
  sessionId: string;
  request: PermissionRequestParams;
}) {
  const options = request.options.toSorted((a, b) => OPTION_ORDER[a.kind] - OPTION_ORDER[b.kind]);
  return (
    <Confirmation
      approval={{ id: request.request_id }}
      state="approval-requested"
      className="gap-0 overflow-hidden rounded-xl border-warn/30 bg-card p-0 shadow-[var(--elevation-card)]"
    >
      <ConfirmationRequest>
        <div className="flex items-start gap-3 px-4 pt-3.5 pb-3">
          <span className="mt-0.5 flex size-7 shrink-0 items-center justify-center rounded-lg bg-warn/12 text-warn">
            <ShieldAlertIcon className="size-4" />
          </span>
          <div className="min-w-0 flex-1 space-y-1">
            <div className="text-[13.5px] font-medium text-foreground">
              {request.title || "工具请求授权"}
            </div>
            <div className="flex items-center gap-1.5 text-xs text-text-tertiary">
              <span>工具</span>
              <code className="rounded bg-muted px-1.5 py-px font-mono text-[11px] text-muted-foreground">
                {request.tool_name}
              </code>
            </div>
          </div>
        </div>
        {request.body && (
          <pre className="mx-4 mb-3 max-h-56 overflow-auto rounded-lg border border-border bg-code px-3 py-2 font-mono text-[12px] leading-[19px] whitespace-pre-wrap [overflow-wrap:anywhere] text-muted-foreground">
            {request.body}
          </pre>
        )}
        {request.diff && request.diff.files.length > 0 && (
          <div className="mx-4 mb-3 space-y-2">
            {request.diff.files.map((file) => (
              <DiffFile key={file.path} file={file} maxHeight={280} />
            ))}
          </div>
        )}
      </ConfirmationRequest>
      <ConfirmationActions className="w-full justify-end gap-2 self-stretch border-t border-border-subtle bg-muted/40 px-4 py-2.5">
        {options.map((option) => (
          <Button
            key={option.id}
            size="sm"
            variant={OPTION_VARIANT[option.kind]}
            className={cn("h-8", option.kind === "reject_once" && "text-muted-foreground")}
            onClick={() => resolvePermission(sessionId, request.request_id, option.id)}
          >
            {option.label}
          </Button>
        ))}
      </ConfirmationActions>
    </Confirmation>
  );
}

const EMPTY_ANSWER: QuestionValue = { selectedValues: [], text: "" };

export function AskCard({ sessionId, request }: { sessionId: string; request: AskParams }) {
  const [answers, setAnswers] = useState<Record<string, QuestionValue>>({});
  const answered = (id: string) => {
    const value = answers[id] ?? EMPTY_ANSWER;
    return value.selectedValues.length > 0 || value.text.trim().length > 0;
  };
  const complete = request.questions.every((question) => answered(question.id));

  const submit = () =>
    resolveAsk(sessionId, request.request_id, {
      answers: request.questions.map((question) => {
        const value = answers[question.id] ?? EMPTY_ANSWER;
        const custom = value.text.trim();
        return {
          id: question.id,
          selected: [...value.selectedValues],
          ...(custom ? { custom } : {}),
        };
      }),
    });

  return (
    <div className="overflow-hidden rounded-xl border border-brand/25 bg-card shadow-[var(--elevation-card)]">
      <div className="flex items-center gap-2 border-b border-border-subtle px-4 py-2.5 text-[13px] font-medium">
        <SparklesIcon className="size-4 text-brand" />
        需要你的回答
        {request.questions.length > 1 && (
          <span className="text-xs font-normal text-text-tertiary">
            共 {request.questions.length} 个问题
          </span>
        )}
      </div>
      <div className="divide-y divide-border-subtle">
        {request.questions.map((question, questionIndex) => {
          const value = answers[question.id] ?? EMPTY_ANSWER;
          const selected = question.options.find((option) =>
            value.selectedValues.includes(option.label),
          );
          return (
            <Question
              key={question.id}
              value={value}
              selectionMode={question.multi ? "multiple" : "single"}
              onValueChange={(next) => setAnswers((state) => ({ ...state, [question.id]: next }))}
              onSubmit={() => {
                if (complete) submit();
              }}
              className="space-y-2.5 rounded-none border-0 bg-transparent px-4 py-3.5"
            >
              <div className="space-y-1">
                {question.header && (
                  <span className="inline-block rounded bg-muted px-1.5 py-px text-[11px] font-medium text-muted-foreground">
                    {question.header}
                  </span>
                )}
                <p className="text-[14px] leading-6 font-medium text-foreground">
                  {question.question}
                </p>
                {question.multi && <p className="text-xs text-text-tertiary">可多选</p>}
              </div>
              {question.options.length > 0 && (
                <div className="grid gap-1.5">
                  {question.options.map((option, index) => {
                    const isSelected = value.selectedValues.includes(option.label);
                    const recommended = question.recommended?.includes(index);
                    return (
                      <button
                        key={option.label}
                        type="button"
                        aria-pressed={isSelected}
                        onClick={() => {
                          const current = value.selectedValues;
                          const next = question.multi
                            ? isSelected
                              ? current.filter((item) => item !== option.label)
                              : [...current, option.label]
                            : isSelected
                              ? []
                              : [option.label];
                          setAnswers((state) => ({
                            ...state,
                            [question.id]: { ...value, selectedValues: next },
                          }));
                        }}
                        className={cn(
                          "flex w-full items-start gap-3 rounded-lg border px-3 py-2 text-left transition-colors",
                          isSelected
                            ? "border-brand/50 bg-brand/6"
                            : "border-border hover:border-border-strong hover:bg-surface-hover",
                        )}
                      >
                        <span
                          className={cn(
                            "mt-[3px] flex size-4 shrink-0 items-center justify-center border transition-colors",
                            question.multi ? "rounded-[5px]" : "rounded-full",
                            isSelected
                              ? "border-brand bg-brand text-brand-foreground"
                              : "border-border-strong",
                          )}
                        >
                          {isSelected && <CheckIcon className="size-3" strokeWidth={3} />}
                        </span>
                        <span className="min-w-0 flex-1">
                          <span className="flex items-center gap-2 text-[13.5px] font-medium text-foreground">
                            <span className="text-text-tertiary tabular-nums">{index + 1}.</span>
                            {option.label}
                            {recommended && (
                              <span className="rounded bg-brand/10 px-1.5 py-px text-[10.5px] font-medium text-brand">
                                推荐
                              </span>
                            )}
                          </span>
                          {option.description && (
                            <span className="mt-0.5 block text-xs leading-5 text-muted-foreground">
                              {option.description}
                            </span>
                          )}
                        </span>
                      </button>
                    );
                  })}
                </div>
              )}
              {selected?.preview && (
                <pre className="max-h-48 overflow-auto rounded-lg border border-border bg-code px-3 py-2 font-mono text-[12px] whitespace-pre-wrap text-muted-foreground">
                  {selected.preview}
                </pre>
              )}
              <QuestionInput
                placeholder={question.options.length > 0 ? "其他想法（可选）" : "输入你的回答"}
                rows={question.options.length > 0 ? 1 : 3}
                className="min-h-0 resize-none rounded-lg text-[13px]"
                aria-label={`第 ${questionIndex + 1} 题补充说明`}
              />
            </Question>
          );
        })}
      </div>
      <div className="flex items-center justify-end gap-2 border-t border-border-subtle bg-muted/40 px-4 py-2.5">
        <Button
          size="sm"
          variant="ghost"
          className="h-8 text-muted-foreground"
          onClick={() => resolveAsk(sessionId, request.request_id, { answers: [] })}
        >
          跳过
        </Button>
        <Button size="sm" className="h-8" disabled={!complete} onClick={submit}>
          提交回答
        </Button>
      </div>
    </div>
  );
}
