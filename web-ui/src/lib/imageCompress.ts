// v1.7 图片压缩（契约 A 段，SPA 侧预压缩规避网关 8MB 上限）：
// 选图 → createImageBitmap + canvas 缩放（最长边 > 2048 才缩，等比）→
// JPEG quality 0.85 重编码（原始尺寸小于阈值则原样尺寸、仍重编码）→
// 输出 {name: 基原名换 .jpg 后缀, mime: "image/jpeg", b64: 单行 base64}。
// 阈值判断（scaledSize）与文件名替换（jpgName）为纯函数，jsdom 无 canvas 的
// 单测只覆盖这两处；canvas 主路径在组件集成测试里经 vi.mock 走 fake。

import type { MediaPayload } from "./protocol";

/** 缩放阈值：最长边超过该值才等比缩小（契约 A 段）。 */
export const IMAGE_MAX_EDGE = 2048;

/** JPEG 重编码质量（契约 A 段：0.85）。 */
export const IMAGE_JPEG_QUALITY = 0.85;

/** 纯函数：等比缩放尺寸——最长边 > IMAGE_MAX_EDGE 才缩（四舍五入取整），
 *  否则原样返回（宽高不变，仍走 JPEG 重编码）。 */
export function scaledSize(w: number, h: number): { w: number; h: number } {
  const longest = Math.max(w, h);
  if (longest <= IMAGE_MAX_EDGE) {
    return { w, h };
  }
  const ratio = IMAGE_MAX_EDGE / longest;
  return { w: Math.round(w * ratio), h: Math.round(h * ratio) };
}

/** 纯函数：输出文件名——基原名换 .jpg 后缀（无扩展名或以点开头的隐藏文件
 *  形态直接追加 .jpg；重编码后必然是 JPEG，契约规定 name 后缀 .jpg）。 */
export function jpgName(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? `${name.slice(0, dot)}.jpg` : `${name}.jpg`;
}

/**
 * 压缩图片文件为可随 message 帧发送的 media 载荷。浏览器主路径：
 * createImageBitmap 解码 → canvas 缩放绘制 → toDataURL(image/jpeg, 0.85) →
 * 剥 data: 前缀取单行 base64。解码失败（非图片/损坏）抛错由调用方提示。
 */
export async function compressImage(file: File): Promise<MediaPayload> {
  const bitmap = await createImageBitmap(file);
  const { w, h } = scaledSize(bitmap.width, bitmap.height);
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    bitmap.close();
    throw new Error("canvas 2d 上下文不可用");
  }
  ctx.drawImage(bitmap, 0, 0, w, h);
  bitmap.close();
  const dataUrl = canvas.toDataURL("image/jpeg", IMAGE_JPEG_QUALITY);
  return { name: jpgName(file.name), mime: "image/jpeg", b64: dataUrl.slice(dataUrl.indexOf(",") + 1) };
}
