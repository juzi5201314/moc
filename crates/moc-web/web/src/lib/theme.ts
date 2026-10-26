// 主题落地：按 prefs.theme 切换 <html class="dark">；system 时跟随 prefers-color-scheme。
// 首帧由 index.html 的内联脚本处理，这里负责之后的变化。

import { useEffect } from "react";
import { type Theme, usePrefs } from "@/store/prefs";

const DARK_QUERY = "(prefers-color-scheme: dark)";

function applyTheme(theme: Theme): void {
  const dark = theme === "dark" || (theme === "system" && matchMedia(DARK_QUERY).matches);
  document.documentElement.classList.toggle("dark", dark);
}

export function useThemeSync(): void {
  const theme = usePrefs((state) => state.theme);
  useEffect(() => {
    applyTheme(theme);
    if (theme !== "system") return;
    const media = matchMedia(DARK_QUERY);
    const onChange = () => applyTheme("system");
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, [theme]);
}
