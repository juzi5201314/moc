// 每个会话一份输入草稿与待发区（插队 steer / 排队 queue）。

import { nanoid } from "nanoid";
import { create } from "zustand";
import { remapFileRefs, remapImageRefs, uniqueRefName } from "@/lib/refs";
import type { ImageSource, InputPart, PromptMode } from "@/protocol/types";

export type Attachment = { id: string; name: string; source: ImageSource };

/** 按路径引用的文件：正文用 `#name` 指代；core 只拿到路径与大小，不内联内容。 */
export type ComposerFile = {
  id: string;
  /** 引用名（不含 `#`），本条消息内唯一。 */
  name: string;
  size: number;
  /** core 所在机器上的绝对路径；草稿会话里尚未上传的浏览器文件没有路径。 */
  path?: string;
  /** 待上传的浏览器文件：草稿会话在发送、真正建出会话后再上传。 */
  blob?: File;
  uploading?: boolean;
};

export type PendingItem = {
  /** 前端本地 key。 */
  key: string;
  kind: "steer" | "queue";
  /** steer 被 core 接受后的 ID。 */
  steerId?: number;
  text: string;
  images: Attachment[];
  files: ComposerFile[];
  /** 仅本条提示生效的执行模式（`/ask` 入队时携带）。 */
  mode?: PromptMode;
};

export type Draft = { text: string; images: Attachment[]; files: ComposerFile[] };

export type ComposerState = {
  drafts: Record<string, Draft>;
  pending: Record<string, PendingItem[]>;
  setText: (sessionId: string, text: string) => void;
  addImage: (sessionId: string, image: Attachment) => void;
  /** 删除图片并改写正文：它的 `#N` 引用随之删除，后面的序号依次前移。 */
  removeImage: (sessionId: string, id: string) => void;
  addFile: (sessionId: string, file: ComposerFile) => void;
  updateFile: (sessionId: string, id: string, patch: Partial<ComposerFile>) => void;
  /** 删除文件并删掉正文里它的 `#name` 引用。 */
  removeFile: (sessionId: string, id: string) => void;
  clearDraft: (sessionId: string) => void;
  restore: (sessionId: string, draft: Draft) => void;
  pushPending: (sessionId: string, item: PendingItem, front?: boolean) => void;
  updatePending: (sessionId: string, key: string, patch: Partial<PendingItem>) => void;
  removePending: (sessionId: string, key: string) => void;
  /** 把待发项移到目标下标（删除后再插入的语义）。 */
  movePending: (sessionId: string, key: string, toIndex: number) => void;
};

const EMPTY: Draft = { text: "", images: [], files: [] };

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
      const removed = draft.images.findIndex((image) => image.id === id) + 1;
      if (removed === 0) return {};
      const text = remapImageRefs(draft.text, draft.images.length, (n) =>
        n === removed ? null : n > removed ? n - 1 : n,
      );
      return {
        drafts: {
          ...state.drafts,
          [sessionId]: { ...draft, text, images: draft.images.filter((image) => image.id !== id) },
        },
      };
    }),
  addFile: (sessionId, file) =>
    set((state) => {
      const draft = state.drafts[sessionId] ?? EMPTY;
      return {
        drafts: { ...state.drafts, [sessionId]: { ...draft, files: [...draft.files, file] } },
      };
    }),
  updateFile: (sessionId, id, patch) =>
    set((state) => {
      const draft = state.drafts[sessionId];
      if (!draft) return {};
      return {
        drafts: {
          ...state.drafts,
          [sessionId]: {
            ...draft,
            files: draft.files.map((file) => (file.id === id ? { ...file, ...patch } : file)),
          },
        },
      };
    }),
  removeFile: (sessionId, id) =>
    set((state) => {
      const draft = state.drafts[sessionId] ?? EMPTY;
      const target = draft.files.find((file) => file.id === id);
      if (!target) return {};
      const text = remapFileRefs(
        draft.text,
        draft.files.map((file) => file.name),
        (name) => (name === target.name ? null : name),
      );
      return {
        drafts: {
          ...state.drafts,
          [sessionId]: { ...draft, text, files: draft.files.filter((file) => file.id !== id) },
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
  movePending: (sessionId, key, toIndex) =>
    set((state) => {
      const list = [...(state.pending[sessionId] ?? [])];
      const from = list.findIndex((item) => item.key === key);
      if (from < 0) return {};
      const [item] = list.splice(from, 1);
      list.splice(Math.max(0, Math.min(toIndex, list.length)), 0, item);
      return { pending: { ...state.pending, [sessionId]: list } };
    }),
}));

/** 发送输入：正文、全部图片（core 按顺序编号对上 `#N`）、已有路径的引用文件。 */
export function toInput(text: string, images: Attachment[], files: ComposerFile[]): InputPart[] {
  const parts: InputPart[] = [];
  if (text) parts.push({ type: "text", text });
  for (const image of images) parts.push({ type: "image", source: image.source });
  for (const file of files)
    if (file.path) parts.push({ type: "file", path: file.path, name: file.name });
  return parts;
}

function sameImage(left: ImageSource, right: ImageSource): boolean {
  if ("Url" in left || "Url" in right)
    return "Url" in left && "Url" in right && left.Url === right.Url;
  return (
    left.Base64.media_type === right.Base64.media_type && left.Base64.data === right.Base64.data
  );
}

/**
 * 把另一份内容（队列项、剪贴板）的图片与文件并入草稿，并改写其文本里的引用：
 * 图片接在已有图片之后（同内容复用原序号），文件同路径复用、重名改名。
 */
export function absorbRefs(
  base: Draft,
  extra: { text: string; images: Attachment[]; files: ComposerFile[] },
): Draft {
  const images = [...base.images];
  const imageMap = new Map<number, number>();
  extra.images.forEach((image, index) => {
    const same = images.findIndex((existing) => sameImage(existing.source, image.source));
    if (same < 0) images.push({ ...image, id: nanoid() });
    imageMap.set(index + 1, same < 0 ? images.length : same + 1);
  });
  const files = [...base.files];
  const fileMap = new Map<string, string>();
  for (const file of extra.files) {
    const same = file.path ? files.find((existing) => existing.path === file.path) : undefined;
    const name =
      same?.name ??
      uniqueRefName(
        file.name,
        files.map((existing) => existing.name),
      );
    if (!same) files.push({ ...file, id: nanoid(), name });
    fileMap.set(file.name, name);
  }
  const text = remapFileRefs(
    remapImageRefs(extra.text, extra.images.length, (n) => imageMap.get(n) ?? n),
    extra.files.map((file) => file.name),
    (name) => fileMap.get(name) ?? name,
  );
  return { text, images, files };
}
