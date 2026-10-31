import { createCn } from "cn/config";

// 登记 index.css @theme 里的自定义字号；不登记的话 text-ui 等会被当成文字颜色，
// 与 text-muted-foreground 这类颜色类互相覆盖。
export const cn = createCn({
  extend: { classGroups: { "font-size": [{ text: ["ui", "chat", "title", "heading"] }] } },
});
