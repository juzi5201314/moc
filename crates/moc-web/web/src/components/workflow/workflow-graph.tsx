// workflow 编排图：ai-elements Canvas（@xyflow/react）+ 自定义 phase 节点。
// 布局走 lib/workflow-graph 的最长路径分层；data=实线、order=虚线、back=弯到上层的标注边。

import type {
  Edge as FlowEdge,
  Node as FlowNode,
  ReactFlowState,
  Rect,
  Viewport,
} from "@xyflow/react";
import {
  getNodesBounds,
  getViewportForBounds,
  MarkerType,
  ReactFlowProvider,
  useStore,
} from "@xyflow/react";
import { CheckIcon, CircleHelpIcon, Loader2Icon, MinusIcon, XIcon } from "lucide-react";
import { type RefObject, useEffect, useMemo, useRef, useState } from "react";
import { Canvas } from "@/components/ai-elements/canvas";
import { Controls } from "@/components/ai-elements/controls";
import { Edge } from "@/components/ai-elements/edge";
import { Node, NodeContent, NodeDescription, NodeTitle } from "@/components/ai-elements/node";
import { type MsgKey, useT } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { layoutPhases, phaseNodeInfo, type WorkflowPlan } from "@/lib/workflow-graph";

const NODE_W = 200;
const NODE_H = 96;
const LAYER_GAP = 140;
const ROW_GAP = 40;

type PhaseStatus = string;

function phaseStatus(status: PhaseStatus): { icon: typeof CheckIcon; tone: string; label: MsgKey } {
  switch (status) {
    case "running":
      return {
        icon: Loader2Icon,
        tone: "text-brand motion-safe:animate-spin",
        label: "todo.running",
      };
    case "done":
    case "completed":
      return { icon: CheckIcon, tone: "bg-primary text-primary-foreground", label: "todo.done" };
    case "failed":
    case "errored":
      return { icon: XIcon, tone: "bg-muted text-destructive", label: "wfr.errored" };
    case "cancelled":
    case "stopped":
      return { icon: MinusIcon, tone: "bg-muted text-text-tertiary", label: "wfr.stopped" };
    default:
      return { icon: MinusIcon, tone: "bg-muted text-text-tertiary", label: "wfr.pending" };
  }
}

