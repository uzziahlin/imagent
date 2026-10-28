import { normalizeExpiryMs, type DeviceInfo } from "../lib/protocol";
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
 * 已登录设备弹层（v1.2）：列表（设备名 / 在线状态点 / 最后活跃相对时间）、
 * 每项「吊销」；id === selfId 的项标「本机」，其吊销按钮为「吊销并退出」
 * （App 侧语义：吊销本机 = 登出）。devices 为 null = 已发 device_list 待回包。
 *
 * 进出过渡（150ms ease）：open 经 useExitMounted 延迟卸载；遮罩 fade + 面板
 * fade/scale；关闭后 visibility 摘除可聚焦性（transition 离散翻转在动画末尾生效）。
 */
export function DeviceManager({
  open,
  devices,
  selfId,
  onRevoke,
  onClose,
}: {
  /** 弹层开关（App 恒挂载本组件，open 只切显隐）。 */
  open: boolean;
  /** null = 加载中（打开面板即发 device_list，relay 回 devices 帧更新）。 */
  devices: DeviceInfo[] | null;
  /** 本机会话 id（hello_ack.session.id）；与列表项 id 匹配标「本机」。 */
  selfId?: string;
  onRevoke: (device: DeviceInfo) => void;
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
        <p className="mt-3 text-[11px] leading-relaxed text-faint">
          吊销后该设备需重新输入访问令牌登录；吊销本机将退出当前登录。
        </p>
      </div>
    </div>
  );
}
