// 轮次派生索引：列表只订阅 ID 顺序，各行只接收自身变化，避免增量广播到全部历史行。
// 键为前端 nextId 生成的唯一块 ID，不是历史日志里可能重复的 entryId。
import { useCallback, useSyncExternalStore } from "react";
import type { Block } from "@/lib/transcript-model";
import { createTurnGrouper, type Turn, type TurnGrouper } from "@/lib/turn";
import { useTranscript } from "./transcript";

const IDS = Symbol("turn-ids");
const EMPTY_IDS: readonly string[] = [];
type Listener = () => void;
type Index = {
  blocks: Block[];
  group: TurnGrouper;
  turns: Turn[];
  byId: Map<string, Turn>;
};
const indexes = new Map<string, Index>();
const listeners = new Map<string, Map<string | typeof IDS, Set<Listener>>>();

function update(index: Index, blocks: Block[]): Set<string> {
  const changed = new Set<string>();
  if (index.blocks === blocks) return changed;
  const before = index.turns;
  const next = index.group(blocks);
  index.blocks = blocks;
  if (before === next) return changed;
  const from = index.group.changedIndex;
  for (let i = from; i < before.length; i++) {
    if (before[i].id !== next[i]?.id) {
      index.byId.delete(before[i].id);
      changed.add(before[i].id);
    }
  }
  for (let i = from; i < next.length; i++) {
    if (index.byId.get(next[i].id) !== next[i]) {
      index.byId.set(next[i].id, next[i]);
      changed.add(next[i].id);
    }
  }
  index.turns = next;
  return changed;
}

function createIndex(): Index {
  return { blocks: [], group: createTurnGrouper(), turns: [], byId: new Map() };
}

function getIndex(sessionId: string): Index | undefined {
  const view = useTranscript.getState().bySession[sessionId];
  if (!view) return undefined;
  let index = indexes.get(sessionId);
  if (!index) {
    index = createIndex();
    indexes.set(sessionId, index);
  }
  if (index.blocks !== view.blocks) update(index, view.blocks);
  return index;
}

function subscribe(sessionId: string, key: string | typeof IDS, listener: Listener): () => void {
  let channels = listeners.get(sessionId);
  if (!channels) {
    channels = new Map();
    listeners.set(sessionId, channels);
  }
  let channel = channels.get(key);
  if (!channel) {
    channel = new Set();
    channels.set(key, channel);
  }
  channel.add(listener);
  return () => {
    channel.delete(listener);
    if (!channel.size) channels.delete(key);
    if (!channels.size) {
      listeners.delete(sessionId);
      indexes.delete(sessionId);
    }
  };
}

// 模块加载时先注册派生更新，再由 React 挂载行订阅；通知时 store 已完成提交。
useTranscript.subscribe((state, previous) => {
  // 被放弃的 React 预渲染也不能永久保留旧的块数组。
  for (const sessionId of indexes.keys()) {
    if (!listeners.has(sessionId)) indexes.delete(sessionId);
  }
  for (const [sessionId, channels] of listeners) {
    const blocks = state.bySession[sessionId]?.blocks;
    if (blocks === previous.bySession[sessionId]?.blocks) continue;
    if (!blocks) {
      indexes.delete(sessionId);
      for (const channel of channels.values()) for (const notify of channel) notify();
      continue;
    }
    const index = indexes.get(sessionId) ?? createIndex();
    const beforeIds = index.group.ids;
    const changed = update(index, blocks);
    indexes.set(sessionId, index);
    if (beforeIds !== index.group.ids) {
      for (const notify of channels.get(IDS) ?? []) notify();
    }
    for (const turnId of changed) {
      for (const notify of channels.get(turnId) ?? []) notify();
    }
  }
});

export function useTurnIds(sessionId: string): readonly string[] {
  return useSyncExternalStore(
    useCallback((notify) => subscribe(sessionId, IDS, notify), [sessionId]),
    useCallback(() => getIndex(sessionId)?.group.ids ?? EMPTY_IDS, [sessionId]),
  );
}

export function useTurn(sessionId: string, turnId: string): Turn | undefined {
  return useSyncExternalStore(
    useCallback((notify) => subscribe(sessionId, turnId, notify), [sessionId, turnId]),
    useCallback(() => getIndex(sessionId)?.byId.get(turnId), [sessionId, turnId]),
  );
}
