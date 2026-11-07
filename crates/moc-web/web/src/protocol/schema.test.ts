/// <reference types="node" />
// 协议 schema 对账（TS 侧）：`protocol/types.gen.ts` 由
// `moc-protocol/protocol.schema.json` 经 `pnpm gen:protocol` 生成，
// 手写区只有 types.ts 里的版本、常量与 helper。本测试验证：
// 1. `PROTOCOL_VERSION` 常量与 schema 的 protocolVersion 一致；
// 2. 代码里出现的 `x/y` RPC 方法名都在 schema 方法索引里，且方向正确；
//    rpc.ts 的 NotificationMap/RequestMap 键与 server 侧索引一一对应；
// 3. `types.gen.ts` 的 banner 指纹与 schema 文件一致——Rust 侧改了 DTO
//    却没重跑 `pnpm gen:protocol` 时这里会失败。
// 枚举 union 的一致性由 codegen 本身保证，不再需要对账；手写区里两个
// `Record<生成 union, true>` 类型表（SESSION_UPDATE_TYPES / BTW_UPDATE_TYPES）
// 漏项会直接编译失败。

import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { PROTOCOL_VERSION } from "./types";

type Schema = {
  protocolVersion: string;
  methods: {
    client: Record<string, unknown>;
    server_request: Record<string, unknown>;
    server_notification: Record<string, unknown>;
  };
};

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC = resolve(HERE, "..");
const SCHEMA_PATH = resolve(HERE, "../../../../moc-protocol/protocol.schema.json");
const SCHEMA_TEXT = readFileSync(SCHEMA_PATH, "utf8");
const SCHEMA: Schema = JSON.parse(SCHEMA_TEXT);

// ---------- 扫源码里的 RPC 方法字面量 ----------

function tsFiles(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) tsFiles(path, out);
    else if (/\.(ts|tsx)$/.test(entry.name) && !entry.name.includes(".test.")) out.push(path);
  }
  return out;
}

/** 切出 rpc.ts 里 `type X = { "a/b": ... }` 的键集合。 */
function mapKeys(text: string, name: string): Set<string> {
  const start = text.indexOf(`type ${name} = {`);
  if (start < 0) throw new Error(`rpc.ts 里找不到 ${name}`);
  const body = text.slice(start, text.indexOf("};", start));
  return new Set([...body.matchAll(/"([a-z_]+\/[a-z_]+)"\s*:/g)].map((m) => m[1]));
}

function methodLiterals(): {
  calls: Set<string>;
  notifications: Set<string>;
  requests: Set<string>;
  notificationMap: Set<string>;
  requestMap: Set<string>;
} {
  const calls = new Set<string>();
  const notifications = new Set<string>();
  const requests = new Set<string>();
  let notificationMap = new Set<string>();
  let requestMap = new Set<string>();
  // 只看生产代码：测试桩（rpc.test.ts 的 "a/one" 等）用假方法名，不该对账。
  const CALL = /rpc\.(?:call|notify)(?:<[^>]*>)?\(\s*"([a-z_]+\/[a-z_]+)"/g;
  const ON = /rpc\.on(?:<[^>]*>)?\(\s*"([a-z_]+\/[a-z_]+)"/g;
  const ON_REQUEST = /rpc\.onRequest(?:<[^>]*>)?\(\s*"([a-z_]+\/[a-z_]+)"/g;
  for (const file of tsFiles(SRC)) {
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(CALL)) calls.add(match[1]);
    for (const match of text.matchAll(ON)) notifications.add(match[1]);
    for (const match of text.matchAll(ON_REQUEST)) requests.add(match[1]);
    if (file.endsWith("rpc.ts")) {
      notificationMap = mapKeys(text, "NotificationMap");
      requestMap = mapKeys(text, "RequestMap");
    }
  }
  return { calls, notifications, requests, notificationMap, requestMap };
}

// ---------- 对账 ----------

describe("protocol schema 对账", () => {
  it("PROTOCOL_VERSION 与 Rust 侧一致", () => {
    expect(PROTOCOL_VERSION).toBe(SCHEMA.protocolVersion);
  });

  it("rpc.call/on/onRequest 只使用方法索引里的方法，且方向正确", () => {
    const { calls, notifications, requests } = methodLiterals();
    const client = new Set(Object.keys(SCHEMA.methods.client));
    const serverNotifications = new Set(Object.keys(SCHEMA.methods.server_notification));
    const serverRequests = new Set(Object.keys(SCHEMA.methods.server_request));
    expect([...calls].filter((name) => !client.has(name))).toEqual([]);
    expect([...notifications].filter((name) => !serverNotifications.has(name))).toEqual([]);
    expect([...requests].filter((name) => !serverRequests.has(name))).toEqual([]);
  });

  it("rpc.ts 的监听/请求 map 与 server 侧方法索引一一对应", () => {
    const { notificationMap, requestMap } = methodLiterals();
    expect([...notificationMap].sort()).toEqual(
      Object.keys(SCHEMA.methods.server_notification).sort(),
    );
    expect([...requestMap].sort()).toEqual(Object.keys(SCHEMA.methods.server_request).sort());
  });

  it("types.gen.ts 与 schema 同步（banner 指纹一致）", () => {
    const digest = createHash("sha256").update(SCHEMA_TEXT).digest("hex").slice(0, 16);
    const head = readFileSync(join(HERE, "types.gen.ts"), "utf8").slice(0, 400);
    expect(head).toContain(`// schema sha256: ${digest}`);
  });
});
