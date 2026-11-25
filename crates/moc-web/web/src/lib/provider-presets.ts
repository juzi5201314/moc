// 内置供应商预设、模型能力推断与字段校验工具（与 moc-core/src/config/presets.rs 保持一致）。

import type { CatalogModel } from "@/protocol/types";

export type ProviderPreset = {
  key: string;
  label: string;
  id: string;
  api: string;
  baseUrl: string;
  apiKeyHint: string;
  azureApiVersion?: string;
};

export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    key: "openai",
    label: "OpenAI",
    id: "openai",
    api: "openai_responses",
    baseUrl: "https://api.openai.com/v1",
    apiKeyHint: "env:OPENAI_API_KEY",
  },
  {
    key: "anthropic",
    label: "Anthropic",
    id: "anthropic",
    api: "anthropic_messages",
    baseUrl: "https://api.anthropic.com",
    apiKeyHint: "env:ANTHROPIC_API_KEY",
  },
  {
    key: "deepseek",
    label: "DeepSeek",
    id: "deepseek",
    api: "openai_chat",
    baseUrl: "https://api.deepseek.com/v1",
    apiKeyHint: "env:DEEPSEEK_API_KEY",
  },
  {
    key: "openrouter",
    label: "OpenRouter",
    id: "openrouter",
    api: "openai_chat",
    baseUrl: "https://openrouter.ai/api/v1",
    apiKeyHint: "env:OPENROUTER_API_KEY",
  },
  {
    key: "gemini",
    label: "Google Gemini",
    id: "gemini",
    api: "openai_chat",
    baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
    apiKeyHint: "env:GEMINI_API_KEY",
  },
  {
    key: "xai",
    label: "xAI (Grok)",
    id: "xai",
    api: "openai_responses",
    baseUrl: "https://api.x.ai/v1",
    apiKeyHint: "env:XAI_API_KEY",
  },
  {
    key: "ollama",
    label: "Ollama (Local)",
    id: "ollama",
    api: "openai_chat",
    baseUrl: "http://127.0.0.1:11434/v1",
    apiKeyHint: "",
  },
  {
    key: "azure",
    label: "Azure OpenAI",
    id: "azure",
    api: "openai_responses",
    baseUrl: "https://YOUR-RESOURCE.openai.azure.com",
    apiKeyHint: "env:AZURE_OPENAI_API_KEY",
    azureApiVersion: "2025-04-01-preview",
  },
  {
    key: "custom",
    label: "Custom",
    id: "",
    api: "openai_chat",
    baseUrl: "",
    apiKeyHint: "",
  },
];

export const BUILTIN_THINKING_LEVELS = ["low", "medium", "high", "xhigh", "max"] as const;

export const EFFORT_MAP_KEYS = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
] as const;

export function isValidBaseUrl(url: string): boolean {
  const trimmed = url.trim();
  return trimmed.startsWith("https://") || trimmed.startsWith("http://");
}

/** 识别非明文凭据引用（`env:VAR` 或 `!command`），界面可明文回显方便核对。 */
export function secretRefKind(value: string): "env" | "cmd" | null {
  const trimmed = value.trim();
  if (trimmed.startsWith("env:")) return "env";
  if (trimmed.startsWith("!")) return "cmd";
  return null;
}

export function isValidCustomThinkingLevel(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed || trimmed.length > 32 || trimmed === "off") return false;
  return /^[a-z0-9._-]+$/.test(trimmed);
}

/** 构造由发现或推断得到的模型初始配置（只写有别于默认模板的字段）。 */
export function modelEntryFromCatalog(model: CatalogModel): Record<string, unknown> {
  const entry: Record<string, unknown> = {};
  if (model.image_gen) {
    entry.image_gen = true;
    return entry;
  }
  if (model.context_window && model.context_window !== 200_000) {
    entry.context_window = model.context_window;
  }
  if (model.max_output_tokens && model.max_output_tokens !== 32_000) {
    entry.max_output_tokens = model.max_output_tokens;
  }
  if (model.reasoning) {
    entry.reasoning = true;
  }
  if (model.input?.includes("image")) {
    entry.input = ["text", "image"];
  }
  if (model.fast_mode) {
    entry.fast_mode = true;
  }
  if (model.pricing) {
    const p: Record<string, number> = {
      input: model.pricing.input,
      output: model.pricing.output,
    };
    if (model.pricing.cache_read !== undefined && model.pricing.cache_read !== null) {
      p.cache_read = model.pricing.cache_read;
    }
    if (model.pricing.cache_write !== undefined && model.pricing.cache_write !== null) {
      p.cache_write = model.pricing.cache_write;
    }
    entry.pricing = p;
  }
  return entry;
}

