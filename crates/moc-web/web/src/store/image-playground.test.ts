import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  FavoritePreset,
  PlaygroundImage,
  PlaygroundSession,
  PlaygroundSessionSummary,
} from "@/lib/image-playground-api";

const api = vi.hoisted(() => ({
  fetchImageModels: vi.fn(),
  fetchPlaygroundConfig: vi.fn(),
  listPgSessions: vi.fn(),
  listPgImages: vi.fn(),
  fetchPgFavoritesMeta: vi.fn(),
  createPgSession: vi.fn(),
  getPgSession: vi.fn(),
  generatePgImages: vi.fn(),
  cancelPgGenerate: vi.fn(),
}));

vi.mock("@/lib/image-playground-api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/image-playground-api")>();
  return {
    ...actual,
    ...api,
  };
});

import { PLAYGROUND_DRAFT_ID, useImagePlayground } from "./image-playground";

const initialState = useImagePlayground.getState();

function resetStore() {
  useImagePlayground.setState(
    {
      ...initialState,
      view: "generate",
      models: [],
      selectedModel: "",
      sessions: [],
      activeSessionId: null,
      sessionDetails: {},
      imagesById: {},
      galleryIds: [],
      favoriteIds: [],
      favoriteFolders: [],
      favoritePresets: [],
      runningBySession: {},
      drafts: {},
      initialized: false,
    },
    true,
  );
}

afterEach(() => {
  vi.clearAllMocks();
  resetStore();
});

