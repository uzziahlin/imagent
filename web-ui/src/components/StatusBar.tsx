import type { ConnStatus } from "../lib/ws";
import { convDisplayName } from "../lib/protocol";

const STATUS_META: Record<ConnStatus, { label: string; dot: string }> = {
  // 在线点呼吸（breathe 2.2s 缓慢缩放）：仅 ready 健康 steady 态使用；
  // connecting 保留脉冲（进行中反馈），reconnecting/superseded 静止不呼吸。
  connecting: { label: "连接中", dot: "bg-amber-400 animate-pulse" },
  ready: { label: "已连接", dot: "bg-emerald-400 animate-breathe" },
  reconnecting: { label: "重连中", dot: "bg-rose-400" },
  // 被同 token 新窗口顶替：醒目但非错误色（violet，区别于 amber 连接中 / rose 重连中）
  superseded: { label: "已在其它窗口连接", dot: "bg-violet-400" },
};

/**
 * 顶部状态条：relay 连接态 + 机器在线态 + 当前 conv + 退出。
 * 机器在线态：多机模式（v1.1）显示当前选中机器名 + machines 列表有无该机器
 * （App 侧换算后传入 machineOnline）；单机模式沿用旧网关 machine 帧
 * （machineName 缺省显示「Mac」）。移动端左上角 ☰ 打开会话抽屉（桌面端侧
 * 边栏常驻，按钮隐藏）。
 * v1.4 secure：当前机器 e2e 会话已激活 → 锁形标记「已加密」；false/缺省
 * 不显示（旧网关/未握手机器）。
 */
export function StatusBar({
  status,
  machineOnline,
  machineName,
  currentConv,
  secure,
  onLogout,
  onReconnect,
  onOpenDrawer,
}: {
  status: ConnStatus;
  machineOnline: boolean;
  /** 多机模式 = 当前机器 id；缺省（旧 relay 单机）显示「Mac」。 */
  machineName?: string;
  currentConv: string;
  /** v1.4：当前机器业务帧走端到端加密（lock 标记）。 */
  secure?: boolean;
  onLogout: () => void;
  /** superseded 态用户显式重连（新连接会顶替另一窗口，是用户意图）。 */
  onReconnect: () => void;
  /** 打开移动端会话抽屉（<768px 显示）。 */
  onOpenDrawer: () => void;
}) {
  const meta = STATUS_META[status];
  return (
    <header className="flex items-center gap-2 border-b border-line bg-panel px-2 py-2 backdrop-blur md:gap-3 md:px-3">
      <button
        type="button"
        onClick={onOpenDrawer}
        aria-label="打开会话列表"
        className="press rounded-lg p-1.5 text-muted transition-colors hover:bg-panel2 hover:text-ink md:hidden"
      >
        <svg viewBox="0 0 20 20" className="h-5 w-5" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
          <line x1="3" y1="5" x2="17" y2="5" strokeLinecap="round" />
          <line x1="3" y1="10" x2="17" y2="10" strokeLinecap="round" />
          <line x1="3" y1="15" x2="17" y2="15" strokeLinecap="round" />
        </svg>
      </button>
      <div className="flex min-w-0 items-center gap-1.5 text-xs text-muted">
        <span className={`h-2 w-2 shrink-0 rounded-full ${meta.dot}`} aria-hidden="true" />
        <span className="truncate">{meta.label}</span>
        {status === "superseded" && (
          <>
            <span className="hidden text-faint sm:inline">如需在此窗口使用</span>
            <button
              onClick={onReconnect}
              className="press shrink-0 rounded-lg border border-violet-400/60 bg-violet-500/10 px-2 py-0.5 text-xs text-think transition-colors hover:bg-violet-500/20 hover:text-ink dark:border-violet-500/40 dark:text-violet-300"
            >
              重新连接
            </button>
          </>
        )}
      </div>
      <div className="hidden items-center gap-1.5 text-xs sm:flex">
        <span
          className={`h-2 w-2 rounded-full ${
            machineOnline ? "bg-emerald-400 animate-breathe" : "bg-slate-500"
          }`}
          aria-hidden="true"
        />
        <span className={`max-w-[16vw] truncate ${machineOnline ? "text-muted" : "text-ghost"}`}>
          {machineName ?? "Mac"} {machineOnline ? "在线" : "离线"}
        </span>
        {secure && (
          <span
            className="flex shrink-0 items-center gap-0.5 text-emerald-600 dark:text-emerald-300"
            title="与该机器的业务帧已端到端加密，中继不可见"
            aria-label="已加密"
          >
            <svg viewBox="0 0 20 20" className="h-3.5 w-3.5" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
              <rect x="4.5" y="8.5" width="11" height="7.5" rx="2" />
              <path d="M7 8.5V6.5a3 3 0 0 1 6 0v2" strokeLinecap="round" />
            </svg>
            已加密
          </span>
        )}
      </div>
      <div className="ml-auto flex shrink-0 items-center gap-2">
        <span className="max-w-[30vw] truncate rounded-full bg-panel2 px-2.5 py-0.5 font-mono text-xs text-muted">
          {convDisplayName(currentConv)}
        </span>
        <button
          onClick={onLogout}
          className="press rounded-lg px-2 py-1 text-xs text-muted transition-colors hover:bg-panel2 hover:text-ink"
        >
          退出
        </button>
      </div>
    </header>
  );
}