function bareModelName(modelId: string): string {
  const trimmed = modelId.trim().toLowerCase();
  const tail = trimmed.split("/").at(-1) ?? trimmed;
  return tail.split(":")[0] ?? tail;
}

/** 手动输入模型 ID 时按常见模型家族推断默认字段，未知 ID 返回 `{}`。 */
export function inferModelEntry(modelId: string): Record<string, unknown> {
  const name = bareModelName(modelId);
  if (!name) return {};
  if (
    name.startsWith("gpt-image-") ||
    name.startsWith("dall-e-") ||
    name.startsWith("imagen-") ||
    name.startsWith("flux-") ||
    name.includes("image-generation")
  ) {
    return { image_gen: true };
  }
  if (name.startsWith("gpt-5")) {
    return {
      context_window: 400_000,
      max_output_tokens: 128_000,
      reasoning: true,
      input: ["text", "image"],
    };
  }
  if (name.startsWith("gpt-4.1")) {
    return { context_window: 1_000_000, max_output_tokens: 32_768, input: ["text", "image"] };
  }
  if (name.startsWith("gpt-4o")) {
    return { context_window: 128_000, max_output_tokens: 16_384, input: ["text", "image"] };
  }
  if (/^o[134]($|-)/.test(name)) {
    return { max_output_tokens: 100_000, reasoning: true, input: ["text", "image"] };
  }
  if (name.startsWith("claude-")) {
    const legacy = name.includes("claude-3-5") || name.includes("claude-3.5");
    const haiku = name.includes("haiku");
    const out: Record<string, unknown> = { input: ["text", "image"] };
    if (legacy) out.max_output_tokens = 8_192;
    else {
      out.reasoning = true;
      if (!haiku) out.max_output_tokens = 64_000;
    }
    return out;
  }
  if (name.startsWith("deepseek-reasoner") || name.startsWith("deepseek-r1")) {
    return { context_window: 128_000, max_output_tokens: 64_000, reasoning: true };
  }
  if (name.startsWith("deepseek-chat") || name.startsWith("deepseek-v3")) {
    return { context_window: 128_000, max_output_tokens: 8_192 };
  }
  if (name.startsWith("gemini-2.5") || name.startsWith("gemini-3")) {
    return {
      context_window: 1_048_576,
      max_output_tokens: 65_536,
      reasoning: true,
      input: ["text", "image"],
    };
  }
  if (name.startsWith("gemini-2.0") || name.startsWith("gemini-1.5")) {
    return { context_window: 1_048_576, max_output_tokens: 8_192, input: ["text", "image"] };
  }
  if (name.startsWith("grok-4") || name.startsWith("grok-3")) {
    const entry: Record<string, unknown> = {
      context_window: name.startsWith("grok-4") ? 256_000 : 131_072,
      max_output_tokens: 32_768,
      input: ["text", "image"],
    };
    if (name.startsWith("grok-4") || name.includes("mini")) entry.reasoning = true;
    return entry;
  }
  if (name.startsWith("qwen3") || name.startsWith("qwq")) {
    const coder = name.includes("coder");
    const vision = name.includes("vl") || name.includes("omni");
    const entry: Record<string, unknown> = {
      context_window: coder ? 262_144 : 131_072,
      max_output_tokens: 32_768,
    };
    if (!coder || name.includes("thinking")) entry.reasoning = true;
    if (vision) entry.input = ["text", "image"];
    return entry;
  }
  return {};
}

/** 根据建议名生成不重名的副本 ID（如 `openai-copy`、`openai-copy-2`）。 */
export function nextCopyId(base: string, existing: Iterable<string>): string {
  const taken = new Set(existing);
  const first = `${base}-copy`;
  if (!taken.has(first)) return first;
  for (let i = 2; i < 1000; i++) {
    const candidate = `${first}-${i}`;
    if (!taken.has(candidate)) return candidate;
  }
  return `${first}-${Date.now()}`;
}
