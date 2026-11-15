// webui 内建 i18n：类型化字典（zh 为主表，en 必须同键全集），不引第三方 i18n 依赖。
//
// locale 来源：`config/get` 返回的 `[general] language`（""/"auto" 时按
// navigator.language 解析），由 store/connection.ts 在握手后调用 setLanguage。
// `useT` 订阅 locale 让组件随切换重渲染；非组件代码用模块级 `t()`。

import { create } from "zustand";
import { en } from "./en";
import { zh } from "./zh";

export type Locale = "zh" | "en";

export type MsgKey = keyof typeof zh;

const dicts: Record<Locale, Record<MsgKey, string>> = { zh, en };

export type I18nState = {
  locale: Locale;
};

export const useI18n = create<I18nState>()(() => ({ locale: "zh" }));

/** 当前 locale（非 hook）；需随 locale 分支语法时使用。 */
export const currentLocale = () => useI18n.getState().locale;

/** `[general] language` 原始值 → 有效 locale；auto/空/未知值按浏览器语言定。 */
export function resolveLocale(language?: string | null): Locale {
  if (language === "zh" || language === "en") return language;
  return navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

/** 握手后由 connection store 调用；language 是服务端 `[general] language` 原值。 */
export function setLanguage(language?: string | null) {
  const locale = resolveLocale(language);
  if (locale !== useI18n.getState().locale) useI18n.setState({ locale });
}

/** 翻译：`{name}` 形式的占位符由 vars 填充；缺键时返回键名（开发期可见）。 */
export function t(key: MsgKey, vars?: Record<string, string | number>): string {
  const text = dicts[useI18n.getState().locale][key] ?? key;
  if (!vars) return text;
  return text.replace(/\{(\w+)\}/g, (raw, name: string) =>
    vars[name] === undefined ? raw : String(vars[name]),
  );
}

/** 组件入口：订阅 locale，切换时触发重渲染。 */
export function useT() {
  useI18n((state) => state.locale);
  return t;
}
