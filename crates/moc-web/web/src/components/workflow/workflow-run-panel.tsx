// workflow run 面板：实时图着色 + role 模型热改 + reports/artifacts/log/holes
// + 停止/续跑；runsOpen 时是 run 列表。
// artifacts 按 member 渲染：file/markdown 预览（session/workflow_artifact 取内容），
// table key 去重、metrics 取最新值、board 按 status 分列（无专用组件，Tailwind 注释处自绘），
// chart 以数据点表呈现（web 无现成 chart 组件，数据完整展示）。

import { useMemo, useState } from "react";
import { toast } from "sonner";
import { Streamdown } from "streamdown";
import { streamdownControls, streamdownPlugins } from "@/components/ai-elements/message";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { WorkflowGraph } from "@/components/workflow/workflow-graph";
import { WorkflowModelSelect } from "@/components/workflow/workflow-model-select";
import { type ChartSpecLite, chartToMermaid } from "@/lib/workflow-chart";
import type { WorkflowRunStatus } from "@/protocol/types";
import { useTranscript } from "@/store/transcript";
import type { WorkflowRunView } from "@/store/workflow";
import {
  openWorkflowRun,
  useWorkflow,
  workflowArtifact,
  workflowResume,
  workflowSetRoleModel,
  workflowStop,
} from "@/store/workflow";

const STATUS_LABEL: Record<string, string> = {
  pending: "等待",
  running: "运行中",
  completed: "已完成",
  errored: "失败",
  stopped: "已停止",
};

function statusText(status: WorkflowRunStatus | null): string {
  if (!status) return "…";
  return STATUS_LABEL[status.state] ?? status.state;
}

