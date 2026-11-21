// Image Playground HTTP API 类型定义与请求封装（令牌经 ?token= 传递）。

import { accessToken } from "@/protocol/client";

export type EditEncoding = "multipart" | "multipart_repeat_image" | "json";

export type OutputCompressionFormat =
  | "original"
  | "webp"
  | "webp_lossless"
  | "avif"
  | "jpeg"
  | "png"
  | "apng";

export type AnimatedOutputFormat = "original" | "webp_animated" | "apng";

export type PlaygroundConfig = {
  storage_dir: string;
  compression: {
    format: OutputCompressionFormat;
    quality: number;
    animated_format: AnimatedOutputFormat;
    animated_quality: number;
  };
  input_compress: {
    enabled: boolean;
    max_dimension: number;
    max_bytes_mb: number;
  };
  default_edit_encoding: EditEncoding;
};

export type PlaygroundConfigResponse = {
  config: PlaygroundConfig;
  resolved_storage_dir: string;
};

export type ImageModelRow = {
  id: string;
  provider: string;
  name: string;
};

export type ImageGenParams = {
  n?: number;
  size?: string;
  aspect_ratio?: string;
  image_size?: string;
  quality?: string;
  background?: string;
  output_format?: string;
  output_compression?: number;
  response_format?: string;
  input_fidelity?: string;
  style?: string;
  moderation?: string;
  edit_encoding?: EditEncoding;
  extra_params?: Record<string, unknown>;
};

export type TurnInputRef = {
  image_id: string | null;
  upload_id: string | null;
  filename: string;
  mime_type: string;
};

export type TurnError = {
  status: number | null;
  message: string;
  raw_body: string | null;
};

export type PlaygroundTurn = {
  id: string;
  created_at_ms: number;
  model: string;
  mode: "generate" | "edit" | string;
  prompt: string;
  params: ImageGenParams;
  inputs: TurnInputRef[];
  outputs: string[];
  revised_prompt: string | null;
  duration_ms: number;
  error: TurnError | null;
};

export type PlaygroundSessionSummary = {
  id: string;
  title: string;
  created_at_ms: number;
  updated_at_ms: number;
  model: string | null;
  turn_count: number;
  image_count: number;
};

export type PlaygroundSession = {
  id: string;
  title: string;
  created_at_ms: number;
  updated_at_ms: number;
  model: string | null;
  params: ImageGenParams;
  turns: PlaygroundTurn[];
};

export type PlaygroundImage = {
  id: string;
  origin: "generated" | "uploaded_favorite";
  session_id: string | null;
  turn_id: string | null;
  created_at_ms: number;
  model: string | null;
  prompt: string | null;
  revised_prompt: string | null;
  width: number;
  height: number;
  format: string;
  extension: string;
  mime_type: string;
  is_animated: boolean;
  size_bytes: number;
  original_size_bytes: number;
  original_format: string;
  file_path: string;
  filename: string;
  favorite: boolean;
  favorite_folder_id: string | null;
};

export type FavoriteFolder = {
  id: string;
  name: string;
  created_at_ms: number;
  updated_at_ms: number;
};

export type FavoritePreset = {
  id: string;
  title: string;
  prompt: string;
  image_ids: string[];
  folder_id: string | null;
  created_at_ms: number;
  updated_at_ms: number;
};

export type FavoritesMetaResponse = {
  folders: FavoriteFolder[];
  presets: FavoritePreset[];
  preset_images: PlaygroundImage[];
};

export type ConvertFormat =
  | "webp"
  | "webp_lossless"
  | "avif"
  | "jpeg"
  | "png"
  | "webp_animated"
  | "apng";

export type ConvertImageResponse = {
  image: PlaygroundImage;
  previous_size_bytes: number;
  previous_format: string;
};

export type SessionDetailResponse = {
  session: PlaygroundSession;
  images: Record<string, PlaygroundImage>;
};

export type GenerateResponse = {
  turn: PlaygroundTurn;
  images: PlaygroundImage[];
  session_title: string;
};

function pgUrl(path: string, params?: Record<string, string>): string {
  const query = new URLSearchParams({ token: accessToken ?? "", ...params });
  return `/image-playground/api${path}?${query.toString()}`;
}

export function pgImageUrl(id: string, download = false, cacheKey?: string): string {
  const params: Record<string, string> = {};
  if (download) params.download = "1";
  if (cacheKey) params.v = cacheKey;
  return pgUrl(
    `/images/${encodeURIComponent(id)}/file`,
    Object.keys(params).length > 0 ? params : undefined,
  );
}

export function pgUploadUrl(uploadId: string): string {
  return pgUrl(`/uploads/${encodeURIComponent(uploadId)}`);
}

async function pgFetch<T>(
  path: string,
  init?: RequestInit,
  params?: Record<string, string>,
): Promise<T> {
  const response = await fetch(pgUrl(path, params), init);
  if (!response.ok) {
    let message = `HTTP ${response.status}`;
    try {
      const data = (await response.json()) as { error?: string };
      if (data.error) message = data.error;
    } catch {
      const text = await response.text().catch(() => "");
      if (text) message = text;
    }
    throw new Error(message);
  }
  return (await response.json()) as T;
}

