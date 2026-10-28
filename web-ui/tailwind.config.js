/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  // 深浅色跟随系统（无手动切换开关）；dark: 变体即 prefers-color-scheme。
  // 语义色（surface/panel/ink/…）走 CSS 变量，定义见 index.css。
  darkMode: "media",
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
    },
  },
  plugins: [],
};
