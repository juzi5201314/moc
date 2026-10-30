// 模型 selector 的纯函数：`provider/model[:level]` ↔ 模型条目与思考等级。

import type { ModelInfo } from "@/protocol/types";

export type ParsedSelector = { model?: ModelInfo; id: string; level?: string };

/**
 * 拆出模型与思考等级。模型 ID 本身可能含 `:`，所以先按已知模型做最长前缀匹配，
 * 匹配不到才退回「最后一个冒号」拆分。
 */
export function parseSelector(selector: string, models: ModelInfo[]): ParsedSelector {
  let best: ModelInfo | undefined;
  for (const model of models) {
    const matches = selector === model.id || selector.startsWith(`${model.id}:`);
    if (matches && (!best || model.id.length > best.id.length)) best = model;
  }
  if (best) {
    const rest = selector.slice(best.id.length + 1);
    return { model: best, id: best.id, level: rest || undefined };
  }
  const index = selector.lastIndexOf(":");
  return index > 0
    ? { id: selector.slice(0, index), level: selector.slice(index + 1) }
    : { id: selector };
}

/** `fusion` 别名：展开为 `[models.fusion]` 配置的组合。 */
export const FUSION_ALIAS = "fusion";

/** 规范 Fusion selector（`fusion:<lead>+<sidekick>`）的两侧；非法形式返回 null。 */
export function parseFusion(selector: string): { lead: string; sidekick: string } | null {
  if (!selector.startsWith("fusion:")) return null;
  const rest = selector.slice("fusion:".length);
  const index = rest.indexOf("+");
  if (index <= 0 || index === rest.length - 1) return null;
  return { lead: rest.slice(0, index), sidekick: rest.slice(index + 1) };
}

/** 把两侧 selector 拼成规范 Fusion selector。 */
export function fusionSelector(lead: string, sidekick: string): string {
  return `fusion:${lead}+${sidekick}`;
}

/** 真实发请求的模型 selector：Fusion 取 lead 侧，普通 selector 原样返回。 */
export function leadSelector(selector: string): string {
  return parseFusion(selector)?.lead ?? selector;
}

export function providerOf(id: string): string {
  const index = id.indexOf("/");
  return index > 0 ? id.slice(0, index) : "";
}

export function modelName(id: string): string {
  const index = id.indexOf("/");
  return index > 0 ? id.slice(index + 1) : id;
}

/** 按供应商分组，组内保持 core 给出的顺序。 */
export function groupByProvider(models: ModelInfo[]): [string, ModelInfo[]][] {
  const groups = new Map<string, ModelInfo[]>();
  for (const model of models) {
    const provider = providerOf(model.id) || "其他";
    const list = groups.get(provider);
    if (list) list.push(model);
    else groups.set(provider, [model]);
  }
  return [...groups.entries()];
}

/** 模型可选的非 off 思考等级；不支持思考时为空。 */
export function reasoningLevels(model: ModelInfo | undefined): string[] {
  return (model?.thinking_levels ?? []).filter((level) => level !== "off");
}

export const LEVEL_LABEL: Record<string, string> = {
  off: "关闭",
  minimal: "极简",
  low: "低",
  medium: "中",
  high: "高",
  xhigh: "超高",
  max: "最大",
};

export function levelLabel(level: string): string {
  return LEVEL_LABEL[level] ?? level;
}

export function compactTokens(value: number | undefined): string {
  if (!value) return "";
  if (value >= 1_000_000) return `${Number((value / 1_000_000).toFixed(1))}M`;
  return `${Math.round(value / 1000)}K`;
}
