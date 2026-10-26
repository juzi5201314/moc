// 计划审阅：markdown 全文 + 执行模型 + 动作。语义对齐 moc-tui 的 `start_plan_execution`：
// 结果 session_id 与当前不同就在新标签打开；有 auto_prompt 就在目标会话里发提示。

import { useEffect, useState } from "react";
import { runAction } from "@/components/layout/session-actions";
import { Markdown } from "@/components/transcript/markdown";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { rpc } from "@/protocol/client";
import type { PlanAction, SessionPlanApproveResult } from "@/protocol/types";
import { sendPrompt } from "@/store/actions";
import { useConnection } from "@/store/connection";
import { openSession } from "@/store/lifecycle";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

/** 模型 × 思考等级展开成 selector 列表。 */
function useSelectors(): { value: string; label: string }[] {
  const models = useConnection((state) => state.config?.models) ?? [];
  return models.flatMap((model) => {
    const levels = model.thinking_levels ?? [];
    if (levels.length === 0) return [{ value: model.id, label: model.display_name }];
    return levels.map((level) => ({
      value: `${model.id}:${level}`,
      label: `${model.display_name} · ${level}`,
    }));
  });
}

export async function approvePlan(
  sessionId: string,
  planId: string,
  action: PlanAction,
  executionModel?: string,
): Promise<void> {
  const result = await rpc.call<SessionPlanApproveResult>("session/plan_approve", {
    session_id: sessionId,
    plan_id: planId,
    action,
    ...(executionModel ? { execution_model: executionModel } : {}),
  });
  useTranscript.getState().mutate(sessionId, (view) => {
    const plan = view.plans.find((item) => item.plan_id === planId);
    if (plan) plan.status = action === "reject" ? "rejected" : "approved";
    view.info.mode = result.mode;
  });
  if (action === "reject") return;
  const target = result.session_id;
  if (target !== sessionId) {
    await useSessions
      .getState()
      .refresh()
      .catch(() => undefined);
    await openSession(target);
  }
  if (result.auto_prompt) {
    await sendPrompt(target, [{ type: "text", text: result.auto_prompt }]);
  }
}

export function PlanReviewDialog() {
  const target = useUi((state) => state.planReview);
  const set = useUi((state) => state.set);
  const plan = useTranscript((state) =>
    target
      ? state.bySession[target.sessionId]?.plans.find((item) => item.plan_id === target.planId)
      : undefined,
  );
  const currentModel = useTranscript((state) =>
    target ? (state.bySession[target.sessionId]?.info.model ?? "") : "",
  );
  const selectors = useSelectors();
  const [model, setModel] = useState("");

  useEffect(() => {
    if (target) setModel(currentModel);
  }, [target, currentModel]);

  const close = () => set({ planReview: null });
  const act = (action: PlanAction) => {
    if (!target) return;
    const { sessionId, planId } = target;
    close();
    void runAction(() => approvePlan(sessionId, planId, action, model || undefined));
  };
  const options =
    model && !selectors.some((item) => item.value === model)
      ? [{ value: model, label: model }, ...selectors]
      : selectors;
  const submitted = plan?.status === "submitted";

  return (
    <Dialog open={target !== null} onOpenChange={(open) => !open && close()}>
      <DialogContent className="flex max-h-[88vh] flex-col sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>{plan?.title ?? "计划"}</DialogTitle>
          <DialogDescription className="font-mono text-xs">{plan?.path}</DialogDescription>
        </DialogHeader>
        <div className="min-h-0 flex-1 overflow-y-auto rounded-lg border p-4">
          {plan ? (
            <Markdown text={plan.markdown} />
          ) : (
            <p className="text-muted-foreground">计划不存在</p>
          )}
        </div>
        <DialogFooter className="flex-wrap items-center gap-2 sm:justify-between">
          <div className="flex items-center gap-2 text-sm">
            <span className="text-muted-foreground">执行模型</span>
            <Select value={model} onValueChange={setModel} disabled={!submitted}>
              <SelectTrigger size="sm" className="w-64" aria-label="执行模型">
                <SelectValue placeholder="当前模型" />
              </SelectTrigger>
              <SelectContent>
                {options.map((item) => (
                  <SelectItem key={item.value} value={item.value}>
                    {item.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="flex flex-wrap gap-2">
            <Button variant="ghost" onClick={close}>
              继续计划
            </Button>
            <Button
              variant="outline"
              className="border-destructive/60 text-destructive"
              disabled={!submitted}
              onClick={() => act("reject")}
            >
              拒绝
            </Button>
            <Button
              variant="secondary"
              disabled={!submitted}
              onClick={() => act("execute_new_session")}
            >
              新会话执行
            </Button>
            <Button
              variant="secondary"
              disabled={!submitted}
              onClick={() => act("execute_compacted")}
            >
              压缩后执行
            </Button>
            <Button disabled={!submitted} onClick={() => act("execute")}>
              执行
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
