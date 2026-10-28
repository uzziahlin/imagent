import { useState, type FormEvent, type KeyboardEvent } from "react";
import { convDisplayName } from "../lib/protocol";

/** 侧边栏会话项数据（App 从 state 派生）：conv 全名 + 最后消息预览/时间。 */
export type ConvItem = {
  convId: string;
  preview: string;
  at: number;
};

/** 会话项时间：当天显示 HH:mm，否则 M/D HH:mm。 */
function formatTime(at: number): string {
  if (!at) {
    return "";
  }
  const d = new Date(at);
  const hm = d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
  if (d.toDateString() === new Date().toDateString()) {
    return hm;
  }
  return `${d.getMonth() + 1}/${d.getDate()} ${hm}`;
}

/**
 * 会话侧边栏（桌面常驻 / 移动抽屉共用）：标题 + Mac 在线状态点、会话列表
 * （当前项高亮，每项含名字/最后消息一行预览/时间）、底部「+ 新会话」。
 *
 * 交互语义：
 * - 点选会话 = 仅切换 SPA 本地当前 conv（不发包）；
 * - 新会话 = 由上层发 `/switch <名字>` message 命令帧（convId 留空落默认
 *   会话），命令语义由网关解析，SPA 同时把当前 conv 切到 `web:<名字>`。
 *
 * 已知限制：会话内消息历史仅 SPA 内存保留，页面刷新后仅当前卡片由 sync
 * 重发恢复（text/user 回显不重放）。
 */
export function Sidebar({
  convs,
  current,
  machineOnline,
  onSelect,
  onCreate,
}: {
  convs: ConvItem[];
  current: string;
  machineOnline: boolean;
  onSelect: (conv: string) => void;
  /** 发 /switch 并切换当前 conv；返回 false（未连接等）则留在输入态。 */
  onCreate: (name: string) => boolean;
}) {
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const n = name.trim();
    if (!n) {
      return;
    }
    if (onCreate(n)) {
      setName("");
      setCreating(false);
    }
  };

  const cancel = () => {
    setName("");
    setCreating(false);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") {
      cancel();
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-4 py-3">
        <span className="text-sm font-semibold tracking-wide text-ink">imagent</span>
        <span
          className={`h-2 w-2 rounded-full ${machineOnline ? "bg-emerald-400" : "bg-slate-500"}`}
          title={`Mac ${machineOnline ? "在线" : "离线"}`}
          aria-label={`Mac ${machineOnline ? "在线" : "离线"}`}
        />
        <span className={`text-xs ${machineOnline ? "text-muted" : "text-ghost"}`}>
          Mac {machineOnline ? "在线" : "离线"}
        </span>
      </div>

      <nav className="min-h-0 flex-1 space-y-1 overflow-y-auto overscroll-contain p-2" aria-label="会话列表">
        {convs.map((item) => {
          const active = item.convId === current;
          return (
            <button
              key={item.convId}
              type="button"
              onClick={() => onSelect(item.convId)}
              aria-current={active ? "page" : undefined}
              className={
                "block w-full rounded-xl px-3 py-2 text-left transition-colors " +
                (active
                  ? "bg-emerald-600/15 text-ink"
                  : "text-muted hover:bg-panel2 hover:text-ink")
              }
            >
              <span className="flex items-center gap-2">
                <span className="min-w-0 flex-1 truncate text-sm font-medium">
                  {convDisplayName(item.convId)}
                </span>
                {active && (
                  <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-emerald-500" aria-hidden="true" />
                )}
              </span>
              <span className="mt-0.5 flex items-baseline gap-2">
                <span className="min-w-0 flex-1 truncate text-xs text-faint">
                  {item.preview || "（暂无消息）"}
                </span>
                <span className="shrink-0 text-[11px] leading-4 text-ghost">
                  {formatTime(item.at)}
                </span>
              </span>
            </button>
          );
        })}
      </nav>

      <div className="border-t border-line p-2">
        {creating ? (
          <form onSubmit={submit} className="space-y-1.5">
            <input
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder="会话名（如 work）"
              aria-label="新会话名称"
              className="w-full rounded-lg border border-line-strong bg-surface px-2.5 py-1.5 text-sm text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
            />
            <div className="flex gap-1.5">
              <button
                type="submit"
                disabled={!name.trim()}
                className="flex-1 rounded-lg bg-emerald-600 px-2 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-emerald-500 disabled:opacity-40"
              >
                创建（/switch）
              </button>
              <button
                type="button"
                onClick={cancel}
                className="rounded-lg border border-line-strong px-2.5 py-1.5 text-xs text-muted transition-colors hover:text-ink"
              >
                取消
              </button>
            </div>
          </form>
        ) : (
          <button
            type="button"
            onClick={() => setCreating(true)}
            className="w-full rounded-lg border border-dashed border-line-strong px-3 py-2 text-sm text-muted transition-colors hover:border-emerald-500/60 hover:text-ink"
          >
            + 新会话
          </button>
        )}
      </div>
    </div>
  );
}
