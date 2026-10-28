import { useState, type FormEvent } from "react";

/** 登录页：输入 USER_TOKEN → 存 localStorage（key imagent.userToken）→ 进会话页。 */
export function Login({ onSave }: { onSave: (token: string) => void }) {
  const [value, setValue] = useState("");
  const [error, setError] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const token = value.trim();
    if (!token) {
      setError("请输入访问令牌");
      return;
    }
    onSave(token);
  };

  // min-h-full 跟随 #root（键盘弹出时 --vvh 收缩，卡片在可见区居中）
  return (
    <div className="flex min-h-full items-center justify-center px-4">
      <div className="w-full max-w-sm">
        <div className="mb-8 text-center">
          <div className="mx-auto mb-3 flex h-14 w-14 items-center justify-center rounded-2xl border border-line-strong bg-panel">
            <svg viewBox="0 0 64 64" className="h-8 w-8" aria-hidden="true">
              <polyline
                points="14,22 24,32 14,42"
                fill="none"
                stroke="#34d399"
                strokeWidth="5"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
              <line x1="30" y1="43" x2="50" y2="43" stroke="#34d399" strokeWidth="5" strokeLinecap="round" />
            </svg>
          </div>
          <h1 className="text-xl font-semibold text-ink">imagent · Web 驾驶舱</h1>
          <p className="mt-1 text-sm text-muted">输入访问令牌连接你的 Mac 网关</p>
        </div>
        <form onSubmit={submit} className="rounded-2xl border border-line bg-panel p-5 shadow-xl">
          <label htmlFor="token" className="mb-2 block text-sm font-medium text-ink">
            访问令牌（USER_TOKEN）
          </label>
          {/*
           * text-base（16px）：字号 <16px 时 iOS Safari 聚焦会自动放大整页、
           * 失焦弹回；禁止用 viewport meta 禁缩放（保留双指缩放无障碍）。
           */}
          <input
            id="token"
            type="password"
            autoComplete="current-password"
            autoFocus
            value={value}
            onChange={(e) => {
              setValue(e.target.value);
              setError("");
            }}
            placeholder="imagent.userToken"
            className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2.5 font-mono text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
          />
          {error && <p className="mt-2 text-xs text-err">{error}</p>}
          <button
            type="submit"
            className="mt-4 w-full rounded-xl bg-emerald-600 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700"
          >
            连接
          </button>
          <p className="mt-3 text-xs leading-relaxed text-faint">
            令牌仅存于本机 localStorage，只经 WS 首帧 hello 加密通道发送，绝不进入
            URL。认证失败会返回本页。
          </p>
        </form>
      </div>
    </div>
  );
}