export const fetchImageModels = () =>
  pgFetch<{ models: ImageModelRow[] }>("/models").then((r) => r.models);

export const fetchPlaygroundConfig = () => pgFetch<PlaygroundConfigResponse>("/config");

export const savePlaygroundConfig = (config: PlaygroundConfig) =>
  pgFetch<PlaygroundConfigResponse>("/config", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(config),
  });

export const openStorageDir = () =>
  pgFetch<{ ok: boolean }>("/config/open-dir", { method: "POST" });

export const listPgSessions = () =>
  pgFetch<{ sessions: PlaygroundSessionSummary[] }>("/sessions").then((r) => r.sessions);

export const createPgSession = (body?: {
  title?: string;
  model?: string;
  params?: ImageGenParams;
}) =>
  pgFetch<SessionDetailResponse>("/sessions", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body ?? {}),
  });

export const getPgSession = (id: string) =>
  pgFetch<SessionDetailResponse>(`/sessions/${encodeURIComponent(id)}`);

export const patchPgSession = (
  id: string,
  patch: { title?: string; model?: string; params?: ImageGenParams },
) =>
  pgFetch<{ session: PlaygroundSession }>(`/sessions/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });

export const deletePgSession = (id: string, deleteUnfavorited = true) =>
  pgFetch<{ ok: boolean }>(
    `/sessions/${encodeURIComponent(id)}`,
    { method: "DELETE" },
    { delete_unfavorited: deleteUnfavorited ? "true" : "false" },
  );

export const generatePgImages = (
  sessionId: string,
  payload: {
    prompt: string;
    model: string;
    params: ImageGenParams;
    sourceImageIds: string[];
    files: File[];
  },
) => {
  const form = new FormData();
  form.append("prompt", payload.prompt);
  form.append("model", payload.model);
  form.append("params", JSON.stringify(payload.params));
  if (payload.sourceImageIds.length > 0) {
    form.append("source_image_ids", JSON.stringify(payload.sourceImageIds));
  }
  for (const file of payload.files) {
    form.append("images", file, file.name || "image.png");
  }
  return pgFetch<GenerateResponse>(`/sessions/${encodeURIComponent(sessionId)}/generate`, {
    method: "POST",
    body: form,
  });
};

export const cancelPgGenerate = (sessionId: string) =>
  pgFetch<{ ok: boolean }>(`/sessions/${encodeURIComponent(sessionId)}/cancel`, {
    method: "POST",
  });

export const listPgImages = (scope: "gallery" | "favorites") =>
  pgFetch<{ images: PlaygroundImage[] }>("/images", undefined, { scope }).then((r) => r.images);

export const fetchPgFavoritesMeta = () => pgFetch<FavoritesMetaResponse>("/favorites");

export const uploadPgFavorites = (files: File[], folderId?: string | null) => {
  const form = new FormData();
  if (folderId) {
    form.append("folder_id", folderId);
  }
  for (const file of files) {
    form.append("files", file, file.name || "reference.png");
  }
  return pgFetch<{ images: PlaygroundImage[] }>("/favorites/upload", {
    method: "POST",
    body: form,
  }).then((r) => r.images);
};

export const togglePgFavorite = (
  id: string,
  patch: { favorite?: boolean; folder_id?: string | null },
) =>
  pgFetch<{ ok: boolean; favorite: boolean; favorite_folder_id: string | null }>(
    `/images/${encodeURIComponent(id)}/favorite`,
    {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    },
  );

export const createPgFavoriteFolder = (name: string) =>
  pgFetch<FavoriteFolder>("/favorites/folders", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });

export const renamePgFavoriteFolder = (id: string, name: string) =>
  pgFetch<FavoriteFolder>(`/favorites/folders/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });

export const deletePgFavoriteFolder = (id: string) =>
  pgFetch<{ ok: boolean }>(`/favorites/folders/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });

export const createPgFavoritePreset = (body: {
  title?: string;
  prompt: string;
  image_ids: string[];
  folder_id: string | null;
}) =>
  pgFetch<FavoritePreset>("/favorites/presets", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

export const patchPgFavoritePreset = (
  id: string,
  patch: {
    title?: string;
    prompt?: string;
    image_ids?: string[];
    folder_id?: string | null;
  },
) =>
  pgFetch<FavoritePreset>(`/favorites/presets/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });

export const deletePgFavoritePreset = (id: string) =>
  pgFetch<{ ok: boolean }>(`/favorites/presets/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });

export const deletePgImage = (id: string) =>
  pgFetch<{ ok: boolean }>(`/images/${encodeURIComponent(id)}`, { method: "DELETE" });

export const convertPgImage = (id: string, format: ConvertFormat, quality?: number) =>
  pgFetch<ConvertImageResponse>(`/images/${encodeURIComponent(id)}/convert`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(quality !== undefined ? { format, quality } : { format }),
  });

export const revealPgImage = (id: string) =>
  pgFetch<{ ok: boolean }>(`/images/${encodeURIComponent(id)}/reveal`, { method: "POST" });
