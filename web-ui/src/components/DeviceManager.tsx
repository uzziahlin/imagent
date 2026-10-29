import { normalizeExpiryMs, type DeviceInfo, type PasskeyInfo } from "../lib/protocol";
import { fingerprint, type E2ePin } from "../lib/e2e";
import { useExitMounted } from "./useExitMounted";

/** 相对时间（v1.2 设备列表「最后活跃」）：刚刚 / N 分钟前 / N 小时前 / N 天前。
 *  ts 容错秒级 epoch（与 expiresAt 同一归一）；缺席给占位符。 */
export function relativeTime(ts: number | undefined, now = Date.now()): string {
  if (ts === undefined) {
    return "—";
  }
  const diff = now - normalizeExpiryMs(ts);
  if (diff < 60_000) {
    return "刚刚";
  }
  if (diff < 3_600_000) {
    return `${Math.floor(diff / 60_000)} 分钟前`;
  }
  if (diff < 86_400_000) {
    return `${Math.floor(diff / 3_600_000)} 小时前`;
  }
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

/**
 * 已登录设备弹层（v1.2 + v1.3 通行钥匙 + v1.4 机器指纹）：设备列表（设备名 /
 * 在线状态点 / 最后活跃相对时间、每项「吊销」；id === selfId 的项标「本机」，
 * 其吊销按钮为「吊销并退出」——App 侧语义：吊销本机 = 登出）+ 通行钥匙区
 * （passkey 列表 label/created、每项「删除」、「添加通行钥匙」按钮走注册流程，
 * label 用本机设备名）+ 已信任机器指纹区（v1.4 TOFU pin 表：机器名 / 双指纹
 * 分组展示 / 信任时间、每项「重置」→ 该机器下次连接重新走指纹确认）。devices
 * / passkeys 为 null = 已发列表帧待回包。
 *
 * 进出过渡（150ms ease）：open 经 useExitMounted 延迟卸载；遮罩 fade + 面板
 * fade/scale；关闭后 visibility 摘除可聚焦性（transition 离散翻转在动画末尾生效）。
 */
export function DeviceManager({
  open,
  devices,
  passkeys,
  pins,
  webauthnAvailable,
  passkeyBusy,
  passkeyMessage,
  selfId,
  onRevoke,
  onAddPasskey,
  onRevokePasskey,
  onResetPin,
  onClose,
}: {
  /** 弹层开关（App 恒挂载本组件，open 只切显隐）。 */
  open: boolean;
  /** null = 加载中（打开面板即发 device_list，relay 回 devices 帧更新）。 */
  devices: DeviceInfo[] | null;
  /** null = 加载中（打开面板即发 cred_list，relay 回 creds 帧更新）。 */
  passkeys: PasskeyInfo[] | null;
  /** v1.4 已信任机器指纹（TOFU pin 表快照；空表 = 尚无信任机器）。 */
  pins: E2ePin[];
  /** 本机浏览器是否支持 WebAuthn（不支持则通行钥匙区只显示指引）。 */
  webauthnAvailable: boolean;
  /** 注册 ceremony 进行中（「添加通行钥匙」禁用 + 等待系统弹窗）。 */
  passkeyBusy: boolean;
  /** 注册/删除的结果提示（成功/失败一行文案；null 不显示）。 */
  passkeyMessage: string | null;
  /** 本机会话 id（hello_ack.session.id）；与列表项 id 匹配标「本机」。 */
  selfId?: string;
  onRevoke: (device: DeviceInfo) => void;
  /** 「添加通行钥匙」：App 侧走 lib/webauthn 注册流程。 */
  onAddPasskey: () => void;
  onRevokePasskey: (passkey: PasskeyInfo) => void;
  /** v1.4 重置机器 pin（重置后该机器重新走 TOFU 指纹确认）。 */
  onResetPin: (machine: string) => void;
  onClose: () => void;
}) {
  const mounted = useExitMounted(open, 150);
  if (!open && !mounted) {
    return null;
  }
  return (
    <div
      className="fixed inset-0 z-50"
      role={open ? "dialog" : undefined}
      aria-modal={open ? "true" : undefined}
      aria-label={open ? "已登录设备" : undefined}
    >
      <div
        className={`absolute inset-0 bg-black/50 transition-opacity duration-150 ease-out ${
          open ? "opacity-100" : "opacity-0"
        }`}
        onClick={onClose}
        aria-hidden="true"
      />
      <div
        className={`absolute left-1/2 top-1/2 w-[calc(100%-2rem)] max-w-sm -translate-x-1/2 -translate-y-1/2 rounded-2xl border border-line bg-panel p-4 shadow-2xl transition-[opacity,transform,visibility] duration-150 ease-out ${
          open ? "visible scale-100 opacity-100" : "invisible scale-95 opacity-0"
        }`}
      >
        <div className="mb-3 flex items-center">
          <h2 className="text-sm font-semibold text-ink">已登录设备</h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="关闭设备管理"
            className="press ml-auto rounded-lg p-1.5 text-muted transition-colors hover:bg-panel2 hover:text-ink"
          >
            <svg viewBox="0 0 20 20" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
              <line x1="5" y1="5" x2="15" y2="15" strokeLinecap="round" />
              <line x1="15" y1="5" x2="5" y2="15" strokeLinecap="round" />
            </svg>
          </button>
        </div>
        {devices === null ? (
          <p className="py-6 text-center text-xs text-ghost">加载中…</p>
        ) : devices.length === 0 ? (
          <p className="py-6 text-center text-xs text-ghost">暂无已登录设备</p>
        ) : (
          <ul className="max-h-[60vh] space-y-1 overflow-y-auto overscroll-contain" aria-label="设备列表">
            {devices.map((d) => {
              const self = d.id === selfId;
              return (
                <li
                  key={d.id}
                  className="flex items-center gap-2.5 rounded-xl border border-line bg-surface px-3 py-2"
                >
                  <span
                    className={`h-2 w-2 shrink-0 rounded-full ${
                      d.connected ? "bg-emerald-400" : "bg-slate-500"
                    }`}
                    title={d.connected ? "在线" : "离线"}
                    aria-label={d.connected ? "在线" : "离线"}
                  />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-1.5">
                      <span className="min-w-0 truncate text-sm text-ink">{d.name}</span>
                      {self && (
                        <span className="shrink-0 rounded bg-emerald-600/15 px-1 py-px text-[10px] font-medium text-emerald-700 dark:text-emerald-300">
                          本机
                        </span>
                      )}
                    </div>
                    <div className="text-[11px] text-ghost">最后活跃 {relativeTime(d.lastSeen)}</div>
                  </div>
                  <button
                    type="button"
                    onClick={() => onRevoke(d)}
                    className={
                      "press shrink-0 rounded-lg border px-2 py-1 text-xs transition-colors " +
                      (self
                        ? "border-rose-400/70 text-rose-600 hover:bg-rose-500/10 dark:border-rose-500/50 dark:text-rose-300 dark:hover:bg-rose-500/15"
                        : "border-line-strong text-muted hover:border-rose-400/60 hover:text-rose-600 dark:hover:border-rose-500/50 dark:hover:text-rose-300")
                    }
                  >
                    {self ? "吊销并退出" : "吊销"}
                  </button>
                </li>
              );
            })}
          </ul>
        )}
        {/* v1.3 通行钥匙区：列表/删除 + 添加（注册 label 用设备名）。 */}
        <div className="mt-4 border-t border-line pt-3">
          <div className="mb-2 flex items-center">
            <h3 className="text-xs font-semibold uppercase tracking-wider text-ghost">通行钥匙</h3>
            {webauthnAvailable && (
              <button
                type="button"
                onClick={onAddPasskey}
                disabled={passkeyBusy}
                className="press ml-auto rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-emerald-500/60 hover:text-ink disabled:opacity-60"
              >
                {passkeyBusy ? "等待系统弹窗…" : "添加通行钥匙"}
              </button>
            )}
          </div>
          {!webauthnAvailable ? (
            <p className="py-1 text-[11px] leading-relaxed text-ghost">
              本浏览器不支持通行钥匙（或非安全上下文），登录请使用访问令牌。
            </p>
          ) : passkeys === null ? (
            <p className="py-1 text-center text-xs text-ghost">加载中…</p>
          ) : passkeys.length === 0 ? (
            <p className="py-1 text-[11px] leading-relaxed text-ghost">
              尚未注册通行钥匙。添加后可在登录页免令牌一步登录。
            </p>
          ) : (
            <ul className="max-h-40 space-y-1 overflow-y-auto overscroll-contain" aria-label="通行钥匙列表">
              {passkeys.map((p) => (
                <li
                  key={p.id}
                  className="flex items-center gap-2.5 rounded-xl border border-line bg-surface px-3 py-2"
                >
                  <svg viewBox="0 0 20 20" className="h-4 w-4 shrink-0 text-emerald-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                    <circle cx="7" cy="10" r="3.2" />
                    <path d="M10.2 10h6.3M14 10v2.2M16.5 10v1.6" strokeLinecap="round" />
                  </svg>
                  <div className="min-w-0 flex-1">
                    <div className="min-w-0 truncate text-sm text-ink">{p.label}</div>
                    <div className="text-[11px] text-ghost">注册于 {relativeTime(p.created)}</div>
                  </div>
                  <button
                    type="button"
                    onClick={() => onRevokePasskey(p)}
                    className="press shrink-0 rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-rose-400/60 hover:text-rose-600 dark:hover:border-rose-500/50 dark:hover:text-rose-300"
                  >
                    删除
                  </button>
                </li>
              ))}
            </ul>
          )}
          {passkeyMessage && <p className="mt-2 text-[11px] leading-relaxed text-faint">{passkeyMessage}</p>}
        </div>
        {/* v1.4 已信任机器指纹（TOFU pin 表）：列表 + 重置（重置后重新走
            首连指纹确认；身份变化告警的恢复路径）。 */}
        <div className="mt-4 border-t border-line pt-3">
          <h3 className="mb-2 text-xs font-semibold uppercase tracking-wider text-ghost">已信任机器指纹</h3>
          {pins.length === 0 ? (
            <p className="py-1 text-[11px] leading-relaxed text-ghost">
              尚未信任任何机器。首次加密连接时会请求确认机器指纹。
            </p>
          ) : (
            <ul className="max-h-44 space-y-1 overflow-y-auto overscroll-contain" aria-label="已信任机器指纹列表">
              {pins.map((p) => (
                <li
                  key={p.machine}
                  className="rounded-xl border border-line bg-surface px-3 py-2"
                >
                  <div className="flex items-center gap-2">
                    <svg viewBox="0 0 20 20" className="h-4 w-4 shrink-0 text-emerald-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                      <rect x="4.5" y="8.5" width="11" height="7.5" rx="2" />
                      <path d="M7 8.5V6.5a3 3 0 0 1 6 0v2" strokeLinecap="round" />
                    </svg>
                    <span className="min-w-0 flex-1 truncate text-sm text-ink">{p.machine}</span>
                    <span className="shrink-0 text-[11px] text-ghost">{relativeTime(p.pinnedAt)}</span>
                    <button
                      type="button"
                      onClick={() => onResetPin(p.machine)}
                      className="press shrink-0 rounded-lg border border-line-strong px-2 py-1 text-xs text-muted transition-colors hover:border-rose-400/60 hover:text-rose-600 dark:hover:border-rose-500/50 dark:hover:text-rose-300"
                    >
                      重置
                    </button>
                  </div>
                  <div className="mt-1 space-y-0.5 font-mono text-[11px] tracking-wider text-ghost">
                    <div aria-label={`${p.machine} 网关密钥指纹`}>X25519 {fingerprint(p.gk)}</div>
                    <div aria-label={`${p.machine} 身份密钥指纹`}>Ed25519 {fingerprint(p.ik)}</div>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
        <p className="mt-3 text-[11px] leading-relaxed text-faint">
          吊销后该设备需重新输入访问令牌登录；吊销本机将退出当前登录。删除通行钥匙后该设备无法再免令牌登录。
        </p>
      </div>
    </div>
  );
}
