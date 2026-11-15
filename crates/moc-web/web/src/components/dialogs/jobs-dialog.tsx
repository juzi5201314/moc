import {
  ClipboardIcon,
  ExternalLinkIcon,
  ListTodoIcon,
  Loader2Icon,
  OctagonXIcon,
  TerminalIcon,
} from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { t, useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type { JobInfo, JobOutputResult, JobResult } from "@/protocol/types";
import { refreshJobs, useTranscript } from "@/store/transcript";

function statusLabel(job: JobInfo): string {
  if (job.status === "running") return job.stalled ? t("job.stalled") : t("ag.running");
  if (job.status === "killing") return t("job.killing");
  if (job.status === "timed_out") return t("job.timed_out");
  if (job.status === "killed") return t("job.killed");
  return job.exit_code === 0 ? t("ag.completed") : t("job.exited", { code: job.exit_code ?? "?" });
}

function statusClass(job: JobInfo): string {
  if (job.status === "running") return job.stalled ? "text-warn" : "text-brand";
  if (job.status === "exited" && job.exit_code === 0) return "text-success";
  return "text-destructive";
}

function elapsed(job: JobInfo, now: number): string {
  const ms = job.duration_ms ?? Math.max(0, now - job.started_at_ms);
  const seconds = Math.floor(ms / 1000);
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

export function JobsDialog({
  sessionId,
  open,
  onOpenChange,
}: {
  sessionId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const t = useT();
  const jobs = useTranscript((state) => Object.values(state.bySession[sessionId]?.jobs ?? {}));
  const [selected, setSelected] = useState<JobInfo | null>(null);
  const [output, setOutput] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!open) return;
    void refreshJobs(sessionId).catch((error) =>
      toast.error(
        t("job.load_failed", { error: error instanceof Error ? error.message : String(error) }),
      ),
    );
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [open, sessionId, t]);

  const loadOutput = (job: JobInfo) => {
    setSelected(job);
    setOutput(null);
    void rpc
      .call<JobOutputResult>("job/output", {
        session_id: sessionId,
        job_id: job.job_id,
        tail_lines: 200,
      })
      .then((result) => setOutput(result.text))
      .catch((error) =>
        toast.error(
          t("job.log_failed", { error: error instanceof Error ? error.message : String(error) }),
        ),
      );
  };

  const kill = (job: JobInfo) => {
    void rpc
      .call<JobResult>("job/kill", { session_id: sessionId, job_id: job.job_id })
      .then(() => toast.success(t("job.killed_id", { id: job.job_id })))
      .catch((error) =>
        toast.error(
          t("job.kill_failed", { error: error instanceof Error ? error.message : String(error) }),
        ),
      );
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ListTodoIcon className="size-4 text-brand" />
            {t("job.title")}
          </DialogTitle>
          <DialogDescription>{t("job.desc")}</DialogDescription>
        </DialogHeader>
        <div className="max-h-[min(56vh,480px)] space-y-1 overflow-auto pr-1">
          {jobs.length === 0 ? (
            <div className="rounded-lg border border-dashed border-border-subtle px-4 py-8 text-center text-sm text-muted-foreground">
              {t("job.empty")}
            </div>
          ) : (
            jobs.map((job) => (
              <button
                type="button"
                key={job.job_id}
                onClick={() => loadOutput(job)}
                className="group flex w-full items-center gap-3 rounded-lg border border-transparent px-3 py-2.5 text-left hover:border-border-subtle hover:bg-accent/40"
              >
                <TerminalIcon className={cn("size-4 shrink-0", statusClass(job))} />
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2 text-sm">
                    <span className="font-mono text-xs text-text-tertiary">{job.job_id}</span>
                    <span className={cn("text-xs", statusClass(job))}>{statusLabel(job)}</span>
                    <span className="text-xs tabular-nums text-text-tertiary">
                      {elapsed(job, now)}
                    </span>
                  </span>
                  <span className="mt-0.5 block truncate font-mono text-xs text-muted-foreground">
                    $ {job.command}
                  </span>
                </span>
                {job.status === "running" && (
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-xs"
                    title={t("job.kill")}
                    onClick={(event) => {
                      event.stopPropagation();
                      kill(job);
                    }}
                  >
                    <OctagonXIcon className="size-3.5 text-destructive" />
                  </Button>
                )}
              </button>
            ))
          )}
        </div>
        {selected && (
          <div className="space-y-2 border-t border-border-subtle pt-3">
            <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
              <span className="truncate font-mono">
                {selected.job_id} · {t("job.log_tail")}
              </span>
              <div className="flex items-center gap-1">
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  title={t("job.copy_log")}
                  disabled={output === null}
                  onClick={() => {
                    if (output !== null) {
                      void navigator.clipboard.writeText(output);
                      toast.success(t("job.log_copied"));
                    }
                  }}
                >
                  <ClipboardIcon className="size-3.5" />
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  title={t("job.open_log")}
                  onClick={() => window.open(selected.log_uri, "_blank", "noopener,noreferrer")}
                >
                  <ExternalLinkIcon className="size-3.5" />
                </Button>
              </div>
            </div>
            <pre className="max-h-48 overflow-auto rounded-lg bg-code px-3 py-2 font-mono text-xs whitespace-pre-wrap text-muted-foreground">
              {output ?? <Loader2Icon className="size-3.5 animate-spin" />}
            </pre>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
