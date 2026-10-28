import type { ConnStatus } from "../lib/ws";
import { convDisplayName } from "../lib/protocol";

const STATUS_META: Record<ConnStatus, { label: string; dot: string }> = {
  connecting: { label: "连接中", dot: "bg-amber-400 animate-pulse" },
  ready: { label: "已连接", dot: "bg-emerald-400" },
  reconnecting: { label: "重连中", dot: "bg-rose-400 animate-pulse" },
  // 被同 token 新窗口顶替：醒目但非错误色（violet，区别于 amber 连接中 / rose 重连中）
  superseded: { label: "已在其它窗口连接", dot: "bg-violet-400" },
};

/** 顶部状态条：relay 连接态 + Mac 在线态（machine 帧）+ 当前 conv + 退出。 */
export function StatusBar({
  status,
  machineOnline,
  currentConv,
  onLogout,
  onReconnect,
}: {
  status: ConnStatus;
  machineOnline: boolean;
  currentConv: string;
  onLogout: () => void;
  /** superseded 态用户显式重连（新连接会顶替另一窗口，是用户意图）。 */
  onReconnect: () => void;
}) {
  const meta = STATUS_META[status];
  return (
    <header className="flex items-center gap-3 border-b border-slate-800 bg-slate-900/80 px-3 py-2 backdrop-blur">
      <div className="flex items-center gap-1.5 text-xs text-slate-300">
        <span className={`h-2 w-2 rounded-full ${meta.dot}`} aria-hidden="true" />
        <span>{meta.label}</span>
        {status === "superseded" && (
          <>
            <span className="text-slate-500">如需在此窗口使用</span>
            <button
              onClick={onReconnect}
              className="rounded-lg border border-violet-500/40 bg-violet-500/10 px-2 py-0.5 text-xs text-violet-300 transition-colors hover:bg-violet-500/20 hover:text-violet-100"
            >
              重新连接
            </button>
          </>
        )}
      </div>
      <div className="flex items-center gap-1.5 text-xs">
        <span
          className={`h-2 w-2 rounded-full ${machineOnline ? "bg-emerald-400" : "bg-slate-600"}`}
          aria-hidden="true"
        />
        <span className={machineOnline ? "text-slate-300" : "text-slate-500"}>
          Mac {machineOnline ? "在线" : "离线"}
        </span>
      </div>
      <div className="ml-auto flex items-center gap-2">
        <span className="max-w-[30vw] truncate rounded-full bg-slate-800 px-2.5 py-0.5 font-mono text-xs text-slate-400">
          {convDisplayName(currentConv)}
        </span>
        <button
          onClick={onLogout}
          className="rounded-lg px-2 py-1 text-xs text-slate-400 transition-colors hover:bg-slate-800 hover:text-slate-200"
        >
          退出
        </button>
      </div>
    </header>
  );
}
