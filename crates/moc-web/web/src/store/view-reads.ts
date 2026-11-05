// 一次打开对应一个读取生命周期；关页/重建时取消，不保留已关闭会话的标记。
// 与 store 数据分开，避免 Immer 代理 AbortController；仅按会话 ID 点查。
const reads = new Map<string, AbortController>();

export function startViewReads(sessionId: string): AbortSignal {
  let controller = reads.get(sessionId);
  if (!controller) {
    controller = new AbortController();
    reads.set(sessionId, controller);
  }
  return controller.signal;
}

export function viewReadSignal(sessionId: string): AbortSignal | undefined {
  return reads.get(sessionId)?.signal;
}

export function cancelViewReads(sessionId: string): void {
  reads.get(sessionId)?.abort();
  reads.delete(sessionId);
}

export function cancelAllViewReads(): void {
  for (const controller of reads.values()) controller.abort();
  reads.clear();
}
