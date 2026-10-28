import { useMemo, useState, type FormEvent, type KeyboardEvent } from "react";
import { convDisplayName, DEFAULT_MACHINE } from "../lib/protocol";

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
 * 会话侧边栏（桌面常驻 / 移动抽屉共用）：标题 + Mac 在线状态点、机器切换器
 * （v1.1 多机模式）、会话列表（当前选中机器的会话池，当前项高亮，每项含
 * 名字/最后消息一行预览/时间）、底部「+ 新会话」。
 *
 * 交互语义：
 * - 点选会话 = 仅切换 SPA 本地当前 conv（不发包）；
 * - 点选机器 = 切换 currentMachine（App 侧把当前 conv 回到该机器默认会话，
 *   并发一次带 machine 的 sync 拉该机器卡片缓存）；
 * - 新会话 = 由上层发 `/switch <名字>` message 命令帧（convId 留空落默认
 *   会话），命令语义由网关解析，SPA 同时把当前 conv 切到 `web:<名字>`。
 * - 未收到任何 machines 帧（旧 relay）：不显示机器区，单机行为与升级前一致。
 *
 * 已知限制：会话内消息历史仅 SPA 内存保留，页面刷新后仅当前卡片由 sync
 * 重发恢复（text/user 回显不重放）。
 */
export function Sidebar({
  convs,
  current,
  machineOnline,
  machines,
  machinesKnown,
  currentMachine,
  onSelect,
  onSelectMachine,
  onCreate,
}: {
  convs: ConvItem[];
  current: string;
  /** 旧网关 machine 帧在线态（仅单机模式显示）。 */
  machineOnline: boolean;
  /** 在线机器全量列表（machines 帧）。 */
  machines: string[];
  /** 收到过 machines 帧 = 多机模式（显示机器切换器）。 */
  machinesKnown: boolean;
  currentMachine: string;
  onSelect: (conv: string) => void;
  /** 机器切换器点选（切 currentMachine + 触发该机器 sync）。 */
  onSelectMachine: (machine: string) => void;
  /** 发 /switch 并切换当前 conv；返回 false（未连接等）则留在输入态。 */
  onCreate: (name: string) => boolean;
}) {
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");

  // 机器行：在线列表 + 当前选中机器掉线时补一行灰点（保持可见可选，帧会回
  // machine_offline toast 提示原因）。default 不是真实机器，不补。
  const machineRows = useMemo(() => {
    const rows = machines.map((id) => ({ id, online: true }));
    if (
      machinesKnown &&
      currentMachine !== DEFAULT_MACHINE &&
      !machines.includes(currentMachine)
    ) {
      rows.push({ id: currentMachine, online: false });
    }
    return rows;
  }, [machines, machinesKnown, currentMachine]);

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
        {machinesKnown ? (
          <span className="ml-auto text-[11px] text-ghost">{machines.length} 台在线</span>
        ) : (
          <>
            <span
              className={`h-2 w-2 rounded-full ${machineOnline ? "bg-emerald-400" : "bg-slate-500"}`}
              title={`Mac ${machineOnline ? "在线" : "离线"}`}
              aria-label={`Mac ${machineOnline ? "在线" : "离线"}`}
            />
            <span className={`text-xs ${machineOnline ? "text-muted" : "text-ghost"}`}>
              Mac {machineOnline ? "在线" : "离线"}
            </span>
          </>
        )}
      </div>

      {/* v1.1 机器切换器：仅多机模式（收到过 machines 帧）显示 */}
      {machinesKnown && (
        <div className="border-b border-line p-2">
          <div className="px-2 pb-1 text-[11px] font-medium uppercase tracking-wider text-ghost">
            机器
          </div>
          <div className="space-y-0.5" role="listbox" aria-label="机器列表">
            {machineRows.length === 0 && (
              <div className="px-2.5 py-1.5 text-xs text-ghost">（无在线机器）</div>
            )}
            {machineRows.map((m) => {
              const active = m.id === currentMachine;
              return (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => onSelectMachine(m.id)}
                  aria-selected={active}
                  className={
                    "flex w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left transition-colors " +
                    (active
                      ? "bg-emerald-600/15 text-ink"
                      : "text-muted hover:bg-panel2 hover:text-ink")
                  }
                >
                  <span
                    className={
                      "h-2 w-2 shrink-0 rounded-full " +
                      (m.online ? "bg-emerald-400" : "bg-slate-500")
                    }
                    aria-hidden="true"
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-xs font-medium">
                    {m.id}
                  </span>
                  {!m.online && (
                    <span className="shrink-0 text-[10px] text-ghost">离线</span>
                  )}
                </button>
              );
            })}
          </div>
        </div>
      )}

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
            {/* text-base（16px）：防 iOS Safari 聚焦 <16px 输入框时自动放大页面 */}
            <input
              autoFocus
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder="会话名（如 work）"
              aria-label="新会话名称"
              className="w-full rounded-lg border border-line-strong bg-surface px-2.5 py-1.5 text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
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
