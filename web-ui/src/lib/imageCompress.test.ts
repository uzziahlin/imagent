// v1.7 图片压缩纯逻辑（契约 A 段）：阈值判断（scaledSize）与文件名替换
// （jpgName）。canvas 主路径依赖 createImageBitmap/2d context——jsdom 无
// canvas，组件集成侧（Composer 测试）经 vi.mock 覆盖，此处只测纯函数。
import { describe, expect, it } from "vitest";
import { IMAGE_JPEG_QUALITY, IMAGE_MAX_EDGE, jpgName, scaledSize } from "./imageCompress";

describe("scaledSize（最长边 2048 阈值）", () => {
  it("最长边 ≤2048 原样返回（不缩放；仍会走 JPEG 重编码）", () => {
    expect(scaledSize(2048, 2048)).toEqual({ w: 2048, h: 2048 });
    expect(scaledSize(1920, 1080)).toEqual({ w: 1920, h: 1080 });
    expect(scaledSize(800, 600)).toEqual({ w: 800, h: 600 });
    expect(scaledSize(1, 1)).toEqual({ w: 1, h: 1 });
  });

  it("最长边 >2048 等比缩到 2048（横图/竖图各按其最长边）", () => {
    // 横图：宽是长边
    expect(scaledSize(4096, 2048)).toEqual({ w: 2048, h: 1024 });
    // 竖图：高是长边（1080×4320 → 512×2048）
    expect(scaledSize(1080, 4320)).toEqual({ w: 512, h: 2048 });
    // 非整除比例四舍五入取整
    expect(scaledSize(3000, 1000)).toEqual({ w: 2048, h: Math.round((1000 * 2048) / 3000) });
  });

  it("阈值与质量常量与契约一致（2048 / 0.85）", () => {
    expect(IMAGE_MAX_EDGE).toBe(2048);
    expect(IMAGE_JPEG_QUALITY).toBe(0.85);
  });
});

describe("jpgName（基原名换 .jpg 后缀）", () => {
  it("常见图片扩展名替换为 .jpg", () => {
    expect(jpgName("shot.png")).toBe("shot.jpg");
    expect(jpgName("照片.HEIC")).toBe("照片.jpg");
    expect(jpgName("IMG_1234.JPEG")).toBe("IMG_1234.jpg");
    expect(jpgName("webp-pic.webp")).toBe("webp-pic.jpg");
  });

  it("无扩展名 / 隐藏文件形态 / 多点文件名", () => {
    expect(jpgName("screenshot")).toBe("screenshot.jpg");
    expect(jpgName(".profile")).toBe(".profile.jpg"); // 点开头无主干 → 追加
    expect(jpgName("a.b.c.png")).toBe("a.b.c.jpg"); // 只换最后一段
  });
});
