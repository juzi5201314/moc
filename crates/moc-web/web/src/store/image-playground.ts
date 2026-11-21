// Image Playground 独立前端状态（与 Agent 主会话完全解耦）。

import { nanoid } from "nanoid";
import { create } from "zustand";
import {
  type ConvertFormat,
  type ConvertImageResponse,
  cancelPgGenerate,
  convertPgImage,
  createPgFavoriteFolder,
  createPgFavoritePreset,
  createPgSession,
  deletePgFavoriteFolder,
  deletePgFavoritePreset,
  deletePgImage,
  deletePgSession,
  type FavoriteFolder,
  type FavoritePreset,
  fetchImageModels,
  fetchPgFavoritesMeta,
  fetchPlaygroundConfig,
  generatePgImages,
  getPgSession,
  type ImageGenParams,
  type ImageModelRow,
  listPgImages,
  listPgSessions,
  type PlaygroundConfig,
  type PlaygroundImage,
  type PlaygroundSession,
  type PlaygroundSessionSummary,
  patchPgFavoritePreset,
  patchPgSession,
  renamePgFavoriteFolder,
  savePlaygroundConfig,
  type TurnInputRef,
  togglePgFavorite,
  type UploadRefInput,
  uploadPgFavorites,
} from "@/lib/image-playground-api";

export type PlaygroundView = "generate" | "gallery" | "favorites";

export type DraftUpload = {
  id: string;
  file: File;
  previewUrl: string;
};

export type SessionDraft = {
  prompt: string;
  uploads: DraftUpload[];
  /** 直接引用的已有图片 / 收藏图 ID，无需重复上传。 */
  refImageIds: string[];
  params: ImageGenParams;
};

export type PresetEditorState = {
  editing?: FavoritePreset | null;
  initialTitle?: string;
  initialPrompt?: string;
  initialImageIds?: string[];
  initialUploads?: UploadRefInput[];
  initialFiles?: File[];
  initialFolderId?: string | null;
};

const EMPTY_DRAFT: SessionDraft = {
  prompt: "",
  uploads: [],
  refImageIds: [],
  params: {},
};

const LAST_MODEL_KEY = "moc-image-pg-model";

