import { useState, type FormEvent } from "react";
import { deviceSummary } from "../lib/protocol";

/**
 * 登录页（v1.2）：输入 USER_TOKEN（+可选设备名）→ 上层经 hello 换发 30 天
 * 会话凭证（登录成功即从 localStorage 删除 USER_TOKEN；旧 relay 无会话层则
 * 沿用 token 直连模式）。notice = 认证失败分流提示（会话过期/设备被吊销）。
 */
export function Login({
  notice,
  onSave,
}: {
  /** 认证失败分流提示（会话已过期 / 此设备已被吊销；空串不显示）。 */
  notice?: string;
  /** token = USER_TOKEN；deviceName 可选设备名（空串 → 上层用 UA 摘要缺省）。 */
  onSave: (token: string, deviceName: string) => void;
}) {
  const [value, setValue] = useState("");
  const [deviceName, setDeviceName] = useState("");
  const [error, setError] = useState("");
  // 缺省设备名展示（placeholder；jsdom navigator.userAgent 也可正常摘要）
  const uaSummary = deviceSummary(navigator.userAgent);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const token = value.trim();
    if (!token) {
      setError("请输入访问令牌");
      return;
    }
    onSave(token, deviceName.trim());
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
          {notice && (
            <p
              role="alert"
              className="mb-3 rounded-lg border border-amber-400/60 bg-amber-50 px-3 py-2 text-xs text-amber-800 dark:border-amber-500/40 dark:bg-amber-950/60 dark:text-amber-200"
            >
              {notice}
            </p>
          )}
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
          <label htmlFor="device" className="mb-2 mt-4 block text-sm font-medium text-ink">
            设备名（可选）
          </label>
          <input
            id="device"
            type="text"
            value={deviceName}
            onChange={(e) => setDeviceName(e.target.value)}
            placeholder={`缺省 ${uaSummary}`}
            className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2.5 text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
          />
          {error && <p className="mt-2 text-xs text-err">{error}</p>}
          <button
            type="submit"
            className="press mt-4 w-full rounded-xl bg-emerald-600 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700"
          >
            连接
          </button>
          <p className="mt-3 text-xs leading-relaxed text-faint">
            令牌仅用于首次换取 30 天会话凭证（成功后即从本机删除）；凭证只经 WS
            首帧 hello 加密通道发送，绝不进入 URL。认证失败会返回本页。
          </p>
        </form>
      </div>
    </div>
  );
}
