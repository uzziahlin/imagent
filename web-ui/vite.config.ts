/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 公网路径：Caddy `handle_path /imagent/*` strip 前缀后转 relay，
// 故 base 固定 /imagent/，产物布局保持 Vite 默认（index.html + assets/*.hash）
// —— relay 的 spa_fallback 对 /assets/ 前缀给 immutable 长缓存、index no-store。
export default defineConfig({
  base: "/imagent/",
  plugins: [react()],
  build: {
    // 产物直接落 relay rust-embed 内嵌目录（提交进仓库，CI 不装 node）。
    // emptyOutDir 覆盖 M1 占位 index.html，属预期行为。
    outDir: "../crates/relay/static",
    emptyOutDir: true,
  },
  server: {
    // 本地开发：`cargo run -p imagent-relay`（IMAGENT_RELAY_BIND=127.0.0.1:3010）
    proxy: {
      "/imagent/ws": {
        target: "ws://127.0.0.1:3010",
        ws: true,
        rewrite: (p) => p.replace(/^\/imagent/, ""),
      },
      "/imagent/healthz": {
        target: "http://127.0.0.1:3010",
        rewrite: (p) => p.replace(/^\/imagent/, ""),
      },
      // v1.3 passkey HTTP 端点（同源 fetch；本地直连形态 rp_id=localhost
      // 可用——注意 http://127.0.0.1 形态 WebAuthn 不受支持，开发页请开
      // http://localhost:5173/imagent/）
      "/imagent/webauthn": {
        target: "http://127.0.0.1:3010",
        rewrite: (p) => p.replace(/^\/imagent/, ""),
      },
    },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