/** report item 的 dot path 取值（"a.b" → item.a.b）。 */
function fieldGet(item: unknown, field: string): unknown {
  let cur = item;
  for (const part of field.split(".")) {
    if (cur == null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[part];
  }
  return cur;
}

function fieldText(item: unknown, field: string): string {
  const v = fieldGet(item, field);
  return v == null ? "—" : typeof v === "string" ? v : JSON.stringify(v);
}

function ArtifactBlock({
  run,
  artifact,
  versions,
  sessionId,
}: {
  run: WorkflowRunView;
  artifact: Record<string, unknown>;
  /** 同一 artifact_id 的全部声明（=版本序列，索引 0 为 v1）。 */
  versions: Record<string, unknown>[];
  sessionId: string;
}) {
  const member = String(artifact.member ?? "file");
  const id = String(artifact.artifact_id ?? "");
  const spec = (artifact.spec ?? {}) as Record<string, unknown>;
  const items = run.reports.filter((r) => r.artifact_id === id).map((r) => r.item);
  const [content, setContent] = useState<string | null>(null);
  // 版本从 1 起；默认最新（最后一版）。
  const [version, setVersion] = useState(versions.length);

  const load = (v: number) => {
    void workflowArtifact({
      session_id: sessionId,
      run_id: run.run_id,
      artifact_id: id,
      version: v,
    })
      .then((result) =>
        setContent(
          typeof result.content === "string"
            ? result.content
            : JSON.stringify(result.content, null, 2),
        ),
      )
      .catch((error) => toast.error(`读取 artifact 失败：${error}`));
  };

  return (
    <div className="space-y-1">
      <div className="text-ui text-foreground">
        [{member}] {String(spec.title ?? artifact.title ?? id)}
      </div>
      {versions.length > 1 && (
        <Select
          value={String(version)}
          onValueChange={(v) => {
            setVersion(Number(v));
            setContent(null);
          }}
        >
          <SelectTrigger className="h-7 w-[110px] text-ui">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {Array.from({ length: versions.length }, (_, i) => i + 1).map((v) => (
              <SelectItem key={v} value={String(v)}>
                v{v}/{versions.length}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      )}
      {member === "file" || member === "markdown" ? (
        content === null ? (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => load(version)}
            className="h-7 px-2 text-ui"
          >
            查看内容
          </Button>
        ) : member === "markdown" ? (
          <Streamdown plugins={streamdownPlugins} controls={streamdownControls}>
            {content}
          </Streamdown>
        ) : (
          <pre className="max-h-64 overflow-auto whitespace-pre-wrap text-ui">{content}</pre>
        )
      ) : null}
      {member === "table" && <TableArtifact spec={spec} items={items} />}
      {member === "metrics" && <MetricsArtifact spec={spec} items={items} />}
      {member === "board" && <BoardArtifact spec={spec} items={items} />}
      {member === "chart" && <ChartArtifact spec={spec} items={items} />}
    </div>
  );
}

type Field = { field?: string; label?: string; unit?: string };

function fieldLabel(f: Field): string {
  return `${f.label ?? f.field ?? "?"}${f.unit ? ` ${f.unit}` : ""}`;
}

function TableArtifact({ spec, items }: { spec: Record<string, unknown>; items: unknown[] }) {
  const columns = (spec.columns ?? []) as Field[];
  const key = spec.key as string | undefined;
  // key 去重：同 key 后续项替换（保序取最后值）。
  const rows = useMemo(() => {
    const map = new Map<string, unknown>();
    for (const item of items) {
      map.set(key ? fieldText(item, key) : String(map.size), item);
    }
    return [...map.values()];
  }, [items, key]);
  // 无表格组件，用最小 Tailwind 网格外貌渲染（AI Elements 未提供表格）。
  return (
    <div className="text-ui">
      <div className="flex gap-4 text-muted-foreground">
        {columns.map((c) => (
          <span key={fieldLabel(c)}>{fieldLabel(c)}</span>
        ))}
      </div>
      {rows.map((row, i) => (
        <div key={fieldText(row, (spec.key as string) ?? "id") + String(i)} className="flex gap-4">
          {columns.map((c) => (
            <span key={fieldLabel(c)}>{fieldText(row, c.field ?? "")}</span>
          ))}
        </div>
      ))}
    </div>
  );
}

function MetricsArtifact({ spec, items }: { spec: Record<string, unknown>; items: unknown[] }) {
  const fields = (spec.metrics ?? []) as Field[];
  return (
    <div className="flex flex-wrap gap-3 text-ui">
      {fields.map((f) => {
        const path = f.field ?? "";
        const latest = [...items].reverse().find((item) => fieldGet(item, path) !== undefined);
        const value = latest === undefined ? "—" : fieldText(latest, path);
        return (
          <div key={fieldLabel(f)} className="rounded-lg bg-muted px-3 py-1.5">
            <span className="text-muted-foreground">{fieldLabel(f)}</span> {value}
          </div>
        );
      })}
    </div>
  );
}

function BoardArtifact({ spec, items }: { spec: Record<string, unknown>; items: unknown[] }) {
  const keyF = String(spec.key ?? "");
  const statusF = String(spec.status ?? "");
  const columns = [...((spec.columns ?? []) as string[]), "other"];
  // key 去重：后到同 key 卡移动/更新。
  const cards = useMemo(() => {
    const map = new Map<string, unknown>();
    for (const item of items) map.set(fieldText(item, keyF), item);
    return [...map.entries()];
  }, [items, keyF]);
  const titleF = String(spec.card_title ?? spec.cardTitle ?? keyF);
  return (
    <div className="flex gap-4 text-ui">
      {columns.map((column) => {
        const inColumn = cards.filter(([, item]) =>
          column === "other"
            ? !columns.slice(0, -1).includes(fieldText(item, statusF))
            : fieldText(item, statusF) === column,
        );
        return (
          <div key={column} className="min-w-0 flex-1">
            <div className="text-muted-foreground">
              {column} ×{inColumn.length}
            </div>
            {inColumn.map(([key, item]) => (
              <div key={key} className="truncate">
                {fieldText(item, titleF)}
              </div>
            ))}
          </div>
        );
      })}
    </div>
  );
}

function ChartArtifact({ spec, items }: { spec: Record<string, unknown>; items: unknown[] }) {
  // 经现有 Streamdown/Mermaid 管线渲染（xychart-beta / quadrantChart）。
  const source = chartToMermaid(spec as ChartSpecLite, items);
  if (!source) return <div className="text-ui text-text-tertiary">（无数据点）</div>;
  return (
    <Streamdown plugins={streamdownPlugins} controls={streamdownControls}>
      {`\`\`\`mermaid
${source}
\`\`\``}
    </Streamdown>
  );
}

export function WorkflowRunPanel({ sessionId }: { sessionId: string }) {
  const readOnly = useTranscript((s) => s.bySession[sessionId]?.readOnly ?? true);
  const runsOpen = useWorkflow((s) => s.runsOpen);
  const panelRun = useWorkflow((s) => s.panelRun);
  const runList = useWorkflow((s) => s.runList);
  const view = useWorkflow((s) => (panelRun ? s.runs[panelRun] : undefined));
  const [busy, setBusy] = useState(false);

  const act = (fn: () => Promise<void>, label: string) => {
    setBusy(true);
    void fn()
      .catch((error) =>
        toast.error(`${label}失败：${error instanceof Error ? error.message : error}`),
      )
      .finally(() => setBusy(false));
  };

  if (runsOpen) {
    return (
      <Dialog open onOpenChange={(o) => useWorkflow.setState({ runsOpen: o })}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>workflow runs</DialogTitle>
          </DialogHeader>
          <div className="max-h-[60vh] space-y-1 overflow-y-auto">
            {runList.length === 0 && <div className="text-ui text-muted-foreground">没有 run</div>}
            {runList.map((run) => (
              <button
                key={run.run_id}
                type="button"
                className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-ui hover:bg-accent"
                onClick={() => act(() => openWorkflowRun(sessionId, run.run_id), "打开 run")}
              >
                <Badge variant="secondary">{statusText(run.status)}</Badge>
                <span className="min-w-0 flex-1 truncate">{run.title ?? run.run_id}</span>
              </button>
            ))}
          </div>
        </DialogContent>
      </Dialog>
    );
  }

  if (!panelRun || !view) return null;

  const stoppable = view.status?.state === "running" || view.status?.state === "pending";
  const resumable = view.status?.state === "stopped";

  return (
    <Dialog open onOpenChange={(o) => !o && useWorkflow.setState({ panelRun: null })}>
      <DialogContent className="max-h-[85vh] max-w-3xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            {view.title} <Badge variant="secondary">{statusText(view.status)}</Badge>
          </DialogTitle>
        </DialogHeader>

        <div className="h-[280px]">
          <WorkflowGraph
            plan={view.plan}
            statusOf={(id) => {
              const phase = view.phases.find((p) => p.id === id);
              return phase
                ? {
                    status: phase.status,
                    detail: `ask ${phase.asks_done ?? 0} · world ${phase.world_done ?? 0}`,
                  }
                : { status: "pending" };
            }}
          />
        </div>

        <div className="space-y-2">
          <div className="text-ui text-muted-foreground">roles</div>
          {view.roles.map((role) => (
            <div key={role.role_id} className="flex items-center gap-2 text-ui">
              <span className="min-w-0 flex-1 truncate">
                {role.name ?? role.role_id}
                {role.fan_out && <Badge variant="secondary">×N</Badge>}
              </span>
              <span className="text-text-tertiary">
                {role.status} {role.asks_done ?? 0}/
                {(role.asks_in_flight ?? 0) + (role.asks_done ?? 0)} asks
                {role.spent_tokens ? ` ${role.spent_tokens} tok` : ""}
              </span>
              <WorkflowModelSelect
                value={role.model ?? ""}
                disabled={readOnly || busy}
                onSelect={(selector) => {
                  if (!selector) return;
                  act(
                    () => workflowSetRoleModel(sessionId, view.run_id, role.role_id, selector),
                    "role 模型热改",
                  );
                }}
              />
            </div>
          ))}
        </div>

        {view.reports.length > 0 && (
          <div className="space-y-1">
            <div className="text-ui text-muted-foreground">reports</div>
            {view.reports.slice(-20).map((report) => (
              <div
                key={JSON.stringify(report.item)}
                className="truncate text-ui text-text-tertiary"
              >
                {JSON.stringify(report.item)}
              </div>
            ))}
          </div>
        )}

        {view.artifacts.length > 0 && (
          <div className="space-y-2">
            <div className="text-ui text-muted-foreground">artifacts</div>
            {(() => {
              // 同一 artifact_id 多次声明=版本；展示取最新声明，版本序列全传给块。
              const groups = new Map<string, Record<string, unknown>[]>();
              for (const artifact of view.artifacts) {
                const id = String(artifact.artifact_id ?? "");
                const list = groups.get(id) ?? [];
                list.push(artifact);
                groups.set(id, list);
              }
              return [...groups.entries()].map(([id, versions]) => (
                <ArtifactBlock
                  key={id}
                  run={view}
                  artifact={versions[versions.length - 1]}
                  versions={versions}
                  sessionId={sessionId}
                />
              ));
            })()}
          </div>
        )}

        {view.holes.length > 0 && (
          <div className="space-y-1">
            <div className="text-ui text-muted-foreground">holes</div>
            {view.holes.map((hole) => (
              <div key={String(hole.name)} className="text-ui text-warn">
                ? {String(hole.name)} {typeof hole.prompt === "string" ? `— ${hole.prompt}` : ""}
              </div>
            ))}
          </div>
        )}

        {view.log.length > 0 && (
          <div className="space-y-1">
            <div className="text-ui text-muted-foreground">log</div>
            <pre className="max-h-40 overflow-auto whitespace-pre-wrap text-ui text-text-tertiary">
              {view.log.join("\n")}
            </pre>
          </div>
        )}

        <div className="flex justify-end gap-2">
          {resumable && (
            <Button
              variant="ghost"
              disabled={readOnly || busy}
              onClick={() => act(() => workflowResume(sessionId, view.run_id), "续跑")}
            >
              续跑
            </Button>
          )}
          {stoppable && (
            <Button
              variant="destructive"
              disabled={readOnly || busy}
              onClick={() => act(() => workflowStop(sessionId, view.run_id), "停止")}
            >
              停止
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
