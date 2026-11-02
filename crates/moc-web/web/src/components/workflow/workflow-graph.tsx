// workflow 编排图：ai-elements Canvas（@xyflow/react）+ 自定义 phase 节点。
// 布局走 lib/workflow-graph 的最长路径分层；data=实线、order=虚线、back=弯到上层的标注边。

import type { Edge as FlowEdge, Node as FlowNode } from "@xyflow/react";
import { MarkerType } from "@xyflow/react";
import { useMemo } from "react";
import { Canvas } from "@/components/ai-elements/canvas";
import { Controls } from "@/components/ai-elements/controls";
import { Edge } from "@/components/ai-elements/edge";
import { Node, NodeContent, NodeDescription, NodeTitle } from "@/components/ai-elements/node";
import { Panel } from "@/components/ai-elements/panel";
import { cn } from "@/lib/utils";
import { layoutPhases, phaseNodeInfo, type WorkflowPlan } from "@/lib/workflow-graph";

const NODE_W = 200;
const NODE_H = 96;
const LAYER_GAP = 140;
const ROW_GAP = 40;

type PhaseStatus = string;

/** 状态 → 节点强调色类（pending 灰、running 蓝、done 绿、failed 红、cancelled 暗）。 */
function statusClass(status: PhaseStatus): string {
  switch (status) {
    case "running":
      return "border-brand/60 bg-brand/10";
    case "done":
    case "completed":
      return "border-success/60 bg-success/10";
    case "failed":
    case "errored":
      return "border-destructive/60 bg-destructive/10";
    case "cancelled":
    case "stopped":
      return "border-muted-foreground/30 opacity-60";
    default:
      return "border-border";
  }
}

function PhaseNode({ data }: { data: Record<string, unknown> }) {
  const info = data.info as { roles: string[]; asks: number; worlds: number; hole: boolean };
  return (
    <Node
      handles={{ target: true, source: true }}
      className={cn("w-[200px]", statusClass(String(data.status)))}
    >
      <NodeContent>
        <NodeTitle className="flex items-center gap-1.5">
          {info.hole ? "?" : "•"} {String(data.name)}
        </NodeTitle>
        <NodeDescription>
          {info.roles.length > 0 && <div>{info.roles.join("、")}</div>}
          <div>
            ask×{info.asks} world×{info.worlds}
          </div>
          {typeof data.detail === "string" && data.detail && <div>{data.detail}</div>}
        </NodeDescription>
      </NodeContent>
    </Node>
  );
}

const nodeTypes = { phase: PhaseNode };

export type WorkflowGraphProps = {
  plan: WorkflowPlan | null | undefined;
  /** phase_id → 运行态；审批态全 pending。 */
  statusOf?: (phaseId: string) => { status: string; detail?: string };
  className?: string;
  /** 点击节点（run 面板可用来选 role 改模型——这里只透出 phase 点击）。 */
  onNodeClick?: (phaseId: string) => void;
};

export function WorkflowGraph({ plan, statusOf, className, onNodeClick }: WorkflowGraphProps) {
  const { nodes, edges } = useMemo(() => {
    if (!plan) return { nodes: [] as FlowNode[], edges: [] as FlowEdge[] };
    const layout = layoutPhases(plan);
    const nodes: FlowNode[] = layout.nodes.map((node) => {
      const phase = plan.phases?.find((p) => p.id === node.id);
      const state = statusOf?.(node.id) ?? { status: "pending" };
      return {
        id: node.id,
        type: "phase",
        position: { x: node.index * (NODE_W + ROW_GAP), y: node.layer * (NODE_H + LAYER_GAP) },
        data: {
          name: node.name,
          status: state.status,
          detail: state.detail ?? "",
          info: phase ? phaseNodeInfo(phase, plan) : { roles: [], asks: 0, worlds: 0, hole: false },
        },
      };
    });
    const edges: FlowEdge[] = layout.edges.map((edge, index) => ({
      id: `e${index}`,
      source: edge.from,
      target: edge.to,
      type: edge.kind === "order" ? "order" : "data",
      ...(edge.kind === "back" ? { label: "↺", animated: true } : {}),
      ...(edge.kind === "order" ? { animated: false, style: { strokeDasharray: "4 4" } } : {}),
    }));
    return { nodes, edges };
  }, [plan, statusOf]);

  return (
    <Canvas
      nodes={nodes}
      edges={edges}
      nodeTypes={nodeTypes}
      edgeTypes={{ data: Edge.Animated, order: Edge.Animated }}
      className={className}
      onNodeClick={(_, node) => onNodeClick?.(node.id)}
      defaultEdgeOptions={{
        markerEnd: { type: MarkerType.ArrowClosed, width: 16, height: 16 },
      }}
      nodesConnectable={false}
      proOptions={{ hideAttribution: true }}
    >
      <Controls position="bottom-right" />
      <Panel position="top-right" className="text-ui text-text-tertiary">
        {/* 实线=data 虚线=order ↺=回边 */}
        实线 data · 虚线 order · ↺ 回边
      </Panel>
    </Canvas>
  );
}
