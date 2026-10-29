import { useMemo } from "react";
import { convDisplayName, DEFAULT_MACHINE, normalizeExpiryMs } from "../lib/protocol";

/** 侧边栏会话项数据（App 从 state 派生）：conv 全名 + 显示名 + 最后消息
 *  预览/时间 + 未读计数（非当前 conv 收到新消息时 reducer 累加，点选清零）。 */
export type ConvItem = {
  convId: string;
  /** 会话显示名（App 经 convTitle 派生传入：自动会话 web:s-xxxx 用首条消息
   *  派生的自动 title，手动名/default/local 恒 convId 剥前缀）。 */
  title?: string;
  preview: string;
  at: number;
  /** 未读条数（0 = 无徽标）。 */
  unread?: number;
};

/** 会话过期横幅阈值：剩余不足 3 天开始提示。 */
const EXPIRY_WARN_DAYS = 3;

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
 * - 新会话（v1.6）= 打开 App 级弹层（会话名 + 工作目录选择 WorkspacePicker），
 *   确认后由上层发 `/switch <名字>` message 命令帧（convId 留空落默认会话）
 *   并按需绑定目录（conv_workdir_set），SPA 同时把当前 conv 切到 `web:<名字>`。
 *   名字留空时上层落 s-xxxx 自动短 ID（协议零改动），会话项显示名由首条消息
 *   派生（title，见 ConvItem）。
 * - 未收到任何 machines 帧（旧 relay）：不显示机器区，单机行为与升级前一致。
 * - v1.2 会话层：底部「管理」入口（仅会话模式；v1.5 起为管理面板——机器/
 *   令牌/设备/钥匙/指纹）与「会话 N 天后过期」续期横幅（剩余 < 3 天；点击
 *   进入登出流程）。
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
  sessionExpiresAt,
  onRenewSession,
  onOpenAdmin,
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
  /** v1.2 会话过期时间（ms/秒 epoch 容错）；缺省 = 旧 relay 直连模式。 */
  sessionExpiresAt?: number;
  /** 过期横幅点击 → 登出流程（重新登录即续期 30 天）。 */
  onRenewSession?: () => void;
  /** v1.5 管理面板入口（App 仅会话模式传入；旧 relay 不显示）。 */
  onOpenAdmin?: () => void;
  onSelect: (conv: string) => void;
  /** 机器切换器点选（切 currentMachine + 触发该机器 sync）。 */
  onSelectMachine: (machine: string) => void;
  /** 打开新会话弹层（v1.6：会话名 + 工作目录选择在 App 级弹层完成）。 */
  onCreate: () => void;
}) {

  // v1.2 过期横幅：剩余 < 3 天提示（天数向上取整，至少 1 天）
  const daysLeft = useMemo(() => {
    if (sessionExpiresAt === undefined) {
      return null;
    }
    const remain = normalizeExpiryMs(sessionExpiresAt) - Date.now();
    return Math.max(1, Math.ceil(remain / 86_400_000));
  }, [sessionExpiresAt]);
  const expiryWarning = daysLeft !== null && daysLeft < EXPIRY_WARN_DAYS;

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
                  {item.title || convDisplayName(item.convId)}
                </span>
                {/* 未读徽标优先于当前项小圆点（有未读必非当前项） */}
                {item.unread && item.unread > 0 ? (
                  <span
                    className="flex h-4 min-w-[1rem] shrink-0 items-center justify-center rounded-full bg-emerald-500 px-1 text-[10px] font-semibold leading-none text-white"
                    aria-label={`${item.unread} 条未读`}
                  >
                    {item.unread > 99 ? "99+" : item.unread}
                  </span>
                ) : (
                  active && (
                    <span
                      className="h-1.5 w-1.5 shrink-0 rounded-full bg-emerald-500"
                      aria-hidden="true"
                    />
                  )
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
        {/* v1.2 会话过期横幅：点击进入登出流程（重新登录续期 30 天） */}
        {expiryWarning && onRenewSession && (
          <button
            type="button"
            onClick={onRenewSession}
            className="mb-1.5 w-full rounded-lg border border-amber-400/60 bg-amber-50 px-3 py-1.5 text-left text-xs text-amber-800 transition-colors hover:bg-amber-100 dark:border-amber-500/40 dark:bg-amber-950/60 dark:text-amber-200 dark:hover:bg-amber-900/60"
          >
            会话 {daysLeft} 天后过期，重新登录可续期
          </button>
        )}
        {/* v1.5 管理面板入口（仅会话模式——旧 relay 无会话层不显示） */}
        {onOpenAdmin && (
          <button
            type="button"
            onClick={onOpenAdmin}
            className="mb-1.5 w-full rounded-lg px-3 py-1.5 text-left text-xs text-muted transition-colors hover:bg-panel2 hover:text-ink"
          >
            管理
          </button>
        )}
        {/* v1.6：新会话入口改为开 App 级弹层（会话名 + 工作目录选择） */}
        <button
          type="button"
          onClick={onCreate}
          className="w-full rounded-lg border border-dashed border-line-strong px-3 py-2 text-sm text-muted transition-colors hover:border-emerald-500/60 hover:text-ink"
        >
          + 新会话
        </button>
      </div>
    </div>
  );
}
