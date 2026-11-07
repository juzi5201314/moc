// 由 moc-protocol/protocol.schema.json 生成 src/protocol/types.gen.ts。
//
// 流程：Rust 侧改 DTO → `MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol`
// 刷新 schema → `pnpm gen:protocol` 重出本文件 → schema.test.ts 对账新鲜度。
// 生成物不含方法索引与版本（那是文档/对账面），只含 $defs 里的线上 DTO 类型。

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { compile } from "json-schema-to-typescript";

const HERE = dirname(fileURLToPath(import.meta.url));
const SCHEMA = resolve(HERE, "../../../moc-protocol/protocol.schema.json");
const OUT = resolve(HERE, "../src/protocol/types.gen.ts");

const schemaText = readFileSync(SCHEMA, "utf8");
const schema = JSON.parse(schemaText);
// 溯源指纹写进 banner：schema.test.ts 重算它，确认生成物没落后于 schema。
const digest = createHash("sha256").update(schemaText).digest("hex").slice(0, 16);

const ts = await compile({ $defs: schema.$defs }, "ProtocolTypes", {
  // 手写区在 types.ts：生成区只留类型，版本/常量/helper 不进生成文件。
  bannerComment:
    "// Code generated from moc-protocol/protocol.schema.json; DO NOT EDIT.\n" +
    `// schema sha256: ${digest}\n` +
    "// 重生成：pnpm gen:protocol（Rust 侧先 MOC_UPDATE_SCHEMA=1 cargo test -p moc-protocol）。\n" +
    "// biome-ignore-all lint/complexity/noBannedTypes: 空 params 的 schema 没有字段，json2ts 生成空类型\n" +
    "// biome-ignore-all lint/suspicious/noEmptyInterface: 空 params 的 schema 没有字段，json2ts 生成空接口",
  // serde 容忍未知字段但 TS 不需要为此开洞；deny_unknown_fields 的
  // additionalProperties:false 已在 schema 里显式带过来。
  additionalProperties: false,
  // 根是空 `{$defs}`，不引用任何 def——必须显式要求输出全部 defs。
  unreachableDefinitions: true,
  format: false,
  unknownAny: true,
});

// compile() 一定给根名产出一个占位 interface；剥掉它。
// （注：类型 JSDoc 里也会出现 `ProtocolTypes` 引用字样，只能查导出形态。）
const text = ts.replace(/export interface ProtocolTypes \{[^}]*\}\n?/, "");
if (/export (interface|type) ProtocolTypes\b/.test(text)) {
  throw new Error("json2ts 根导出形状变了：检查 compile 输出，再调整占位符剥离");
}

writeFileSync(OUT, text);
execFileSync("pnpm", ["exec", "biome", "format", "--write", OUT], {
  cwd: resolve(HERE, ".."),
  stdio: "inherit",
});
