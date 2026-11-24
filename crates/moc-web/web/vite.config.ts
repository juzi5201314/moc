import { fileURLToPath, URL } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const DOM_TESTS = [
  "src/**/*.dom.test.{ts,tsx}",
  "src/store/**/*.test.{ts,tsx}",
  "src/lib/plan-annotation.test.ts",
  "src/lib/slash.test.ts",
  "src/protocol/rpc.test.ts",
  "src/components/ai-elements/svg-figure.test.ts",
];

// 开发：一个终端 `moc web --token dev`，另一个 `pnpm dev`，
// 打开 http://localhost:5173/?token=dev；/ws 代理到 moc web。
export default defineConfig({
  base: "/",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    // cargo 构建时 build.rs 经 MOC_WEB_OUT_DIR 指向 OUT_DIR，产物不落进源码树。
    outDir: process.env.MOC_WEB_OUT_DIR ?? "dist",
    assetsDir: "assets",
    emptyOutDir: true,
    // 产物会被压进二进制分发；sourcemap 占 dist 的三分之二且只对 `pnpm dev` 有用。
    sourcemap: false,
  },
  server: {
    proxy: {
      "/ws": { target: "ws://127.0.0.1:7432", ws: true },
      "/pty": { target: "ws://127.0.0.1:7432", ws: true },
      "/commands.json": "http://127.0.0.1:7432",
      "/features.json": "http://127.0.0.1:7432",
      "/doctor.json": "http://127.0.0.1:7432",
      "/procstats.json": "http://127.0.0.1:7432",
      "/fs/raw": "http://127.0.0.1:7432",
      "/uploads": "http://127.0.0.1:7432",
      "/widget-host": "http://127.0.0.1:7432",
    },
  },
  test: {
    // 仅对带 DOM/窗口依赖的测试启用 jsdom，避免纯逻辑测试重复创建 jsdom。
    globals: true,
    projects: [
      {
        extends: true,
        test: {
          name: "node",
          environment: "node",
          include: ["src/**/*.test.{ts,tsx}"],
          exclude: DOM_TESTS,
        },
      },
      {
        extends: true,
        test: {
          name: "dom",
          environment: "jsdom",
          include: DOM_TESTS,
        },
      },
    ],
  },
});
