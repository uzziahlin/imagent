import { normalizeExpiryMs } from "../lib/protocol";
import { countdownText, useNow } from "./useNow";

/**
 * 配对请求卡（v1.5 已登录侧）：relay 收到新设备 hello{pairing} 后向全部
 * 已认证会话广播 pairing_request——本卡渲染在消息区顶部，展示设备名 + 6 位
 * 码 + 倒计时，与请求设备屏幕上的大字码核对后允许/拒绝。两者均为管理级
 * 操作（须 passkey re-auth，App 侧 ensureAdminToken）；busy = 验证进行中。
 * expiresAt 缺席（容错）则不显示倒计时。
 */
export function PairingRequestCard({
  device,
  code,
  expiresAt,
  busy,
  onAllow,
  onDeny,
}: {
  device: string;
  code: string;
  expiresAt?: number;
  /** re-auth ceremony 进行中（按钮禁用 + 「验证通行钥匙…」）。 */
  busy: boolean;
  onAllow: () => void;
  onDeny: () => void;
}) {
  const now = useNow();
  const remain = expiresAt !== undefined ? normalizeExpiryMs(expiresAt) - now : null;
  const expired = remain !== null && remain <= 0;

  return (
    <div
      role="alertdialog"
      aria-label="新设备配对请求"
      className="pointer-events-auto mx-3 mt-3 rounded-xl border border-sky-400/60 bg-sky-50 px-3 py-2.5 shadow-lg dark:border-sky-500/40 dark:bg-sky-950/90"
    >
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <svg viewBox="0 0 20 20" className="h-5 w-5 shrink-0 text-sky-600 dark:text-sky-300" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
          <rect x="3" y="6" width="14" height="9" rx="2" />
          <path d="M7 15v1.5M13 15v1.5M6 9.5h8" strokeLinecap="round" />
        </svg>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-baseline gap-x-2">
            <span className="text-sm font-medium text-sky-900 dark:text-sky-100">新设备配对请求</span>
            <span className="min-w-0 truncate text-xs text-sky-800 dark:text-sky-200">{device}</span>
            <span
              className="select-all font-mono text-base font-bold tracking-[0.2em] text-sky-900 dark:text-sky-100"
              aria-label="配对请求码"
            >
              {code}
            </span>
            {remain !== null && (
              <span className={`text-[11px] ${expired ? "text-rose-600 dark:text-rose-300" : "text-sky-700 dark:text-sky-300"}`}>
                {expired ? "已过期" : countdownText(remain)}
              </span>
            )}
          </div>
          <p className="mt-0.5 text-[11px] leading-relaxed text-sky-800/80 dark:text-sky-200/80">
            请与该设备屏幕上的代码核对一致后允许（需通行钥匙验证）；拒绝将断开其等待连接。
          </p>
        </div>
        <div className="flex shrink-0 gap-1.5">
          <button
            type="button"
            onClick={onDeny}
            disabled={busy}
            className="press rounded-lg border border-line-strong px-2.5 py-1 text-xs text-muted transition-colors hover:border-rose-400/60 hover:text-err disabled:opacity-60"
          >
            拒绝
          </button>
          <button
            type="button"
            onClick={onAllow}
            disabled={busy}
            className="press rounded-lg bg-sky-600 px-2.5 py-1 text-xs font-semibold text-white transition-colors hover:bg-sky-500 disabled:opacity-60"
          >
            {busy ? "验证通行钥匙…" : "允许"}
          </button>
        </div>
      </div>
    </div>
  );
}