function PhaseNode({ data }: { data: Record<string, unknown> }) {
  const t = useT();
  const info = data.info as { roles: string[]; asks: number; worlds: number; hole: boolean };
  const { icon: StatusIcon, tone, label } = phaseStatus(String(data.status));
  return (
    <Node
      handles={{ target: true, source: true }}
      className="w-[200px] rounded-2xl border-border bg-card shadow-sm"
    >
      <NodeContent className="space-y-2.5">
        <NodeTitle className="flex items-center gap-2 text-ui leading-tight font-medium">
          <span
            role="img"
            aria-label={t(label)}
            title={t(label)}
            className={cn("flex size-5 shrink-0 items-center justify-center rounded-full", tone)}
          >
            <StatusIcon className="size-3.5" />
          </span>
          <span className="min-w-0 flex-1 break-words">{String(data.name)}</span>
          {info.hole && <CircleHelpIcon className="size-3.5 shrink-0 text-warn" />}
        </NodeTitle>
        <NodeDescription className="space-y-0.5 break-words rounded-lg bg-code px-2 py-1.5 text-xs">
          {info.roles.length > 0 && <div>{info.roles.join(", ")}</div>}
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
const fitViewOptions = { padding: 0.15 };
const MIN_ZOOM = 0.05;
const MAX_ZOOM = 2;
// 不让鼠标拖动画布，保留框选；触摸仍可平移和双指缩放。
const PAN_MOUSE_BUTTONS: number[] = [];

// 受控节点没有回写 measured；只用 React Flow 内部的实际测量结果。
function measuredBounds({ nodeLookup, nodeOrigin }: ReactFlowState): Rect | null {
  if (!nodeLookup.size) return null;
  for (const { measured } of nodeLookup.values()) {
    if (!measured.width || !measured.height) return null;
  }
  return getNodesBounds([...nodeLookup.keys()], { nodeLookup, nodeOrigin });
}

function equalBounds(a: Rect | null, b: Rect | null): boolean {
  return (
    a === b ||
    (!!a && !!b && a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height)
  );
}

function FitOnResize({
  canvas,
  onFit,
}: {
  canvas: RefObject<HTMLDivElement | null>;
  onFit: (viewport: Viewport) => void;
}) {
  const bounds = useStore(measuredBounds, equalBounds);
  const lastFit = useRef<Viewport | null>(null);
  useEffect(() => {
    const container = canvas.current;
    if (!container || !bounds) return;
    const adaptViewport = () => {
      const { clientWidth: width, clientHeight: height } = container;
      if (!width || !height) return;
      const viewport = getViewportForBounds(
        bounds,
        width,
        height,
        MIN_ZOOM,
        MAX_ZOOM,
        fitViewOptions.padding,
      );
      const previous = lastFit.current;
      if (
        previous?.x === viewport.x &&
        previous.y === viewport.y &&
        previous.zoom === viewport.zoom
      )
        return;
      lastFit.current = viewport;
      onFit(viewport);
    };
    const observer = new ResizeObserver(adaptViewport);
    observer.observe(container);
    window.addEventListener("resize", adaptViewport);
    adaptViewport();
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", adaptViewport);
    };
  }, [canvas, bounds, onFit]);
  return null;
}

export type WorkflowGraphProps = {
  plan: WorkflowPlan | null | undefined;
  /** phase_id → 运行态；审批态全 pending。 */
  statusOf?: (phaseId: string) => { status: string; detail?: string };
  className?: string;
  /** 点击节点（run 面板可用来选 role 改模型——这里只透出 phase 点击）。 */
  onNodeClick?: (phaseId: string) => void;
};

export function WorkflowGraph({ plan, statusOf, className, onNodeClick }: WorkflowGraphProps) {
  const t = useT();
  const canvas = useRef<HTMLDivElement>(null);
  const [viewport, setViewport] = useState<Viewport>({ x: 0, y: 0, zoom: 1 });
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
    <ReactFlowProvider initialMinZoom={MIN_ZOOM} initialMaxZoom={MAX_ZOOM}>
      <div
        className={cn(
          "flex h-full min-w-0 flex-col overflow-hidden rounded-2xl border border-border bg-card",
          className,
        )}
      >
        {/* 图例不覆盖画布，以免窄屏自动缩放后遮住节点。 */}
        <p className="shrink-0 border-b border-border-subtle px-3 py-1.5 text-2xs text-text-tertiary">
          {t("wfr.edge_legend")}
        </p>
        <div ref={canvas} className="min-h-0 flex-1">
          <Canvas
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            edgeTypes={{ data: Edge.Animated, order: Edge.Animated }}
            className="moc-workflow-canvas"
            minZoom={MIN_ZOOM}
            maxZoom={MAX_ZOOM}
            panOnDrag={PAN_MOUSE_BUTTONS}
            fitView={false}
            fitViewOptions={fitViewOptions}
            viewport={viewport}
            onViewportChange={setViewport}
            onNodeClick={(_, node) => onNodeClick?.(node.id)}
            defaultEdgeOptions={{
              markerEnd: {
                type: MarkerType.ArrowClosed,
                color: "var(--text-tertiary)",
                width: 14,
                height: 14,
              },
            }}
            nodesConnectable={false}
            proOptions={{ hideAttribution: true }}
          >
            <FitOnResize canvas={canvas} onFit={setViewport} />
          </Canvas>
        </div>
        <div className="flex shrink-0 justify-end border-t border-border-subtle px-2 py-1">
          <Controls
            className="static! m-0! [&>button]:size-8"
            orientation="horizontal"
            fitViewOptions={fitViewOptions}
          />
        </div>
      </div>
    </ReactFlowProvider>
  );
}
