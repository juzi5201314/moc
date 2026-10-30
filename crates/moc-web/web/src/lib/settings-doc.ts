// 设置文档的纯函数：按 path 段读写 document、字段文本 ↔ 值的转换。
// 解析规则对齐 moc-tui `settings/forms.rs` 的 `parse`（数值支持 k/m 与千分位）。

import type { FieldKind } from "@/protocol/types";

type Json = Record<string, unknown>;

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function getPath(root: unknown, path: string[]): unknown {
  let current: unknown = root;
  for (const segment of path) {
    if (!isObject(current)) return undefined;
    current = current[segment];
  }
  return current;
}

/** 返回新对象：`value === undefined` 时删除该键，并清掉因此变空的父对象。 */
export function setPath(root: Json, path: string[], value: unknown): Json {
  const [head, ...rest] = path;
  if (head === undefined) return root;
  const next: Json = { ...root };
  if (rest.length === 0) {
    if (value === undefined) delete next[head];
    else next[head] = value;
    return next;
  }
  const child = isObject(next[head]) ? (next[head] as Json) : {};
  const updated = setPath(child, rest, value);
  if (Object.keys(updated).length === 0 && value === undefined) delete next[head];
  else next[head] = updated;
  return next;
}

/** 值 → 输入框文本。 */
export function formatField(value: unknown, kind: FieldKind): string {
  if (value === undefined || value === null) return "";
  // 数组形态的 choice（模型 `input`）按选项值的 "a, b" 写法展示。
  if (kind.type === "choice" && Array.isArray(value)) return value.join(", ");
  if (kind.type === "strings") {
    return Array.isArray(value) ? value.filter((item) => typeof item === "string").join(", ") : "";
  }
  if (kind.type === "object") return JSON.stringify(value, null, 2);
  return typeof value === "string" ? value : JSON.stringify(value);
}

export type ParseResult = { ok: true; value: unknown } | { ok: false; error: string };

function parseScaled(text: string): { base: string; multiplier: number } {
  const plain = text.replaceAll(",", "").trim();
  const last = plain.at(-1)?.toLowerCase();
  if (last === "k") return { base: plain.slice(0, -1).trim(), multiplier: 1_000 };
  if (last === "m") return { base: plain.slice(0, -1).trim(), multiplier: 1_000_000 };
  return { base: plain, multiplier: 1 };
}

/** 输入框文本 → 值；空文本返回 `undefined`（删除该键，回落到继承值）。 */
export function parseField(text: string, kind: FieldKind): ParseResult {
  const trimmed = text.trim();
  if (!trimmed) return { ok: true, value: undefined };
  switch (kind.type) {
    case "bool":
      return { ok: true, value: trimmed === "true" };
    case "integer": {
      const { base, multiplier } = parseScaled(trimmed);
      if (!/^\d+$/.test(base))
        return { ok: false, error: "请输入有效整数（支持 200k、1m 或千分位）" };
      const value = Number(base) * multiplier;
      if (!Number.isSafeInteger(value)) return { ok: false, error: "数值超出支持范围" };
      return { ok: true, value };
    }
    case "number": {
      const { base, multiplier } = parseScaled(trimmed);
      const value = Number(base) * multiplier;
      if (!base || !Number.isFinite(value) || value < 0) {
        return { ok: false, error: "请输入有限的非负数" };
      }
      return { ok: true, value };
    }
    case "strings":
      return {
        ok: true,
        value: trimmed
          .split(",")
          .map((item) => item.trim())
          .filter(Boolean),
      };
    case "object":
      try {
        const value: unknown = JSON.parse(trimmed);
        if (!isObject(value) && typeof value !== "string") {
          return { ok: false, error: "需要 JSON 对象" };
        }
        return { ok: true, value };
      } catch (error) {
        return { ok: false, error: `JSON 格式有误：${String(error)}` };
      }
    default:
      return { ok: true, value: text };
  }
}

/** Choice 选项展示：标签与取值相同时只显示一次（同 `ChoiceOption::display`）。 */
export function choiceDisplay(option: { value: string; label: string }): string {
  return option.label === option.value ? option.value : `${option.label}（${option.value}）`;
}

/**
 * 模型 `input` 在 catalog 里是 choice（`text` / `text, image`），文档里却存成字符串数组；
 * 与 core `catalog.rs` 的特判保持一致。
 */
export function choiceValue(path: string[], option: string): unknown {
  const modelInput = path.at(-1) === "input" && path.includes("models");
  return modelInput ? option.split(",").map((item) => item.trim()) : option;
}
