import { useEffect, useState } from "react";

/** 最左侧标签栏切换的顶层页面；各自对应一个 URL 路径，前进后退可用。 */
export type AppRoute = "agent" | "image" | "plugins";

const PATHS: Record<AppRoute, string> = {
  agent: "/",
  image: "/image-playground",
  plugins: "/plugins",
};

export function routeOf(pathname: string): AppRoute {
  if (pathname.startsWith(PATHS.image)) return "image";
  if (pathname.startsWith(PATHS.plugins)) return "plugins";
  return "agent";
}

/** 切页只改路径，保留 query（含 token）。 */
export function navigate(route: AppRoute): void {
  if (routeOf(window.location.pathname) === route) return;
  window.history.pushState(null, "", `${PATHS[route]}${window.location.search}`);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

export function useRoute(): AppRoute {
  const [route, setRoute] = useState(() => routeOf(window.location.pathname));
  useEffect(() => {
    const onPop = () => setRoute(routeOf(window.location.pathname));
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  return route;
}
