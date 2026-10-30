import { useState, type FormEvent } from "react";
import { deviceSummary } from "../lib/protocol";

/**
 * 登录页（v1.3 双入口 + v1.5 配对入口）：「使用通行钥匙」（passkey，默认
 * 高亮）+「使用访问令牌」（USER_TOKEN，恢复路径完整保留）+「我是新设备」
 * （配对：hello{pairing} 无凭证，由已登录设备批准进入）。passkey 流程
 * （begin → credentials.get → complete → 存 session）与配对流（PairingWait
 * 等待屏 + WS）由上层 App 驱动，本组件只管入口切换与提示（notice = 认证
 * 失败分流提示，passkeyError = 通行钥匙失败回落提示）。WebAuthn 不可用
 * （旧浏览器/私有模式/非安全上下文）→ 隐藏通行钥匙入口、直接令牌形态。
 */
export function Login({
  notice,
  webauthnAvailable,
  passkeyBusy,
  passkeyError,
  onPasskeyLogin,
  onSave,
  onPairing,
}: {
  /** 认证失败分流提示（会话已过期 / 此设备已被吊销；空串不显示）。 */
  notice?: string;
  /** 浏览器是否支持 WebAuthn（false → 隐藏通行钥匙入口）。 */
  webauthnAvailable: boolean;
  /** passkey ceremony 进行中（按钮禁用 + 「验证中…」）。 */
  passkeyBusy: boolean;
  /** passkey 失败提示（用户取消/无凭证/网络错——引导改用访问令牌）。 */
  passkeyError: string | null;
  onPasskeyLogin: () => void;
  /** token = USER_TOKEN；deviceName 可选设备名（空串 → 上层用 UA 摘要缺省）。 */
  onSave: (token: string, deviceName: string) => void;
  /** v1.5 新设备配对入口（deviceName 空串 → 上层用 UA 摘要缺省）。 */
  onPairing: (deviceName: string) => void;
}) {
  // 缺省入口：支持 WebAuthn 时通行钥匙优先（高亮主入口）。
  const [mode, setMode] = useState<"passkey" | "token" | "pairing">(
    webauthnAvailable ? "passkey" : "token",
  );
  const [value, setValue] = useState("");
  const [deviceName, setDeviceName] = useState("");
  const [pairingDevice, setPairingDevice] = useState("");
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

  const submitPairing = (e: FormEvent) => {
    e.preventDefault();
    onPairing(pairingDevice.trim());
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
          <p className="mt-1 text-sm text-muted">连接你的 Mac 网关</p>
        </div>
        <div className="rounded-2xl border border-line bg-panel p-5 shadow-xl">
          {notice && (
            <p
              role="alert"
              className="mb-3 rounded-lg border border-amber-400/60 bg-amber-50 px-3 py-2 text-xs text-amber-800 dark:border-amber-500/40 dark:bg-amber-950/60 dark:text-amber-200"
            >
              {notice}
            </p>
          )}
          {mode === "pairing" ? (
            /* v1.5 新设备配对：无凭证 hello，由已登录设备批准（App 侧接管等待屏） */
            <form onSubmit={submitPairing}>
              <p className="mb-4 text-sm leading-relaxed text-muted">
                无访问令牌？在任一已登录设备上批准后，本设备可直接进入（需该设备通过通行钥匙验证）。
              </p>
              <label htmlFor="pairing-device" className="mb-2 block text-sm font-medium text-ink">
                设备名（可选）
              </label>
              <input
                id="pairing-device"
                type="text"
                value={pairingDevice}
                onChange={(e) => setPairingDevice(e.target.value)}
                placeholder={`缺省 ${uaSummary}`}
                className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2.5 text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
              />
              <button
                type="submit"
                className="press mt-4 w-full rounded-xl bg-sky-600 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-sky-500 active:bg-sky-700"
              >
                开始配对
              </button>
              <button
                type="button"
                onClick={() => setMode(webauthnAvailable ? "passkey" : "token")}
                className="press mt-2 w-full rounded-xl border border-line-strong py-2 text-sm text-muted transition-colors hover:text-ink"
              >
                返回登录
              </button>
            </form>
          ) : (
            <>
              {/* 双入口切换（WebAuthn 可用才有 passkey 页签；当前项高亮） */}
              {webauthnAvailable && (
                <div className="mb-4 grid grid-cols-2 gap-1 rounded-xl bg-surface p-1" role="tablist" aria-label="登录方式">
                  {(
                    [
                      ["passkey", "使用通行钥匙"],
                      ["token", "使用访问令牌"],
                    ] as const
                  ).map(([m, label]) => (
                    <button
                      key={m}
                      type="button"
                      role="tab"
                      aria-selected={mode === m}
                      onClick={() => setMode(m)}
                      className={
                        "press rounded-lg px-3 py-1.5 text-xs font-medium transition-colors " +
                        (mode === m
                          ? "bg-emerald-600 text-white"
                          : "text-muted hover:bg-panel2 hover:text-ink")
                      }
                    >
                      {label}
                    </button>
                  ))}
                </div>
              )}

              {mode === "passkey" && webauthnAvailable ? (
                <div>
                  <p className="mb-4 text-sm leading-relaxed text-muted">
                    使用本机的面容 ID / 指纹 / 安全密钥一步登录，无需输入令牌。
                    通行钥匙需先在「已登录设备」面板注册。
                  </p>
                  <button
                    type="button"
                    onClick={onPasskeyLogin}
                    disabled={passkeyBusy}
                    className="press w-full rounded-xl bg-emerald-600 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700 disabled:opacity-60"
                  >
                    {passkeyBusy ? "验证中…" : "使用通行钥匙登录"}
                  </button>
                  {passkeyError && (
                    <p role="alert" className="mt-3 text-xs text-err">
                      {passkeyError}——可改用
                      <button
                        type="button"
                        onClick={() => setMode("token")}
                        className="mx-0.5 underline underline-offset-2"
                      >
                        访问令牌
                      </button>
                      登录
                    </p>
                  )}
                  <p className="mt-3 text-xs leading-relaxed text-faint">
                    凭证只存本机（服务器仅存公钥）；登录换取 30 天会话凭证。本地
                    http 直连（非 localhost）不支持通行钥匙，请用访问令牌。
                  </p>
                </div>
              ) : (
                <form onSubmit={submit}>
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
                    令牌仅用于首次换取 30 天会话凭证（成功后即从本机删除）；凭证只经
                    WS 首帧 hello 加密通道发送，绝不进入 URL。认证失败会返回本页。
                  </p>
                </form>
              )}
              {/* v1.5 新设备配对入口（无令牌路径） */}
              <button
                type="button"
                onClick={() => setMode("pairing")}
                className="press mt-4 w-full text-center text-xs text-sky-700 underline-offset-2 transition-colors hover:text-sky-600 hover:underline dark:text-sky-300 dark:hover:text-sky-200"
              >
                我是新设备 · 配对进入
              </button>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