export type ImagePlaygroundState = {
  view: PlaygroundView;
  models: ImageModelRow[];
  selectedModel: string;
  config: PlaygroundConfig | null;
  resolvedStorageDir: string;
  sessions: PlaygroundSessionSummary[];
  activeSessionId: string | null;
  sessionDetails: Record<string, PlaygroundSession>;
  imagesById: Record<string, PlaygroundImage>;
  galleryIds: string[];
  favoriteIds: string[];
  favoriteFolders: FavoriteFolder[];
  favoritePresets: FavoritePreset[];
  presetEditor: PresetEditorState | null;
  runningBySession: Record<string, boolean>;
  drafts: Record<string, SessionDraft>;
  lightboxImageId: string | null;
  lightboxList: string[];
  confirmDeleteImage: PlaygroundImage | null;
  confirmDeleteSession: PlaygroundSessionSummary | null;
  pendingConvert: { image: PlaygroundImage; format: ConvertFormat } | null;
  initialized: boolean;

  setView: (view: PlaygroundView) => void;
  setSelectedModel: (model: string) => void;
  setLightbox: (imageId: string | null, list?: string[]) => void;
  setConfirmDeleteImage: (image: PlaygroundImage | null) => void;
  setConfirmDeleteSession: (session: PlaygroundSessionSummary | null) => void;
  setPendingConvert: (pending: { image: PlaygroundImage; format: ConvertFormat } | null) => void;
  openPresetEditor: (state: PresetEditorState) => void;
  closePresetEditor: () => void;

  bootstrap: () => Promise<void>;
  refreshModels: () => Promise<void>;
  refreshConfig: () => Promise<void>;
  updateConfig: (config: PlaygroundConfig) => Promise<void>;
  refreshImages: () => Promise<void>;
  selectSession: (id: string) => Promise<void>;
  newSession: () => Promise<string>;
  renameSession: (id: string, title: string) => Promise<void>;
  removeSession: (id: string) => Promise<void>;

  updateDraft: (sessionId: string, updater: (prev: SessionDraft) => SessionDraft) => void;
  addUploadFiles: (sessionId: string, files: File[]) => void;
  removeUploadFile: (sessionId: string, uploadId: string) => void;
  toggleRefImage: (sessionId: string, imageId: string) => void;
  removeRefImage: (sessionId: string, imageId: string) => void;
  editWithImage: (imageId: string) => Promise<void>;
  applyFavoritePreset: (preset: FavoritePreset, targetSessionId?: string) => Promise<void>;

  submitGenerate: (sessionId: string) => Promise<void>;
  retryTurn: (
    sessionId: string,
    prompt: string,
    params: ImageGenParams,
    inputs: TurnInputRef[],
    fallbackModel?: string,
  ) => Promise<void>;
  cancelGenerate: (sessionId: string) => Promise<void>;

  toggleFavorite: (imageId: string) => Promise<void>;
  moveFavoriteImageToFolder: (imageId: string, folderId: string | null) => Promise<void>;
  uploadFavorites: (files: File[], folderId?: string | null) => Promise<PlaygroundImage[]>;
  createFavoriteFolder: (name: string) => Promise<FavoriteFolder>;
  renameFavoriteFolder: (id: string, name: string) => Promise<void>;
  deleteFavoriteFolder: (id: string) => Promise<void>;
  createFavoritePreset: (body: {
    title?: string;
    prompt: string;
    image_ids: string[];
    source_uploads?: UploadRefInput[];
    folder_id: string | null;
  }) => Promise<FavoritePreset>;
  updateFavoritePreset: (
    id: string,
    patch: {
      title?: string;
      prompt?: string;
      image_ids?: string[];
      folder_id?: string | null;
    },
  ) => Promise<FavoritePreset>;
  deleteFavoritePreset: (id: string) => Promise<void>;
  convertImage: (
    imageId: string,
    format: ConvertFormat,
    quality?: number,
  ) => Promise<ConvertImageResponse>;
  removeImage: (imageId: string) => Promise<void>;
};

function mergeImages(
  prev: Record<string, PlaygroundImage>,
  incoming: Iterable<PlaygroundImage>,
): Record<string, PlaygroundImage> {
  const next = { ...prev };
  for (const img of incoming) next[img.id] = img;
  return next;
}

function stripRefFromDrafts(
  drafts: Record<string, SessionDraft>,
  imageId: string,
): Record<string, SessionDraft> {
  let changed = false;
  const next: Record<string, SessionDraft> = {};
  for (const [sid, draft] of Object.entries(drafts)) {
    if (draft.refImageIds.includes(imageId)) {
      changed = true;
      next[sid] = {
        ...draft,
        refImageIds: draft.refImageIds.filter((id) => id !== imageId),
      };
    } else {
      next[sid] = draft;
    }
  }
  return changed ? next : drafts;
}

async function executeGenerateRequest(
  sessionId: string,
  payload: {
    prompt: string;
    model: string;
    params: ImageGenParams;
    sourceImageIds: string[];
    sourceUploads: UploadRefInput[];
    files: File[];
  },
): Promise<void> {
  useImagePlayground.setState((prev) => ({
    runningBySession: { ...prev.runningBySession, [sessionId]: true },
  }));
  try {
    const resp = await generatePgImages(sessionId, payload);
    useImagePlayground.setState((prev) => {
      const existing = prev.sessionDetails[sessionId];
      const nextSession: PlaygroundSession | undefined = existing
        ? {
            ...existing,
            title: resp.session_title || existing.title,
            model: payload.model,
            updated_at_ms: resp.turn.created_at_ms,
            turns: [...existing.turns, resp.turn],
          }
        : undefined;
      const newIds = resp.images.map((i) => i.id);
      return {
        sessionDetails: nextSession
          ? { ...prev.sessionDetails, [sessionId]: nextSession }
          : prev.sessionDetails,
        sessions: prev.sessions
          .map((s) =>
            s.id === sessionId
              ? {
                  ...s,
                  title: resp.session_title || s.title,
                  model: payload.model,
                  updated_at_ms: resp.turn.created_at_ms,
                  turn_count: s.turn_count + 1,
                  image_count: s.image_count + newIds.length,
                }
              : s,
          )
          .toSorted((a, b) => b.updated_at_ms - a.updated_at_ms),
        imagesById: mergeImages(prev.imagesById, resp.images),
        galleryIds: [...newIds, ...prev.galleryIds.filter((id) => !newIds.includes(id))],
      };
    });
  } finally {
    useImagePlayground.setState((prev) => ({
      runningBySession: { ...prev.runningBySession, [sessionId]: false },
    }));
  }
}

