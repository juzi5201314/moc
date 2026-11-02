import { describe, expect, it } from "vitest";
import { chartToMermaid } from "./workflow-chart";

const items = [
  { x: 0, v: 1.0, w: 2.0 },
  { x: 1, v: 4.0, w: 1.0 },
  { x: 2, v: 2.0, w: 3.0 },
];

describe("chartToMermaid", () => {
  it("line：xychart-beta 含类目轴、值域与 series", () => {
    const out = chartToMermaid(
      { type: "line", x: { field: "x" }, y: { field: "v", label: "speed" } },
      items,
    );
    expect(out).toContain("xychart-beta");
    expect(out).toContain('x-axis ["0", "1", "2"]');
    expect(out).toContain("line [1, 4, 2]");
    expect(out).not.toContain("log");
  });

  it("多 series + baseline 常值线", () => {
    const out = chartToMermaid(
      {
        type: "bar",
        x: { field: "x" },
        y: [{ field: "v" }, { field: "w" }],
        baseline: { field: "v" },
      },
      items,
    );
    expect(out).toContain("bar [1, 4, 2]");
    expect(out).toContain("bar [2, 1, 3]");
    expect(out).toContain("line [1, 1, 1]");
  });

  it("scatter：quadrantChart 归一化并注实际范围", () => {
    const out = chartToMermaid(
      { type: "scatter", x: { field: "x" }, y: { field: "v" } },
      items,
    );
    expect(out).toContain("quadrantChart");
    expect(out).toContain("y 1..4");
  });

  it("log 轴注明按线性绘制；无数据返回 null", () => {
    const out = chartToMermaid(
      { type: "line", x: { field: "x" }, y: { field: "v" }, scale: "log" },
      items,
    );
    expect(out).toContain("按线性");
    expect(chartToMermaid({ x: { field: "x" }, y: { field: "v" } }, [])).toBeNull();
  });
});
