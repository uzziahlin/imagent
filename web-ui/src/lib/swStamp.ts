// Service Worker 构建戳（v1.41.7）：纯函数模块，构建期（vite.config.ts 的
// stampServiceWorker 插件）与单测共用——本文件不做任何文件 I/O。
//
// 背景：浏览器只在 SW 脚本逐字节不同时才安装新 SW。public/sw.js 被原样拷进
// 产物、此前不含任何构建期信息——自 v1.39.2 起所有发版的 sw.js 字节完全相同，
// 新 SW 永不安装，首页文档被旧 SW 的 cache-first 永久冻结（真机「发版后驾驶舱
// 永远不更新」的根因）。
//
// 为什么哈希用 @noble/hashes 而不是 node:crypto：本模块在 tsconfig 的 include
// （src）内、又被单测导入，必须在 tsc 下类型自洽；仓库未装 @types/node（Node
// 侧代码按惯例只留在 vite.config.ts，不进 tsc 类型面）。@noble/hashes 是应用
// 既有依赖（纯 JS、自带类型、Node/浏览器/测试环境通用），结果与 sha256 标准
// 向量一致（见 swStamp.test）。
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex } from "@noble/hashes/utils.js";

/** sw.js 源文件里的构建戳占位符（vite.config.ts 在**构建产物**上替换为实值；
 *  源文件 public/sw.js 始终保持占位符原样）。 */
export const SW_BUILD_PLACEHOLDER = "__SW_BUILD__";

/** 构建戳长度：sha256 hex 的前 16 位（64 bit）。 */
const STAMP_HEX_LEN = 16;

/** 构建戳 = index.html **字节**的 sha256 前 16 位 hex（小写）。纯内容寻址：
 *  同一份产物 → 同一个戳（重新构建不无谓改动 sw.js 字节）；index.html 引用的
 *  是带 hash 的资产文件名，应用代码任何变化都会落到它的字节上 → 戳必变。
 *  不含版本号——版本号变了但产物没变时不触发 SW 重装。 */
export function computeBuildStamp(indexHtml: Uint8Array): string {
  return bytesToHex(sha256(indexHtml)).slice(0, STAMP_HEX_LEN);
}

/** 把 sw.js 源码里的全部构建戳占位符替换为 stamp。一个占位符都没有 → 抛错：
 *  没被戳上的 sw.js 字节恒不变，客户端永远无法感知新版本——这正是本机制要
 *  修的 bug，必须在构建期失败，不能静默带病上线。 */
export function stampServiceWorkerSource(swSource: string, stamp: string): string {
  if (!swSource.includes(SW_BUILD_PLACEHOLDER)) {
    throw new Error(
      `sw.js 源文件缺少 ${SW_BUILD_PLACEHOLDER} 占位符，客户端将永远无法感知新版本`,
    );
  }
  return swSource.replaceAll(SW_BUILD_PLACEHOLDER, stamp);
}
