// dist 只保留 woff2：KaTeX 每种字形同时发 woff2/woff/ttf 三份，@font-face 的 src
// 列表里 woff2 排第一、现代浏览器全部命中；另两份从不被请求，只增加二进制内嵌体积。

import { readdirSync, unlinkSync } from "node:fs";
import { join } from "node:path";

const assets = join(process.env.MOC_WEB_OUT_DIR ?? "dist", "assets");
let removed = 0;
for (const file of readdirSync(assets)) {
  if (file.endsWith(".woff") || file.endsWith(".ttf")) {
    unlinkSync(join(assets, file));
    removed += 1;
  }
}
console.log(`prune-dist: removed ${removed} non-woff2 font files`);
