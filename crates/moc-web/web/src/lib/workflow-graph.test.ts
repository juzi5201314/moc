import { describe, expect, it } from "vitest";
import { layoutPhases, phaseNodeInfo } from "./workflow-graph";

const PLAN = {
  phases: [
    { id: "phase#1", name: "plan", kind: "phase", sites: ["phase#1", "actor#1", "ask#1"] },
    {
      id: "phase#2",
      name: "build",
      kind: "phase",
      sites: ["phase#2", "actor#2", "ask#2", "world#1"],
    },
    { id: "phase#3", name: "ship", kind: "phase", sites: ["phase#3", "world#2", "hole#1"] },
  ],
  edges: [
    { from: "ask#1", to: "ask#2", kind: "data" },
    { from: "phase#1", to: "phase#2", kind: "order" },
    { from: "world#1", to: "world#2", kind: "data" },
  ],
  roles: [
    { role_id: "actor#1", name_pattern: "planner" },
    { role_id: "actor#2", name_pattern: "worker", fan_out: true },
  ],
};

describe("layoutPhases", () => {
  it("最长路径分层：data 边决定层次", () => {
    const { nodes, layers } = layoutPhases(PLAN);
    const byId = new Map(nodes.map((n) => [n.id, n]));
    // phase#1 → ask#1→ask#2 → phase#2 → world#2 → phase#3
    expect(byId.get("phase#1")?.layer).toBe(0);
    expect(byId.get("phase#2")?.layer).toBe(1);
    expect(byId.get("phase#3")?.layer).toBe(2);
    expect(layers.map((l) => l.map((n) => n.id))).toEqual([["phase#1"], ["phase#2"], ["phase#3"]]);
  });

  it("order 边标 order（与同向 data 边共存不吞）；环上 data 边标 back", () => {
    const { edges } = layoutPhases(PLAN);
    const order = edges.find((e) => e.kind === "order");
    expect(order).toMatchObject({ from: "phase#1", to: "phase#2" });
    // phase 级环：全部压 0 层，此时 data 边 to.layer<=from.layer → back。
    const cyclic = layoutPhases({
      phases: PLAN.phases,
      edges: [
        { from: "ask#1", to: "ask#2", kind: "data" },
        { from: "ask#2", to: "ask#1", kind: "data" },
      ],
    });
    expect(cyclic.edges.every((e) => e.kind === "back")).toBe(true);
  });

  it("phaseNodeInfo：role ×N、ask/world 计数、hole 标识", () => {
    const info = phaseNodeInfo(PLAN.phases[1], PLAN);
    expect(info.roles).toEqual(["worker ×N"]);
    expect(info.asks).toBe(1);
    expect(info.worlds).toBe(1);
    expect(info.hole).toBe(false);
    expect(phaseNodeInfo(PLAN.phases[2], PLAN).hole).toBe(true);
  });
});
