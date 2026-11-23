/// <reference types="vitest/config" />
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, searchForWorkspaceRoot, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { computeBuildStamp, stampServiceWorkerSource } from "./src/lib/swStamp";

// 构建期读取版本号（package.json 随批发版更新），经 define 注入全局常量
// __APP_VERSION__（类型声明在 src/vite-env.d.ts）——侧栏底部显示当前运行版本
// （排查「发版后没更新」时确认实际在跑哪一版；版本号随批发版同步）。
const pkg = JSON.parse(
  readFileSync(fileURLToPath(new URL("./package.json", import.meta.url)), "utf-8"),
) as { version: string };
const APP_VERSION_DEFINE = { __APP_VERSION__: JSON.stringify(pkg.version) };

/**
 * sw.js 构建期打戳（v1.41.7）：浏览器只在 SW 脚本逐字节不同时才安装新 SW，
 * 而 public/sw.js 被 Vite 原样拷贝、此前不含任何构建期信息——自 v1.39.2 起
 * 所有发版的 sw.js 字节完全相同，新 SW 永不安装，首页文档被旧 SW 的
 * cache-first 永久冻结（真机「发版后驾驶舱永远不更新」的根因）。
 *
 * 做法：closeBundle 里读产物 index.html 算戳（sha256 前 16 位 hex，纯函数在
 * src/lib/swStamp.ts）→ 替换产物 sw.js 里的全部 __SW_BUILD__ 占位符 → 写回。
 * 戳是内容寻址的：同一份产物 → 同一个戳 → sw.js 字节不变（不无谓重装 SW）；
 * 产物变 → 戳必变。只改**产物**：public/sw.js 源文件始终保持占位符原样。
 *
 * 钩子时序（vite 5.4.21 源码 + 实际构建实证）：Vite 在 bundle.write() 之前
 * 就把 public/ 拷进 outDir（prepareOutDir），index.html 随 writeBundle 落盘，
 * closeBundle 由 bundle.close() 触发、恒在两者之后——此处两个文件均已就位。
 */
function stampServiceWorker(): Plugin {
  // outDir 由 configResolved 落位（Vite 已解析为绝对路径——不在此重复写死
  // 路径）；written = 本轮构建是否已成功写出（见 closeBundle 守卫）。
  let outDir: string;
  let written = false;
  return {
    name: "stamp-service-worker",
    apply: "build",
    configResolved(config) {
      outDir = config.build.outDir;
    },
    buildStart() {
      // watch 模式每轮构建重置：上一轮写过不代表这一轮成功
      written = false;
    },
    writeBundle() {
      written = true;
    },
    closeBundle() {
      // 构建中途失败（解析 / 写出抛错）时 rollup 仍会触发 closeBundle：此刻
      // 产物不全，直接返回——否则这里的读文件异常会在 finally 路径上顶替、
      // 掩盖真正的构建错误。writeBundle 成功过 = 产物齐全，此后才打戳。
      if (!written) {
        return;
      }
      const stamp = computeBuildStamp(readFileSync(resolve(outDir, "index.html")));
      const swPath = resolve(outDir, "sw.js");
      writeFileSync(swPath, stampServiceWorkerSource(readFileSync(swPath, "utf-8"), stamp));
    },
  };
}

// 公网路径：Caddy `handle_path /imagent/*` strip 前缀后转 relay，
// 故 base 固定 /imagent/，产物布局保持 Vite 默认（index.html + assets/*.hash）
// —— relay 的 spa_fallback 对 /assets/ 前缀给 immutable 长缓存、index no-store。
export default defineConfig({
  base: "/imagent/",
  plugins: [react(), stampServiceWorker()],
  define: APP_VERSION_DEFINE,
  build: {
    // 产物直接落 relay rust-embed 内嵌目录（提交进仓库，CI 不装 node）。
    // emptyOutDir 覆盖 M1 占位 index.html，属预期行为。
    outDir: "../crates/relay/static",
    emptyOutDir: true,
  },
  server: {
    // 单测经 ?raw 读取仓库内已提交的构建产物（src/lib/swStamp.test.ts 的
    // 「提交产物一致性」用例）：产物目录在 web-ui 之外，不在 vite 默认的文件
    // 访问白名单内，按目录精确放行（只此一个目录；首项即默认的项目根，显式
    // 写出是因为 allow 一旦自定义就整体替换默认值）。
    fs: {
      allow: [searchForWorkspaceRoot(process.cwd()), "../crates/relay/static"],
    },
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
