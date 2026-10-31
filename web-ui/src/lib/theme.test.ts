// lib/theme 单测：三态偏好存取（缺省 system / 往返 / 非法值回退）、applyTheme
// 对 documentElement 属性与 theme-color meta 的副作用（system 移除属性且 meta
// 随 matchMedia 探测）、meta 缺失自动补建、initTheme 一次完成读+应用（main.tsx
// 渲染前防闪烁的入口契约）。
import { afterEach, describe, expect, it, vi } from "vitest";
import { applyTheme, initTheme, loadThemePref, saveThemePref } from "./theme";

afterEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("data-theme");
  document.querySelector('meta[name="theme-color"]')?.remove();
  vi.restoreAllMocks();
});

const metaContent = () =>
  document.querySelector<HTMLMetaElement>('meta[name="theme-color"]')?.content ?? null;

describe("theme 偏好存取", () => {
  it("缺省 system；合法值往返；非法值回退 system", () => {
    expect(loadThemePref()).toBe("system");

    saveThemePref("dark");
    expect(loadThemePref()).toBe("dark");
    saveThemePref("light");
    expect(loadThemePref()).toBe("light");
    saveThemePref("system");
    expect(loadThemePref()).toBe("system");

    localStorage.setItem("imagent-theme", "blue");
    expect(loadThemePref()).toBe("system");
  });
});

describe("applyTheme 副作用", () => {
  it("light/dark：设置 data-theme 属性 + meta 写对应 content", () => {
    applyTheme("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(metaContent()).toBe("#020617");

    applyTheme("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
    expect(metaContent()).toBe("#f6f8fb");
  });

  it("system：移除 data-theme 属性（交还 CSS media 查询）；meta 随系统探测（jsdom 缺省浅色）", () => {
    document.documentElement.setAttribute("data-theme", "dark");
    applyTheme("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
    expect(metaContent()).toBe("#f6f8fb");
  });

  it("system + 系统深色（matchMedia stub）：属性仍移除，meta 写深色", () => {
    vi.spyOn(window, "matchMedia").mockReturnValue({ matches: true } as MediaQueryList);
    applyTheme("system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
    expect(metaContent()).toBe("#020617");
  });

  it("meta 不存在时自动补建且仅一条（jsdom 裸文档 / 生产兜底）", () => {
    expect(document.querySelector('meta[name="theme-color"]')).toBeNull();
    applyTheme("dark");
    expect(document.querySelectorAll('meta[name="theme-color"]')).toHaveLength(1);
    expect(metaContent()).toBe("#020617");
  });
});

describe("initTheme（渲染前初始化）", () => {
  it("读偏好并一次应用：属性与 meta 同帧到位", () => {
    saveThemePref("dark");
    document.documentElement.removeAttribute("data-theme");
    initTheme();
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(metaContent()).toBe("#020617");
  });
});
