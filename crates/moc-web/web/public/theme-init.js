// 首帧前设好 .dark，避免亮色闪屏。主题权威来源是 core 的 web.toml；
// store/prefs.ts 把已生效的主题镜像到 moc-web-theme 供这里预读。
// 独立成文件是因为 CSP script-src 只放行自身来源，不豁免内联脚本。
(() => {
  const theme = localStorage.getItem("moc-web-theme") || "system";
  const dark =
    theme === "dark" ||
    (theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
})();
