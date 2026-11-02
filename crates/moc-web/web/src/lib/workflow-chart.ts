// chart artifact → Mermaid 文本：line/bar 走 `xychart-beta`，scatter 走
// `quadrantChart`（按数据范围归一化，标题注明实际轴范围）。baseline 画成常值 series。

export type ChartField = { field?: string; label?: string; unit?: string };

export type ChartSpecLite = {
  type?: string;
  x?: ChartField;
  y?: ChartField | ChartField[];
  scale?: string;
  baseline?: ChartField;
  title?: string;
};

function labelOf(field: ChartField | undefined, fallback: string): string {
  return field?.label ?? field?.field ?? fallback;
}

function getField(item: unknown, field: string): unknown {
  let cur = item;
  for (const part of field.split(".")) {
    if (cur == null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[part];
  }
  return cur;
}

function num(v: unknown): number | undefined {
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
}

/**
 * spec + report items → Mermaid 源。无数据点返回 null。
 * xychart-beta 只支持线性 y；`scale:"log"` 时在标题注明按线性绘制。
 */
export function chartToMermaid(spec: ChartSpecLite, items: unknown[]): string | null {
  const type = spec.type ?? "line";
  const xField = spec.x?.field ?? "";
  const yFields = Array.isArray(spec.y) ? spec.y : spec.y ? [spec.y] : [];
  if (yFields.length === 0) return null;

  // 每条 series 收点（x 允许非数值——xychart 的 x 轴接受类目标签）。
  const series = yFields.map((f) => ({
    label: labelOf(f, "y"),
    points: items
      .map((item) => ({ x: getField(item, xField), y: num(getField(item, f?.field ?? "")) }))
      .filter((p): p is { x: unknown; y: number } => p.y !== undefined && p.x !== undefined),
  }));
  const categories = series[0]?.points.map((p) => String(p.x)) ?? [];
  if (categories.length === 0) return null;

  const baseline = spec.baseline?.field
    ? items
        .map((item) => num(getField(item, spec.baseline?.field ?? "")))
        .find((v) => v !== undefined)
    : undefined;

  if (type === "scatter") {
    // quadrantChart：把 y 归一化到 0..1，标题注明实际轴范围。
    const all = series.flatMap((s) => s.points.map((p) => p.y));
    const lo = Math.min(...all);
    const hi = Math.max(...all);
    const span = Math.max(hi - lo, 1e-9);
    const xs = series[0].points.map((p) => Number(p.x));
    const xLo = Math.min(...xs.filter(Number.isFinite));
    const xHi = Math.max(...xs.filter(Number.isFinite));
    const xSpan = Math.max(xHi - xLo, 1e-9);
    const lines = [
      "quadrantChart",
      `  title ${spec.title ?? "scatter"} (x ${xLo}..${xHi}, y ${lo}..${hi})`,
      "  x-axis 0 --> 1",
      "  y-axis 0 --> 1",
    ];
    for (const s of series) {
      for (const p of s.points) {
        const nx = Number.isFinite(Number(p.x)) ? (Number(p.x) - xLo) / xSpan : 0.5;
        const ny = (p.y - lo) / span;
        lines.push(`  ${s.label}: [${nx.toFixed(4)}, ${ny.toFixed(4)}]`);
      }
    }
    return lines.join("\n");
  }

  // xychart-beta：line/bar；baseline = 常值 series。
  const kind = type === "bar" ? "bar" : "line";
  const title = `${spec.title ?? "chart"}${spec.scale === "log" ? " (y 轴按线性绘制)" : ""}`;
  const ys = series.flatMap((s) => s.points.map((p) => p.y));
  if (baseline !== undefined) ys.push(baseline);
  const lo = Math.min(0, Math.min(...ys));
  const hi = Math.max(...ys, lo + 1e-9);
  const pad = (hi - lo) * 0.05;
  const lines = [
    "xychart-beta",
    `  title "${title}"`,
    `  x-axis [${categories.map((c) => `"${c}"`).join(", ")}]`,
    `  y-axis ${(lo - pad).toPrecision(4)} --> ${(hi + pad).toPrecision(4)}`,
  ];
  for (const s of series) {
    lines.push(`  ${kind} [${s.points.map((p) => p.y).join(", ")}]`);
  }
  if (baseline !== undefined) {
    lines.push(`  line [${categories.map(() => baseline).join(", ")}]`);
  }
  return lines.join("\n");
}
