// 当前是否为暗色：跟随 <html class="dark">（主题逻辑统一在那里落地）。

import { useSyncExternalStore } from "react";

function subscribe(onChange: () => void): () => void {
  const observer = new MutationObserver(onChange);
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
  return () => observer.disconnect();
}

function snapshot(): boolean {
  return document.documentElement.classList.contains("dark");
}

export function useIsDark(): boolean {
  return useSyncExternalStore(subscribe, snapshot);
}
