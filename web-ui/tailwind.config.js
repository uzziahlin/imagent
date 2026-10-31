/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  // 深浅色双驱动：html[data-theme] 属性（手动三态开关，lib/theme.ts）缺省
  // 交还系统——"system" 时移除属性，行为等同旧 media 策略（index.css 深色
  // 变量块 media 查询接管）。dark: 变体选择器即 html[data-theme="dark"] 下的
  // 后代；语义色（surface/panel/ink/…）走 CSS 变量，定义见 index.css。
  darkMode: ["class", '[data-theme="dark"]'],
  theme: {
    extend: {
      colors: {
        surface: "var(--c-bg)",
        panel: "var(--c-panel)",
        panel2: "var(--c-panel2)",
        code: "var(--c-code)",
        line: "var(--c-line)",
        "line-strong": "var(--c-line-strong)",
        ink: "var(--c-ink)",
        muted: "var(--c-muted)",
        faint: "var(--c-faint)",
        ghost: "var(--c-ghost)",
        ok: "var(--c-ok)",
        warn: "var(--c-warn)",
        err: "var(--c-err)",
        info: "var(--c-info)",
        think: "var(--c-think)",
      },
      // 微动效 keyframes（prefers-reduced-motion 禁用见 index.css）：
      // fade-in：卡片正文流式补丁的轻微淡入（120ms，起始 0.55 避免流式高频闪烁）
      // slide-in：审批弹层滑入（150ms ease-out）
      // breathe：在线点呼吸（2.2s 缓慢缩放+透明度；仅 ready/在线态使用）
      keyframes: {
        "fade-in": {
          "0%": { opacity: "0.55" },
          "100%": { opacity: "1" },
        },
        "slide-in": {
          "0%": { opacity: "0", transform: "translateY(-10px)" },
          "100%": { opacity: "1", transform: "translateY(0)" },
        },
        breathe: {
          "0%, 100%": { opacity: "1", transform: "scale(1)" },
          "50%": { opacity: "0.5", transform: "scale(1.3)" },
        },
      },
      animation: {
        "fade-in": "fade-in 120ms ease-out",
        "slide-in": "slide-in 150ms ease-out",
        breathe: "breathe 2.2s ease-in-out infinite",
      },
    },
  },
  plugins: [],
};
