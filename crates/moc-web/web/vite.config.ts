import { fileURLToPath, URL } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

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
    outDir: "dist",
    assetsDir: "assets",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/ws": { target: "ws://127.0.0.1:7432", ws: true },
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
  },
});
