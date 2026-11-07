/// <reference types="node" />
// 协议清单对账（TS 侧）：`protocol/types.ts` 是手抄的 Rust serde DTO，两侧会
// 静默漂移。本测试读 `moc-protocol` 由 `tests/protocol_manifest.rs` 从 Rust
// 源码生成的 `protocol-manifest.json`，验证：
// 1. 清单里每个 serde 枚举的 wire 值集合与对应 TS union 完全一致；
//    有意不镜像的枚举必须登记在 UNMIRRORED。
// 2. 代码里出现的 `x/y` RPC 方法名都在清单方法表里，且方向正确。
// 3. `PROTOCOL_VERSION` 常量与 Rust 侧一致。
// Rust 侧改了方法/枚举后：先 `MOC_UPDATE_MANIFEST=1 cargo test -p moc-protocol`
// 重生成清单，本测试随即指出 TS 端需要同步的项。

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { PROTOCOL_VERSION } from "./types";

type Manifest = {
  protocolVersion: string;
  methods: { client: string[]; server: string[] };
  enums: Record<string, { tag: string | null; values: (string | number)[] }>;
};

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC = resolve(HERE, "..");
const MANIFEST: Manifest = JSON.parse(
  readFileSync(resolve(HERE, "../../../../moc-protocol/protocol-manifest.json"), "utf8"),
);

/** Rust 枚举名 → TS 类型名不一致时的映射。 */
const NAME_MAP: Record<string, string> = {};

/**
 * 清单里存在但 TS 有意不镜像的 Rust 枚举：登记即声明「已知这条线不抄」；
 * 新 Rust 枚举未镜像也未登记时测试失败。
 */
const UNMIRRORED = new Set<string>([
  // session/history 前端未使用，参数与结果 union 都不镜像：
  "SessionHistoryParams",
  "SessionHistoryResult",
]);

// ---------- 从 types.ts 提取每个 type alias 的 wire 字面量集合 ----------

const stripComments = (text: string) =>
  text.replaceAll(/\/\/[^\n]*/g, "").replaceAll(/\/\*[\s\S]*?\*\//g, "");

/** 切出 `export type Name = <body>`：body 到下一个顶层 `export ` 为止。 */
function aliasBodies(): Map<string, string> {
  const text = stripComments(readFileSync(join(HERE, "types.ts"), "utf8"));
  const bodies = new Map<string, string>();
  const matches = [...text.matchAll(/export type (\w+)\s*=/g)];
  for (const [i, match] of matches.entries()) {
    const end = matches[i + 1]?.index ?? text.length;
    bodies.set(match[1], text.slice(match.index + match[0].length, end));
  }
  return bodies;
}

function typeWireValues(): { bodies: Map<string, string>; values: Map<string, Set<string>> } {
  const bodies = aliasBodies();
  const values = new Map<string, Set<string>>();
  const memo = new Map<string, Set<string>>();

  const collect = (name: string, tag: string | null, seen: Set<string>): Set<string> => {
    const key = `${name}${tag}`;
    if (memo.has(key)) return memo.get(key) ?? new Set();
    const into = new Set<string>();
    memo.set(key, into);
    const body = bodies.get(name);
    if (body === undefined || seen.has(name)) return into;
    seen.add(name);
    if (tag === null) {
      // 纯字面量 union：收全部字符串字面量，再并入引用的兄弟 alias。
      for (const lit of body.matchAll(/"([^"\n]+)"/g)) into.add(lit[1]);
      for (const ref of body.matchAll(/\b([A-Z]\w*)\b/g)) {
        if (bodies.has(ref[1])) for (const v of collect(ref[1], tag, seen)) into.add(v);
      }
    } else {
      // 判别 union：收 `tag: "v"` 字面量；`tag: "a" | "b"` 内联 union 逐个展开。
      const tagUnion = new RegExp(
        `\\b${tag}\\s*:\\s*("(?:[^"\\n]+)"(?:\\s*\\|\\s*"(?:[^"\\n]+)")*)`,
        "g",
      );
      for (const lit of body.matchAll(tagUnion)) {
        for (const value of lit[1].matchAll(/"([^"\n]+)"/g)) into.add(value[1]);
      }
    }
    return into;
  };

  for (const [rustName, entry] of Object.entries(MANIFEST.enums)) {
    const tsName = NAME_MAP[rustName] ?? rustName;
    values.set(tsName, collect(tsName, entry.tag, new Set()));
  }
  return { bodies, values };
}

// ---------- 扫源码里的 RPC 方法字面量 ----------

function tsFiles(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) tsFiles(path, out);
    else if (/\.(ts|tsx)$/.test(entry.name) && !entry.name.includes(".test.")) out.push(path);
  }
  return out;
}

function methodLiterals(): { calls: Set<string>; listens: Set<string> } {
  const calls = new Set<string>();
  const listens = new Set<string>();
  // 只看生产代码：测试桩（rpc.test.ts 的 "a/one" 等）用假方法名，不该对账。
  const CALL = /rpc\.(?:call|notify)(?:<[^>]*>)?\(\s*"([a-z_]+\/[a-z_]+)"/g;
  const LISTEN = /rpc\.(?:on|onRequest)(?:<[^>]*>)?\(\s*"([a-z_]+\/[a-z_]+)"/g;
  for (const file of tsFiles(SRC)) {
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(CALL)) calls.add(match[1]);
    for (const match of text.matchAll(LISTEN)) listens.add(match[1]);
    // RequestMap / NotificationMap 的键也是线上方法名（rpc.ts 只放协议字面量）。
    if (file.endsWith("rpc.ts")) {
      for (const match of text.matchAll(/"([a-z_]+\/[a-z_]+)"\s*:/g)) listens.add(match[1]);
    }
  }
  return { calls, listens };
}

// ---------- 对账 ----------

describe("protocol manifest 对账", () => {
  it("PROTOCOL_VERSION 与 Rust 侧一致", () => {
    expect(PROTOCOL_VERSION).toBe(MANIFEST.protocolVersion);
  });

  it("rpc.call/on 只使用清单里的方法，且方向正确", () => {
    const { calls, listens } = methodLiterals();
    const client = new Set(MANIFEST.methods.client);
    const server = new Set(MANIFEST.methods.server);
    expect([...calls].filter((name) => !client.has(name))).toEqual([]);
    expect([...listens].filter((name) => !server.has(name))).toEqual([]);
  });

  it("清单枚举与 types.ts 的 union 一一对应", () => {
    const { bodies, values: wire } = typeWireValues();
    const missing = new Set<string>();
    const problems: string[] = [];
    for (const [name, entry] of Object.entries(MANIFEST.enums)) {
      const tsName = NAME_MAP[name] ?? name;
      if (!bodies.has(tsName)) {
        if (!UNMIRRORED.has(name)) missing.add(name);
        continue;
      }
      const literals = wire.get(tsName) ?? new Set();
      const expected = new Set(entry.values.map(String));
      const extra = [...literals].filter((v) => !expected.has(v));
      const lacking = [...expected].filter((v) => !literals.has(v));
      if (extra.length || lacking.length) {
        problems.push(
          `${name}（TS ${tsName}）：TS 多出 [${extra.join(", ")}]，Rust 多出 [${lacking.join(", ")}]`,
        );
      }
    }
    // UNMIRRORED 里登记了实际已镜像的名字时提示精简。
    const stale = [...UNMIRRORED].filter((name) => bodies.has(NAME_MAP[name] ?? name));
    expect(problems).toEqual([]);
    expect([...missing].sort()).toEqual([]);
    expect(stale).toEqual([]);
  });
});
