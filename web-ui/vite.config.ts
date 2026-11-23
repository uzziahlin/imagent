/// <reference types="vitest/config" />
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 构建期读取版本号（package.json 随批发版更新），经 define 注入全局常量
// __APP_VERSION__（类型声明在 src/vite-env.d.ts）——SW 更新提示文案携带版本。
const pkg = JSON.parse(
  readFileSync(fileURLToPath(new URL("./package.json", import.meta.url)), "utf-8"),
) as { version: string };
const APP_VERSION_DEFINE = { __APP_VERSION__: JSON.stringify(pkg.version) };

// 公网路径：Caddy `handle_path /imagent/*` strip 前缀后转 relay，
// 故 base 固定 /imagent/，产物布局保持 Vite 默认（index.html + assets/*.hash）
// —— relay 的 spa_fallback 对 /assets/ 前缀给 immutable 长缓存、index no-store。
export default defineConfig({
  base: "/imagent/",
  plugins: [react()],
  define: APP_VERSION_DEFINE,
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
    // 测试里同样注入 __APP_VERSION__（文案断言引用该常量，注入缺失即红）
    define: APP_VERSION_DEFINE,
  },
});
