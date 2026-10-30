import { useEffect, useState } from "react";
import { NARROW_VIEWPORT } from "@/store/layout";

/** 窗口是否窄于 {@link NARROW_VIEWPORT}：侧栏与检查面板据此切换为浮层。 */
export function useNarrow(): boolean {
  const query = `(max-width: ${NARROW_VIEWPORT - 1}px)`;
  const [narrow, setNarrow] = useState(() => matchMedia(query).matches);
  useEffect(() => {
    const media = matchMedia(query);
    const onChange = () => setNarrow(media.matches);
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, [query]);
  return narrow;
}
