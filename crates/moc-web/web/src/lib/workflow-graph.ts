// workflow 编排图的纯函数层：plan → phase 节点 + 边 + 最长路径分层布局。
// 语义与 TUI `phase_layers` 一致：只 data 边决定层次（order 边画虚线、回边标出）。

export type WorkflowPlanPhase = {
  id: string;
  name: string;
  kind?: string;
  sites?: string[];
};

export type WorkflowPlanEdge = { from: string; to: string; kind?: string };

export type WorkflowPlan = {
  phases?: WorkflowPlanPhase[];
  edges?: WorkflowPlanEdge[];
  roles?: { role_id?: string; name_pattern?: string; fan_out?: boolean }[];
  model_refs?: { ref_id?: string }[];
  commands?: { command?: string }[];
};

export type GraphNode = {
  id: string;
  name: string;
  kind: string;
  layer: number;
  /** 同层内序号（稳定序）。 */
  index: number;
};

export type GraphEdge = {
  from: string;
  to: string;
  /** data=实线数据边；order=虚线顺序边；back=回到更早层的边（视觉上单独标注）。 */
  kind: "data" | "order" | "back";
};

export type GraphLayout = {
  nodes: GraphNode[];
  edges: GraphEdge[];
  layers: GraphNode[][];
};

/**
 * 最长路径分层：site → phase 映射聚合 data 边为 phase 级边，Kahn 序保证
 * pred 先算；环上剩余 phase 压到 0 层保证仍能出图。order 边与跨层 data 边
 * （to.layer <= from.layer）归 back，order 边归 order，其余归 data。
 */
export function layoutPhases(plan: WorkflowPlan): GraphLayout {
  const phases = plan.phases ?? [];
  const sitePhase = new Map<string, string>();
  for (const phase of phases) {
    for (const site of phase.sites ?? []) sitePhase.set(site, phase.id);
  }
  // phase 级 data 边聚合（自环丢弃）。
  const edgeSet = new Set<string>();
  const dataEdges: [string, string][] = [];
  const orderEdges: [string, string][] = [];
  for (const edge of plan.edges ?? []) {
    const from = sitePhase.get(edge.from);
    const to = sitePhase.get(edge.to);
    if (!from || !to || from === to) continue;
    if (edge.kind === "order") {
      // order 边不与 data 边共享去重键：同向 data 边不能把 order 边吞掉。
      orderEdges.push([from, to]);
      continue;
    }
    const key = `${from}->${to}`;
    if (edgeSet.has(key)) continue;
    edgeSet.add(key);
    dataEdges.push([from, to]);
  }

  const ids = phases.map((p) => p.id);
  const layer = new Map<string, number>();
  const done = new Set<string>();
  let progress = true;
  while (progress) {
    progress = false;
    for (const id of ids) {
      if (done.has(id)) continue;
      const preds = dataEdges.filter(([, to]) => to === id);
      if (!preds.every(([from]) => done.has(from))) continue;
      const level = preds.reduce((max, [from]) => Math.max(max, (layer.get(from) ?? 0) + 1), 0);
      layer.set(id, level);
      done.add(id);
      progress = true;
    }
    if (done.size === ids.length) break;
    if (!progress) {
      // 环：剩余压 0 层，渲染仍能出图。
      for (const id of ids) {
        if (!done.has(id)) {
          layer.set(id, 0);
          done.add(id);
        }
      }
      break;
    }
  }

  const maxLayer = Math.max(0, ...ids.map((id) => layer.get(id) ?? 0));
  const layers: GraphNode[][] = Array.from({ length: maxLayer + 1 }, () => []);
  const nodes: GraphNode[] = [];
  const byId = new Map(phases.map((p) => [p.id, p]));
  for (const id of ids) {
    const phase = byId.get(id);
    const node: GraphNode = {
      id,
      name: phase?.name ?? id,
      kind: phase?.kind ?? "phase",
      layer: layer.get(id) ?? 0,
      index: layers[layer.get(id) ?? 0].length,
    };
    layers[node.layer].push(node);
    nodes.push(node);
  }

  const edges: GraphEdge[] = [];
  for (const [from, to] of dataEdges) {
    edges.push({
      from,
      to,
      kind: (layer.get(to) ?? 0) <= (layer.get(from) ?? 0) ? "back" : "data",
    });
  }
  for (const [from, to] of orderEdges) {
    edges.push({ from, to, kind: "order" });
  }
  return { nodes, edges, layers };
}

/** phase 节点卡片内容：role 名（×N）、ask/world.run 计数、hole 标识。 */
export function phaseNodeInfo(
  phase: WorkflowPlanPhase,
  plan: WorkflowPlan,
): { roles: string[]; asks: number; worlds: number; hole: boolean } {
  const sites = new Set(phase.sites ?? []);
  const roles: string[] = [];
  for (const role of plan.roles ?? []) {
    const id = role.role_id ?? "";
    if (sites.has(id)) {
      roles.push(`${role.name_pattern ?? id}${role.fan_out ? " ×N" : ""}`);
    }
  }
  let asks = 0;
  let worlds = 0;
  let hole = false;
  for (const site of phase.sites ?? []) {
    if (site.startsWith("ask#")) asks += 1;
    else if (site.startsWith("world#")) worlds += 1;
    else if (site.startsWith("hole#")) hole = true;
  }
  return { roles, asks, worlds, hole };
}
