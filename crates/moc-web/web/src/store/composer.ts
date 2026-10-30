// 每个会话一份输入草稿与待发区（插队 steer / 排队 queue）。

import { create } from "zustand";
import type { ImageSource, InputPart, PromptMode } from "@/protocol/types";

export type Attachment = { id: string; name: string; source: ImageSource };

export type PendingItem = {
  /** 前端本地 key。 */
  key: string;
  kind: "steer" | "queue";
  /** steer 被 core 接受后的 ID。 */
  steerId?: number;
  text: string;
  images: Attachment[];
  /** 仅本条提示生效的执行模式（`/ask` 入队时携带）。 */
  mode?: PromptMode;
};

export type Draft = { text: string; images: Attachment[] };

export type ComposerState = {
  drafts: Record<string, Draft>;
  pending: Record<string, PendingItem[]>;
  setText: (sessionId: string, text: string) => void;
  addImage: (sessionId: string, image: Attachment) => void;
  removeImage: (sessionId: string, id: string) => void;
  clearDraft: (sessionId: string) => void;
  restore: (sessionId: string, draft: Draft) => void;
  pushPending: (sessionId: string, item: PendingItem, front?: boolean) => void;
  updatePending: (sessionId: string, key: string, patch: Partial<PendingItem>) => void;
  removePending: (sessionId: string, key: string) => void;
};

const EMPTY: Draft = { text: "", images: [] };

export const useComposer = create<ComposerState>()((set) => ({
  drafts: {},
  pending: {},
  setText: (sessionId, text) =>
    set((state) => ({
      drafts: { ...state.drafts, [sessionId]: { ...(state.drafts[sessionId] ?? EMPTY), text } },
    })),
  addImage: (sessionId, image) =>
    set((state) => {
      const draft = state.drafts[sessionId] ?? EMPTY;
      return {
        drafts: { ...state.drafts, [sessionId]: { ...draft, images: [...draft.images, image] } },
      };
    }),
  removeImage: (sessionId, id) =>
    set((state) => {
      const draft = state.drafts[sessionId] ?? EMPTY;
      return {
        drafts: {
          ...state.drafts,
          [sessionId]: { ...draft, images: draft.images.filter((image) => image.id !== id) },
        },
      };
    }),
  clearDraft: (sessionId) => set((state) => ({ drafts: { ...state.drafts, [sessionId]: EMPTY } })),
  restore: (sessionId, draft) =>
    set((state) => ({ drafts: { ...state.drafts, [sessionId]: draft } })),
  pushPending: (sessionId, item, front = false) =>
    set((state) => {
      const list = state.pending[sessionId] ?? [];
      return {
        pending: { ...state.pending, [sessionId]: front ? [item, ...list] : [...list, item] },
      };
    }),
  updatePending: (sessionId, key, patch) =>
    set((state) => ({
      pending: {
        ...state.pending,
        [sessionId]: (state.pending[sessionId] ?? []).map((item) =>
          item.key === key ? { ...item, ...patch } : item,
        ),
      },
    })),
  removePending: (sessionId, key) =>
    set((state) => ({
      pending: {
        ...state.pending,
        [sessionId]: (state.pending[sessionId] ?? []).filter((item) => item.key !== key),
      },
    })),
}));

export function toInput(text: string, images: Attachment[]): InputPart[] {
  const parts: InputPart[] = [];
  if (text) parts.push({ type: "text", text });
  for (const image of images) parts.push({ type: "image", source: image.source });
  return parts;
}
