/**
 * 主题三态偏好（跟随系统/浅色/深色，缺省跟随系统）：
 * - 存储：localStorage `imagent-theme`（非法/缺失值一律回落 "system"）；
 * - 生效：html[data-theme] 属性驱动——index.css 深色变量块双处生效（media
 *   查询 + data-theme 选择器）、tailwind darkMode 自定义选择器同属性；
 *   "system" 移除属性，交还 CSS media 查询，系统切换由浏览器实时跟随；
 * - theme-color meta：index.html 仅留一条无 media 的 meta，运行时按生效
 *   主题写 content（浅 #f6f8fb / 深 #020617，= --c-bg 两态值）。PWA 安装
 *   启动画面的 theme_color 仍在 manifest.json（与运行时 meta 分离）。
 * initTheme() 必须在 React 渲染前调用（main.tsx 顶部）：首帧即终态防闪烁。
 */

export type ThemePref = "system" | "light" | "dark";

const STORAGE_KEY = "imagent-theme";

const THEME_PREFS: readonly string[] = ["system", "light", "dark"];

/** 浏览器 UI（地址栏/任务切换器）随主题色：浅/深两态值（index.css --c-bg）。 */
const THEME_COLOR: Record<"light" | "dark", string> = {
  light: "#f6f8fb",
  dark: "#020617",
};

/** 读取偏好：缺省/非法值回落 "system"（不写回，读路径零副作用）。 */
export function loadThemePref(): ThemePref {
  const raw = localStorage.getItem(STORAGE_KEY);
  return raw !== null && THEME_PREFS.includes(raw) ? (raw as ThemePref) : "system";
}

/** 持久化偏好（侧栏切换时由 App 一并调用）。 */
export function saveThemePref(pref: ThemePref): void {
  localStorage.setItem(STORAGE_KEY, pref);
}

/** 生效主题："system" 实时探测系统（matchMedia，同 main.tsx 的 visualViewport
 *  防御：缺失的旧环境回落浅色）；light/dark 即偏好本身。 */
function effectiveTheme(pref: ThemePref): "light" | "dark" {
  if (pref !== "system") {
    return pref;
  }
  return window.matchMedia?.("(prefers-color-scheme: dark)")?.matches === true
    ? "dark"
    : "light";
}

/** 同步 theme-color meta：缺 meta 时补建（jsdom 裸文档 / 生产兜底）。 */
function syncThemeColorMeta(theme: "light" | "dark"): void {
  let meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
  if (!meta) {
    meta = document.createElement("meta");
    meta.name = "theme-color";
    document.head.appendChild(meta);
  }
  meta.content = THEME_COLOR[theme];
}

/** 应用偏好到 document：html[data-theme] 属性 + theme-color meta。
 *  "system" 移除属性（CSS media 查询接管，浏览器实时跟随系统切换）。 */
export function applyTheme(pref: ThemePref): void {
  const root = document.documentElement;
  if (pref === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", pref);
  }
  syncThemeColorMeta(effectiveTheme(pref));
}

/** 启动初始化（读 + 应用一次）：main.tsx 顶部、React 渲染前调用防闪烁。 */
export function initTheme(): void {
  applyTheme(loadThemePref());
}