describe("useImagePlayground 草稿与延迟创建会话", () => {
  it("bootstrap 默认停留在新建会话草稿态，不自动选中旧会话也不创建会话", async () => {
    const oldSession: PlaygroundSessionSummary = {
      id: "pg-old",
      title: "旧会话",
      created_at_ms: 100,
      updated_at_ms: 200,
      model: "openai/gpt-image-1",
      turn_count: 1,
      image_count: 1,
    };
    api.fetchImageModels.mockResolvedValue([
      { id: "openai/gpt-image-1", provider: "openai", name: "gpt-image-1" },
    ]);
    api.fetchPlaygroundConfig.mockResolvedValue(null);
    api.listPgSessions.mockResolvedValue([oldSession]);
    api.listPgImages.mockResolvedValue([]);
    api.fetchPgFavoritesMeta.mockResolvedValue({ folders: [], presets: [], preset_images: [] });

    await useImagePlayground.getState().bootstrap();

    const state = useImagePlayground.getState();
    expect(state.view).toBe("generate");
    expect(state.activeSessionId).toBeNull();
    expect(state.sessions).toEqual([oldSession]);
    expect(api.getPgSession).not.toHaveBeenCalled();
    expect(api.createPgSession).not.toHaveBeenCalled();
  });

  it("newSession、editWithImage 与 applyFavoritePreset 均写入本地草稿而不提前建会话", async () => {
    const img: PlaygroundImage = {
      id: "img-1",
      origin: "generated",
      session_id: "pg-old",
      turn_id: "t-1",
      created_at_ms: 1,
      model: "openai/gpt-image-1",
      prompt: "cat",
      revised_prompt: null,
      width: 512,
      height: 512,
      format: "png",
      extension: "png",
      mime_type: "image/png",
      is_animated: false,
      size_bytes: 1024,
      original_size_bytes: 1024,
      original_format: "png",
      file_path: "/tmp/cat.png",
      filename: "cat.png",
      favorite: true,
      favorite_folder_id: null,
    };
    const preset: FavoritePreset = {
      id: "preset-1",
      title: "赛博猫",
      prompt: "cyberpunk cat",
      image_ids: [img.id],
      folder_id: null,
      created_at_ms: 1,
      updated_at_ms: 1,
    };
    useImagePlayground.setState({
      view: "gallery",
      activeSessionId: "pg-old",
      imagesById: { [img.id]: img },
    });

    useImagePlayground.getState().newSession();
    expect(useImagePlayground.getState().activeSessionId).toBeNull();
    expect(useImagePlayground.getState().view).toBe("generate");

    useImagePlayground.setState({ view: "gallery" });
    useImagePlayground.getState().editWithImage(img.id);
    expect(useImagePlayground.getState().activeSessionId).toBeNull();
    expect(useImagePlayground.getState().drafts[PLAYGROUND_DRAFT_ID]?.refImageIds).toEqual([
      img.id,
    ]);

    await useImagePlayground.getState().applyFavoritePreset(preset);
    expect(useImagePlayground.getState().activeSessionId).toBeNull();
    expect(useImagePlayground.getState().drafts[PLAYGROUND_DRAFT_ID]?.prompt).toBe("cyberpunk cat");
    expect(useImagePlayground.getState().sessions).toEqual([]);
    expect(api.createPgSession).not.toHaveBeenCalled();
  });

  it("草稿态仅在发送生成时创建真实会话并加入左侧列表，建会话失败时保留草稿", async () => {
    useImagePlayground.setState({
      selectedModel: "openai/gpt-image-1",
      drafts: {
        [PLAYGROUND_DRAFT_ID]: {
          prompt: "一只在雨夜霓虹下的橘猫",
          uploads: [],
          refImageIds: ["ref-1"],
          params: { size: "1024x1024" },
        },
      },
    });

    // 创建会话失败时不清空草稿、不污染列表
    api.createPgSession.mockRejectedValueOnce(new Error("network down"));
    await expect(useImagePlayground.getState().submitGenerate(PLAYGROUND_DRAFT_ID)).rejects.toThrow(
      "network down",
    );
    expect(useImagePlayground.getState().sessions).toEqual([]);
    expect(useImagePlayground.getState().activeSessionId).toBeNull();
    expect(useImagePlayground.getState().runningBySession[PLAYGROUND_DRAFT_ID]).toBe(false);
    expect(useImagePlayground.getState().drafts[PLAYGROUND_DRAFT_ID]?.prompt).toBe(
      "一只在雨夜霓虹下的橘猫",
    );

    // 成功发送：创建真实会话、自动加入 sessions 列表并切换到该会话执行生成
    const created: PlaygroundSession = {
      id: "pg-new",
      title: "一只在雨夜霓虹下的橘猫",
      created_at_ms: 1000,
      updated_at_ms: 1000,
      model: "openai/gpt-image-1",
      params: { size: "1024x1024" },
      turns: [],
    };
    api.createPgSession.mockResolvedValueOnce({ session: created, images: {} });
    api.generatePgImages.mockResolvedValueOnce({
      session_title: created.title,
      turn: {
        id: "turn-1",
        created_at_ms: 1200,
        model: "openai/gpt-image-1",
        mode: "edit",
        prompt: "一只在雨夜霓虹下的橘猫",
        params: { size: "1024x1024" },
        inputs: [],
        outputs: [],
        revised_prompt: null,
        duration_ms: 200,
        error: null,
      },
      images: [],
    });

    await useImagePlayground.getState().submitGenerate(PLAYGROUND_DRAFT_ID);

    const state = useImagePlayground.getState();
    expect(api.createPgSession).toHaveBeenLastCalledWith({
      title: "一只在雨夜霓虹下的橘猫",
      model: "openai/gpt-image-1",
      params: { size: "1024x1024" },
    });
    expect(api.generatePgImages).toHaveBeenCalledWith(
      "pg-new",
      expect.objectContaining({
        prompt: "一只在雨夜霓虹下的橘猫",
        model: "openai/gpt-image-1",
        sourceImageIds: ["ref-1"],
      }),
    );
    expect(state.activeSessionId).toBe("pg-new");
    expect(state.sessions.map((s) => s.id)).toEqual(["pg-new"]);
    expect(state.sessionDetails["pg-new"]?.turns).toHaveLength(1);
    expect(state.drafts[PLAYGROUND_DRAFT_ID]?.prompt).toBe("");
    expect(state.drafts["pg-new"]?.params).toEqual({ size: "1024x1024" });
  });
});
