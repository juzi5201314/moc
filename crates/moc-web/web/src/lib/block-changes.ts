// Immer 提交提供的精确变化起点；弱引用不把历次大数组串成常驻历史链。
import type { Block } from "./transcript-model";

const changes = new WeakMap<Block[], { previous: WeakRef<Block[]>; from: number }>();

export function recordBlockChange(previous: Block[], next: Block[], from: number): void {
  if (previous !== next) changes.set(next, { previous: new WeakRef(previous), from });
}

/** React 合并多次提交时至多追溯 32 步；缺少连续证据就让调用者比较真实引用。 */
export function firstChangedBlock(previous: Block[], next: Block[]): number | undefined {
  let current = next;
  let from = next.length;
  for (let depth = 0; depth < 32; depth++) {
    const change = changes.get(current);
    if (!change) return undefined;
    from = Math.min(from, change.from);
    const base = change.previous.deref();
    if (base === previous) return from;
    if (!base) return undefined;
    current = base;
  }
  return undefined;
}
