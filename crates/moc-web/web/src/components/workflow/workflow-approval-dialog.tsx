// workflow 审批窗：ApprovalRequested 弹出；图 + role/ModelRef 模型选择 + 命令集
// + 脚本全文 + 批准/拒绝。只读连接（无写租约）全部操作禁用。

import { lazy, Suspense, useMemo, useState } from "react";
import { toast } from "sonner";
import { CodeBlock } from "@/components/ai-elements/code-block";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Textarea } from "@/components/ui/textarea";
import { WorkflowModelSelect } from "@/components/workflow/workflow-model-select";
import { type MsgKey, useT } from "@/lib/i18n";
import type { WorkflowApproval } from "@/protocol/types";
import { useTranscript } from "@/store/transcript";
import { workflowApprove } from "@/store/workflow";

// xyflow 整个子树经这里进首屏 chunk；图只在审批/面板打开时才需要。
const WorkflowGraph = lazy(() =>
  import("@/components/workflow/workflow-graph").then((module) => ({
    default: module.WorkflowGraph,
  })),
);

export type WorkflowApprovalDialogProps = {
  sessionId: string;
  approval: WorkflowApproval | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
};

const KIND_LABEL: Record<WorkflowApproval["kind"], MsgKey> = {
  launch: "wfa.launch",
  amend: "wfa.amend",
  hole_fill: "wfa.hole_fill",
};

export function WorkflowApprovalDialog({
  sessionId,
  approval,
  open,
  onOpenChange,
}: WorkflowApprovalDialogProps) {
  const t = useT();
  const readOnly = useTranscript((s) => s.bySession[sessionId]?.readOnly ?? true);
  const [roleModels, setRoleModels] = useState<Record<string, string>>({});
  const [refMap, setRefMap] = useState<Record<string, string>>({});
  const [rejecting, setRejecting] = useState(false);
  const [feedback, setFeedback] = useState("");
  const [pending, setPending] = useState(false);

  const plan = approval?.plan as Parameters<typeof WorkflowGraph>[0]["plan"];
  const roles = useMemo(
    () => (plan?.roles ?? []) as { role_id?: string; name_pattern?: string; fan_out?: boolean }[],
    [plan],
  );
  const refs = useMemo(() => (plan?.model_refs ?? []) as { ref_id?: string }[], [plan]);
  const commands = useMemo(
    () =>
      approval?.kind === "hole_fill"
        ? (approval.new_commands ?? [])
        : ((plan?.commands ?? []) as { command?: string }[])
            .map((c) => c.command ?? "")
            .filter(Boolean),
    [approval, plan],
  );

  if (!approval) return null;

  const decide = (approved: boolean) => {
    setPending(true);
    const decision = approved
      ? {
          action: "approve" as const,
          role_models: roleModels,
          model_ref_map: refMap,
        }
      : { action: "reject" as const, feedback: feedback.trim() || undefined };
    void workflowApprove(sessionId, approval.request_id, decision)
      .then(() => onOpenChange(false))
      .catch((error) =>
        toast.error(
          t("wfa.failed", { error: error instanceof Error ? error.message : String(error) }),
        ),
      )
      .finally(() => setPending(false));
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>{t("wfa.title", { kind: t(KIND_LABEL[approval.kind]) })}</DialogTitle>
          <DialogDescription>{approval.run_id}</DialogDescription>
        </DialogHeader>

        <div className="h-[300px]">
          <Suspense
            fallback={
              <div className="grid h-full place-items-center text-ui text-muted-foreground">…</div>
            }
          >
            <WorkflowGraph plan={plan} />
          </Suspense>
        </div>

        <div className="space-y-2">
          <div className="text-ui text-muted-foreground">roles</div>
          {roles.map((role) => {
            const id = role.role_id ?? "";
            return (
              <div key={id} className="flex items-center gap-2 text-ui">
                <span className="min-w-0 flex-1 truncate">
                  {role.name_pattern ?? id}
                  {role.fan_out && <Badge variant="secondary">×N</Badge>}
                </span>
                <WorkflowModelSelect
                  value={roleModels[id] ?? approval.defaults.role_models?.[id] ?? ""}
                  disabled={readOnly || pending}
                  onSelect={(selector) => setRoleModels((prev) => ({ ...prev, [id]: selector }))}
                />
              </div>
            );
          })}
          {refs.length > 0 && <div className="text-ui text-muted-foreground">{t("wfa.remap")}</div>}
          {refs.map((ref) => {
            const id = ref.ref_id ?? "";
            return (
              <div key={id} className="flex items-center gap-2 text-ui">
                <span className="min-w-0 flex-1 truncate">model({id})</span>
                <WorkflowModelSelect
                  value={refMap[id] ?? approval.defaults.model_ref_map?.[id] ?? ""}
                  disabled={readOnly || pending}
                  onSelect={(selector) => setRefMap((prev) => ({ ...prev, [id]: selector }))}
                />
              </div>
            );
          })}
        </div>

        {commands.length > 0 && (
          <div className="space-y-1">
            <div className="text-ui text-muted-foreground">
              {approval.kind === "hole_fill" ? t("wfa.new_commands") : t("wfa.commands")}
            </div>
            {commands.map((command) => (
              <div key={command} className="text-ui text-warn">
                {command}
              </div>
            ))}
          </div>
        )}

        {approval.kind !== "hole_fill" && approval.script && (
          <CodeBlock
            code={approval.script}
            language="ts"
            className="min-w-0 [&_pre]:whitespace-pre-wrap [&_pre]:[overflow-wrap:anywhere]"
          />
        )}

        {rejecting && (
          <Textarea
            value={feedback}
            onChange={(event) => setFeedback(event.target.value)}
            placeholder={t("wfa.reject_ph")}
            rows={3}
            disabled={readOnly || pending}
          />
        )}

        <DialogFooter>
          {rejecting ? (
            <>
              <Button variant="ghost" onClick={() => setRejecting(false)} disabled={pending}>
                {t("common.cancel")}
              </Button>
              <Button
                variant="destructive"
                onClick={() => decide(false)}
                disabled={readOnly || pending}
              >
                {t("wfa.confirm_reject")}
              </Button>
            </>
          ) : (
            <>
              <Button
                variant="ghost"
                onClick={() => setRejecting(true)}
                disabled={readOnly || pending}
              >
                {t("wfa.reject")}
              </Button>
              <Button onClick={() => decide(true)} disabled={readOnly || pending}>
                {t("wfa.approve")}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