export const useImagePlayground = create<ImagePlaygroundState>()((set, get) => ({
  view: "generate",
  models: [],
  selectedModel: localStorage.getItem(LAST_MODEL_KEY) ?? "",
  config: null,
  resolvedStorageDir: "",
  sessions: [],
  activeSessionId: null,
  sessionDetails: {},
  imagesById: {},
  galleryIds: [],
  favoriteIds: [],
  favoriteFolders: [],
  favoritePresets: [],
  presetEditor: null,
  runningBySession: {},
  drafts: {},
  lightboxImageId: null,
  lightboxList: [],
  confirmDeleteImage: null,
  confirmDeleteSession: null,
  pendingConvert: null,
  initialized: false,

  setView: (view) => set({ view }),

  setSelectedModel: (model) => {
    localStorage.setItem(LAST_MODEL_KEY, model);
    set({ selectedModel: model });
    const active = get().activeSessionId;
    if (active) {
      void patchPgSession(active, { model }).catch(() => undefined);
    }
  },

  setLightbox: (imageId, list) =>
    set({
      lightboxImageId: imageId,
      lightboxList: imageId ? (list ?? [imageId]) : [],
    }),

  setConfirmDeleteImage: (confirmDeleteImage) => set({ confirmDeleteImage }),
  setConfirmDeleteSession: (confirmDeleteSession) => set({ confirmDeleteSession }),
  setPendingConvert: (pendingConvert) => set({ pendingConvert }),
  openPresetEditor: (presetEditor) => set({ presetEditor }),
  closePresetEditor: () => set({ presetEditor: null }),

  bootstrap: async () => {
    const [models, cfgResp, sessions, gallery, favorites, favMeta] = await Promise.all([
      fetchImageModels().catch(() => [] as ImageModelRow[]),
      fetchPlaygroundConfig().catch(() => null),
      listPgSessions().catch(() => [] as PlaygroundSessionSummary[]),
      listPgImages("gallery").catch(() => [] as PlaygroundImage[]),
      listPgImages("favorites").catch(() => [] as PlaygroundImage[]),
      fetchPgFavoritesMeta().catch(() => ({
        folders: [] as FavoriteFolder[],
        presets: [] as FavoritePreset[],
        preset_images: [] as PlaygroundImage[],
      })),
    ]);

    const currentModel = get().selectedModel;
    const validModel = models.some((m) => m.id === currentModel)
      ? currentModel
      : (models[0]?.id ?? "");
    if (validModel) localStorage.setItem(LAST_MODEL_KEY, validModel);

    const imagesById = mergeImages(
      mergeImages(mergeImages({}, gallery), favorites),
      favMeta.preset_images,
    );
    const activeId = get().activeSessionId ?? sessions[0]?.id ?? null;

    set({
      models,
      selectedModel: validModel,
      config: cfgResp?.config ?? get().config,
      resolvedStorageDir: cfgResp?.resolved_storage_dir ?? get().resolvedStorageDir,
      sessions,
      imagesById,
      galleryIds: gallery.map((i) => i.id),
      favoriteIds: favorites.map((i) => i.id),
      favoriteFolders: favMeta.folders,
      favoritePresets: favMeta.presets,
      activeSessionId: activeId,
      initialized: true,
    });

    if (activeId) {
      await get().selectSession(activeId);
    }
  },

  refreshModels: async () => {
    const models = await fetchImageModels();
    const current = get().selectedModel;
    const next = models.some((m) => m.id === current) ? current : (models[0]?.id ?? "");
    if (next) localStorage.setItem(LAST_MODEL_KEY, next);
    set({ models, selectedModel: next });
  },

  refreshConfig: async () => {
    const resp = await fetchPlaygroundConfig();
    set({ config: resp.config, resolvedStorageDir: resp.resolved_storage_dir });
  },

  updateConfig: async (config) => {
    const prevDir = get().resolvedStorageDir;
    const resp = await savePlaygroundConfig(config);
    const dirChanged = Boolean(prevDir) && resp.resolved_storage_dir !== prevDir;
    if (!dirChanged) {
      set({ config: resp.config, resolvedStorageDir: resp.resolved_storage_dir });
      return;
    }
    set((state) => {
      for (const draft of Object.values(state.drafts)) {
        for (const up of draft.uploads) URL.revokeObjectURL(up.previewUrl);
      }
      return {
        config: resp.config,
        resolvedStorageDir: resp.resolved_storage_dir,
        sessions: [],
        activeSessionId: null,
        sessionDetails: {},
        imagesById: {},
        galleryIds: [],
        favoriteIds: [],
        favoriteFolders: [],
        favoritePresets: [],
        presetEditor: null,
        runningBySession: {},
        drafts: {},
        lightboxImageId: null,
        lightboxList: [],
        confirmDeleteImage: null,
        confirmDeleteSession: null,
        pendingConvert: null,
      };
    });
    if (get().initialized) {
      await get().bootstrap();
    }
  },

  refreshImages: async () => {
    const [gallery, favorites, favMeta] = await Promise.all([
      listPgImages("gallery"),
      listPgImages("favorites"),
      fetchPgFavoritesMeta(),
    ]);
    set((state) => ({
      imagesById: mergeImages(
        mergeImages(mergeImages(state.imagesById, gallery), favorites),
        favMeta.preset_images,
      ),
      galleryIds: gallery.map((i) => i.id),
      favoriteIds: favorites.map((i) => i.id),
      favoriteFolders: favMeta.folders,
      favoritePresets: favMeta.presets,
    }));
  },

  selectSession: async (id) => {
    set({ activeSessionId: id, view: "generate" });
    const detail = await getPgSession(id);
    set((state) => {
      if (!state.sessions.some((s) => s.id === id)) {
        return state;
      }
      const nextDraft = state.drafts[id] ?? {
        ...EMPTY_DRAFT,
        params: detail.session.params ?? {},
      };
      const sessionModel = detail.session.model;
      const selectedModel =
        state.activeSessionId === id &&
        sessionModel &&
        state.models.some((m) => m.id === sessionModel)
          ? sessionModel
          : state.selectedModel;
      return {
        sessionDetails: { ...state.sessionDetails, [id]: detail.session },
        imagesById: mergeImages(state.imagesById, Object.values(detail.images)),
        drafts: { ...state.drafts, [id]: nextDraft },
        selectedModel,
      };
    });
  },

  newSession: async () => {
    const model = get().selectedModel || undefined;
    const detail = await createPgSession({ model });
    const session = detail.session;
    const summary: PlaygroundSessionSummary = {
      id: session.id,
      title: session.title,
      created_at_ms: session.created_at_ms,
      updated_at_ms: session.updated_at_ms,
      model: session.model,
      turn_count: 0,
      image_count: 0,
    };
    set((state) => ({
      sessions: [summary, ...state.sessions],
      activeSessionId: session.id,
      view: "generate",
      sessionDetails: { ...state.sessionDetails, [session.id]: session },
      drafts: { ...state.drafts, [session.id]: { ...EMPTY_DRAFT } },
    }));
    return session.id;
  },

  renameSession: async (id, title) => {
    const { session } = await patchPgSession(id, { title });
    set((state) => ({
      sessions: state.sessions.map((s) =>
        s.id === id ? { ...s, title: session.title, updated_at_ms: session.updated_at_ms } : s,
      ),
      sessionDetails: state.sessionDetails[id]
        ? { ...state.sessionDetails, [id]: session }
        : state.sessionDetails,
    }));
  },

  removeSession: async (id) => {
    await deletePgSession(id, true);
    const remaining = get().sessions.filter((s) => s.id !== id);
    const nextActive =
      get().activeSessionId === id ? (remaining[0]?.id ?? null) : get().activeSessionId;
    set((state) => {
      const details = { ...state.sessionDetails };
      delete details[id];
      const drafts = { ...state.drafts };
      for (const up of drafts[id]?.uploads ?? []) URL.revokeObjectURL(up.previewUrl);
      delete drafts[id];
      return {
        sessions: remaining,
        activeSessionId: nextActive,
        sessionDetails: details,
        drafts,
        confirmDeleteSession: null,
      };
    });
    await get().refreshImages();
    if (nextActive && !get().sessionDetails[nextActive]) {
      await get().selectSession(nextActive);
    }
  },

  updateDraft: (sessionId, updater) =>
    set((state) => ({
      drafts: {
        ...state.drafts,
        [sessionId]: updater(state.drafts[sessionId] ?? { ...EMPTY_DRAFT }),
      },
    })),

  addUploadFiles: (sessionId, files) => {
    const images = files.filter((f) => f.type.startsWith("image/"));
    if (images.length === 0) return;
    const added: DraftUpload[] = images.map((file) => ({
      id: nanoid(),
      file,
      previewUrl: URL.createObjectURL(file),
    }));
    get().updateDraft(sessionId, (prev) => ({
      ...prev,
      uploads: [...prev.uploads, ...added],
    }));
  },

  removeUploadFile: (sessionId, uploadId) =>
    get().updateDraft(sessionId, (prev) => {
      const target = prev.uploads.find((u) => u.id === uploadId);
      if (target) URL.revokeObjectURL(target.previewUrl);
      return { ...prev, uploads: prev.uploads.filter((u) => u.id !== uploadId) };
    }),

  toggleRefImage: (sessionId, imageId) =>
    get().updateDraft(sessionId, (prev) => {
      const exists = prev.refImageIds.includes(imageId);
      return {
        ...prev,
        refImageIds: exists
          ? prev.refImageIds.filter((id) => id !== imageId)
          : [...prev.refImageIds, imageId],
      };
    }),

  removeRefImage: (sessionId, imageId) =>
    get().updateDraft(sessionId, (prev) => ({
      ...prev,
      refImageIds: prev.refImageIds.filter((id) => id !== imageId),
    })),

  editWithImage: async (imageId) => {
    let sid = get().activeSessionId;
    if (!sid) {
      sid = await get().newSession();
    }
    set({ view: "generate", activeSessionId: sid, lightboxImageId: null });
    get().updateDraft(sid, (prev) =>
      prev.refImageIds.includes(imageId)
        ? prev
        : { ...prev, refImageIds: [...prev.refImageIds, imageId] },
    );
  },

  submitGenerate: async (sessionId) => {
    const state = get();
    const draft = state.drafts[sessionId] ?? EMPTY_DRAFT;
    const prompt = draft.prompt.trim();
    const model = state.selectedModel;
    if (!prompt || !model || state.runningBySession[sessionId]) return;

    const files = draft.uploads.map((u) => u.file);
    const refIds = [...draft.refImageIds];
    const params = { ...draft.params };

    // 清空文本与临时上传（保留参数配置；引用图清空避免误带入下轮，收藏浮层随时可点选）。
    for (const u of draft.uploads) URL.revokeObjectURL(u.previewUrl);
    set((prev) => ({
      drafts: {
        ...prev.drafts,
        [sessionId]: { ...draft, prompt: "", uploads: [], refImageIds: [] },
      },
    }));

    await executeGenerateRequest(sessionId, {
      prompt,
      model,
      params,
      sourceImageIds: refIds,
      sourceUploads: [],
      files,
    });
  },

  retryTurn: async (sessionId, prompt, params, inputs, fallbackModel) => {
    const state = get();
    const trimmed = prompt.trim();
    const model = state.selectedModel || fallbackModel || "";
    if (!trimmed || !model || state.runningBySession[sessionId]) return;

    const sourceImageIds = inputs.map((i) => i.image_id).filter((id): id is string => Boolean(id));
    const sourceUploads: UploadRefInput[] = inputs
      .filter((i): i is TurnInputRef & { upload_id: string } => Boolean(!i.image_id && i.upload_id))
      .map((i) => ({ upload_id: i.upload_id, filename: i.filename }));

    await executeGenerateRequest(sessionId, {
      prompt: trimmed,
      model,
      params: { ...params },
      sourceImageIds,
      sourceUploads,
      files: [],
    });
  },

  cancelGenerate: async (sessionId) => {
    await cancelPgGenerate(sessionId);
  },

  applyFavoritePreset: async (preset, targetSessionId) => {
    let sessionId = targetSessionId ?? get().activeSessionId;
    if (!sessionId) {
      sessionId = await get().newSession();
    } else if (!get().sessionDetails[sessionId]) {
      await get().selectSession(sessionId);
    } else {
      set({ activeSessionId: sessionId, view: "generate" });
    }
    const existingImages = get().imagesById;
    const validIds = preset.image_ids.filter((id) => {
      const img = existingImages[id];
      return Boolean(img) && img.format.toLowerCase() !== "avif";
    });
    get().updateDraft(sessionId, (prev) => {
      const nextPrompt = preset.prompt
        ? prev.prompt.trim()
          ? `${prev.prompt.trim()}\n${preset.prompt}`
          : preset.prompt
        : prev.prompt;
      const mergedRefs = [...prev.refImageIds];
      for (const id of validIds) {
        if (!mergedRefs.includes(id)) mergedRefs.push(id);
      }
      return {
        ...prev,
        prompt: nextPrompt,
        refImageIds: mergedRefs,
      };
    });
  },

  toggleFavorite: async (imageId) => {
    const img = get().imagesById[imageId];
    if (!img) return;
    const nextFav = !img.favorite;
    const nextFolderId = nextFav ? img.favorite_folder_id : null;
    // 乐观更新
    set((state) => ({
      imagesById: {
        ...state.imagesById,
        [imageId]: { ...img, favorite: nextFav, favorite_folder_id: nextFolderId },
      },
      favoriteIds: nextFav
        ? [imageId, ...state.favoriteIds.filter((id) => id !== imageId)]
        : state.favoriteIds.filter((id) => id !== imageId),
    }));
    try {
      const resp = await togglePgFavorite(imageId, { favorite: nextFav });
      if (resp.removed) {
        set((state) => {
          const nextImages = { ...state.imagesById };
          delete nextImages[imageId];
          return {
            imagesById: nextImages,
            drafts: stripRefFromDrafts(state.drafts, imageId),
            lightboxImageId: state.lightboxImageId === imageId ? null : state.lightboxImageId,
          };
        });
      }
    } catch (error) {
      // 回滚
      set((state) => ({
        imagesById: { ...state.imagesById, [imageId]: img },
        favoriteIds: img.favorite
          ? [imageId, ...state.favoriteIds.filter((id) => id !== imageId)]
          : state.favoriteIds.filter((id) => id !== imageId),
      }));
      throw error;
    }
  },

  moveFavoriteImageToFolder: async (imageId, folderId) => {
    const img = get().imagesById[imageId];
    if (!img) return;
    const prevImg = img;
    set((state) => ({
      imagesById: {
        ...state.imagesById,
        [imageId]: { ...img, favorite: true, favorite_folder_id: folderId },
      },
      favoriteIds: state.favoriteIds.includes(imageId)
        ? state.favoriteIds
        : [imageId, ...state.favoriteIds],
    }));
    try {
      const resp = await togglePgFavorite(imageId, { favorite: true, folder_id: folderId });
      set((state) => {
        const cur = state.imagesById[imageId];
        if (!cur) return state;
        return {
          imagesById: {
            ...state.imagesById,
            [imageId]: {
              ...cur,
              favorite: resp.favorite,
              favorite_folder_id: resp.favorite_folder_id,
            },
          },
        };
      });
    } catch (error) {
      set((state) => ({
        imagesById: { ...state.imagesById, [imageId]: prevImg },
        favoriteIds: prevImg.favorite
          ? state.favoriteIds.includes(imageId)
            ? state.favoriteIds
            : [imageId, ...state.favoriteIds]
          : state.favoriteIds.filter((id) => id !== imageId),
      }));
      throw error;
    }
  },

  uploadFavorites: async (files, folderId) => {
    const images = files.filter((f) => f.type.startsWith("image/"));
    if (images.length === 0) return [];
    const added = await uploadPgFavorites(images, folderId);
    const addedIds = added.map((i) => i.id);
    set((state) => ({
      imagesById: mergeImages(state.imagesById, added),
      favoriteIds: [...addedIds, ...state.favoriteIds.filter((id) => !addedIds.includes(id))],
    }));
    return added;
  },

  createFavoriteFolder: async (name) => {
    const folder = await createPgFavoriteFolder(name);
    set((state) => ({
      favoriteFolders: [...state.favoriteFolders, folder],
    }));
    return folder;
  },

  renameFavoriteFolder: async (id, name) => {
    const updated = await renamePgFavoriteFolder(id, name);
    set((state) => ({
      favoriteFolders: state.favoriteFolders.map((f) => (f.id === id ? updated : f)),
    }));
  },

  deleteFavoriteFolder: async (id) => {
    await deletePgFavoriteFolder(id);
    set((state) => {
      const nextImages: Record<string, PlaygroundImage> = {};
      for (const [imgId, img] of Object.entries(state.imagesById)) {
        nextImages[imgId] =
          img.favorite_folder_id === id ? { ...img, favorite_folder_id: null } : img;
      }
      return {
        favoriteFolders: state.favoriteFolders.filter((f) => f.id !== id),
        imagesById: nextImages,
        favoritePresets: state.favoritePresets.map((p) =>
          p.folder_id === id ? { ...p, folder_id: null } : p,
        ),
      };
    });
  },

  createFavoritePreset: async (body) => {
    const preset = await createPgFavoritePreset(body);
    if (body.source_uploads && body.source_uploads.length > 0) {
      const meta = await fetchPgFavoritesMeta().catch(() => null);
      set((state) => ({
        imagesById: meta ? mergeImages(state.imagesById, meta.preset_images) : state.imagesById,
        favoritePresets: [preset, ...state.favoritePresets.filter((p) => p.id !== preset.id)],
      }));
      return preset;
    }
    set((state) => ({
      favoritePresets: [preset, ...state.favoritePresets.filter((p) => p.id !== preset.id)],
    }));
    return preset;
  },

  updateFavoritePreset: async (id, patch) => {
    const updated = await patchPgFavoritePreset(id, patch);
    set((state) => ({
      favoritePresets: state.favoritePresets.map((p) => (p.id === id ? updated : p)),
    }));
    return updated;
  },

  deleteFavoritePreset: async (id) => {
    await deletePgFavoritePreset(id);
    set((state) => ({
      favoritePresets: state.favoritePresets.filter((p) => p.id !== id),
    }));
  },

  convertImage: async (imageId, format, quality) => {
    const resp = await convertPgImage(imageId, format, quality);
    set((state) => ({
      imagesById: {
        ...state.imagesById,
        [imageId]: resp.image,
      },
      drafts:
        resp.image.format.toLowerCase() === "avif"
          ? stripRefFromDrafts(state.drafts, imageId)
          : state.drafts,
      pendingConvert: null,
    }));
    return resp;
  },

  removeImage: async (imageId) => {
    await deletePgImage(imageId);
    set((state) => {
      const nextImages = { ...state.imagesById };
      delete nextImages[imageId];
      const nextDetails: Record<string, PlaygroundSession> = {};
      for (const [sid, session] of Object.entries(state.sessionDetails)) {
        nextDetails[sid] = {
          ...session,
          turns: session.turns.map((turn) => ({
            ...turn,
            outputs: turn.outputs.filter((id) => id !== imageId),
          })),
        };
      }
      return {
        imagesById: nextImages,
        galleryIds: state.galleryIds.filter((id) => id !== imageId),
        favoriteIds: state.favoriteIds.filter((id) => id !== imageId),
        favoritePresets: state.favoritePresets.map((p) => ({
          ...p,
          image_ids: p.image_ids.filter((id) => id !== imageId),
        })),
        sessionDetails: nextDetails,
        drafts: stripRefFromDrafts(state.drafts, imageId),
        confirmDeleteImage: null,
        lightboxImageId: state.lightboxImageId === imageId ? null : state.lightboxImageId,
      };
    });
  },
}));
