// 提问卡片：停在 composer 上方。单选 radio / 多选 checkbox，推荐项默认选中并加「（推荐）」，
// 每题带「自定义」输入框；聚焦的选项有 preview 时在右侧（窄屏在下方）渲染 markdown。
// 提交回 {answers:[{id, selected, custom?}]}；「跳过」回所有题 selected: []。

import { MessageCircleQuestion } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Markdown } from "@/components/transcript/markdown";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";
import type { AskAnswer, AskParams, AskQuestion } from "@/protocol/types";
import { resolveAsk, useTranscript } from "@/store/transcript";
import { requestComposerFocus } from "@/store/ui";

const EMPTY: AskParams[] = [];

export function AskCard({ sessionId }: { sessionId: string }) {
  const queue = useTranscript((state) => state.asks[sessionId] ?? EMPTY);
  const request = queue[0];
  if (!request) return null;
  return <AskCardInner key={request.request_id} request={request} more={queue.length - 1} />;
}

type Answer = { selected: string[]; custom: string };

function initialAnswer(question: AskQuestion): Answer {
  const recommended = (question.recommended ?? [])
    .map((index) => question.options[index]?.label)
    .filter((label): label is string => Boolean(label));
  return { selected: question.multi ? recommended : recommended.slice(0, 1), custom: "" };
}

function AskCardInner({ request, more }: { request: AskParams; more: number }) {
  const [answers, setAnswers] = useState<Record<string, Answer>>(() =>
    Object.fromEntries(request.questions.map((question) => [question.id, initialAnswer(question)])),
  );
  const [preview, setPreview] = useState<string | null>(null);
  const first = useRef<HTMLInputElement>(null);

  useEffect(() => {
    first.current?.focus();
  }, []);

  const finish = (result: AskAnswer[]) => {
    resolveAsk(request.session_id, request.request_id, { answers: result });
    requestComposerFocus();
  };

  const submit = () =>
    finish(
      request.questions.map((question) => {
        const answer = answers[question.id] ?? { selected: [], custom: "" };
        const custom = answer.custom.trim();
        return { id: question.id, selected: answer.selected, ...(custom ? { custom } : {}) };
      }),
    );

  const skip = () =>
    finish(request.questions.map((question) => ({ id: question.id, selected: [] })));

  const toggle = (question: AskQuestion, label: string) =>
    setAnswers((state) => {
      const current = state[question.id] ?? { selected: [], custom: "" };
      const selected = question.multi
        ? current.selected.includes(label)
          ? current.selected.filter((item) => item !== label)
          : [...current.selected, label]
        : [label];
      return { ...state, [question.id]: { ...current, selected } };
    });

  const setCustom = (question: AskQuestion, custom: string) =>
    setAnswers((state) => ({
      ...state,
      [question.id]: { ...(state[question.id] ?? { selected: [], custom: "" }), custom },
    }));

  let firstAssigned = false;

  return (
    <section
      aria-label="模型提问"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          skip();
        }
      }}
      className="mx-auto mb-2 w-full max-w-[780px] rounded-xl border border-brand/40 bg-card p-3 shadow-sm"
    >
      <header className="mb-2 flex items-center gap-2 text-sm">
        <MessageCircleQuestion className="size-4 text-brand" aria-hidden />
        <span className="font-medium">需要你的回答</span>
        {more > 0 ? (
          <span className="ml-auto text-muted-foreground text-xs">还有 {more} 组提问</span>
        ) : null}
      </header>
      <div className={cn("flex flex-col gap-3", preview && "md:flex-row")}>
        <div className="max-h-[50vh] min-w-0 flex-1 space-y-4 overflow-y-auto">
          {request.questions.map((question) => {
            const answer = answers[question.id] ?? { selected: [], custom: "" };
            const recommended = question.recommended ?? [];
            return (
              <fieldset key={question.id} className="space-y-1.5">
                <legend className="mb-1 text-sm">
                  {question.header ? (
                    <span className="mr-2 rounded bg-muted px-1.5 py-0.5 text-muted-foreground text-xs">
                      {question.header}
                    </span>
                  ) : null}
                  {question.question}
                </legend>
                {question.options.map((option, index) => {
                  const checked = answer.selected.includes(option.label);
                  const isFirst = !firstAssigned;
                  firstAssigned = true;
                  return (
                    <label
                      key={option.label}
                      className={cn(
                        "flex cursor-pointer items-start gap-2 rounded-lg border px-2.5 py-1.5 text-sm transition-colors hover:bg-accent/60",
                        checked && "border-brand/60 bg-brand/5",
                      )}
                    >
                      <input
                        ref={isFirst ? first : undefined}
                        type={question.multi ? "checkbox" : "radio"}
                        name={`${request.request_id}-${question.id}`}
                        checked={checked}
                        onChange={() => toggle(question, option.label)}
                        onFocus={() => setPreview(option.preview ?? null)}
                        className="mt-1 accent-[var(--brand)]"
                      />
                      <span className="min-w-0">
                        <span>
                          {option.label}
                          {recommended.includes(index) ? (
                            <span className="text-muted-foreground">（推荐）</span>
                          ) : null}
                        </span>
                        {option.description ? (
                          <span className="block text-muted-foreground text-xs">
                            {option.description}
                          </span>
                        ) : null}
                      </span>
                    </label>
                  );
                })}
                <Input
                  aria-label={`${question.question} 自定义回答`}
                  placeholder="自定义"
                  value={answer.custom}
                  onChange={(event) => setCustom(question, event.target.value)}
                  onFocus={() => setPreview(null)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" && !event.nativeEvent.isComposing) {
                      event.preventDefault();
                      submit();
                    }
                  }}
                  className="h-8"
                />
              </fieldset>
            );
          })}
        </div>
        {preview ? (
          <div className="max-h-[50vh] min-w-0 flex-1 overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-xs">
            <Markdown text={preview} />
          </div>
        ) : null}
      </div>
      <div className="mt-3 flex justify-end gap-2">
        <Button size="sm" variant="ghost" onClick={skip}>
          跳过
        </Button>
        <Button size="sm" onClick={submit}>
          提交
        </Button>
      </div>
    </section>
  );
}
