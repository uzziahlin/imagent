/**
 * 会话删除确认弹层（v1.41.3）：侧边栏长按 500ms 触发，二次确认后由 App 发
 * conv_delete 业务帧（网关删 sessions/outbound_log/卡片缓存三处）并本地先行
 * 移除该会话（快照随后权威收敛）。破坏性动作：确认钮走 rose 危险配色（区别
 * E2eConfirm 的信任/不信任对称双钮——此处取消是安全侧，删除是危险侧）。
 * 弹层形态对齐 E2eConfirm（fixed 遮罩 + 居中卡片；不可点遮罩关闭——防误触，
 * 必须显式二选一）。
 */
export function ConvDeleteConfirm({
  convTitle,
  busy,
  onConfirm,
  onCancel,
}: {
  /** 目标会话显示名（App 经 convTitle 派生传入）。 */
  convTitle: string;
  /** 删除请求发送中（防重复提交；发帧失败保持弹层可重试）。 */
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-50 bg-black/50"
      role="dialog"
      aria-modal="true"
      aria-label="确认删除会话"
    >
      <div className="absolute left-1/2 top-1/2 w-[calc(100%-2rem)] max-w-sm -translate-x-1/2 -translate-y-1/2 rounded-2xl border border-line bg-panel p-4 shadow-2xl">
        <div className="mb-1 flex items-center gap-2">
          <svg viewBox="0 0 20 20" className="h-5 w-5 shrink-0 text-err" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
            <path d="M4 6h12M8 6V4.5A1.5 1.5 0 0 1 9.5 3h1A1.5 1.5 0 0 1 12 4.5V6M6 6l.7 9a1.5 1.5 0 0 0 1.5 1.4h3.6a1.5 1.5 0 0 0 1.5-1.4L14 6" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
          <h2 className="text-sm font-semibold text-ink">删除会话</h2>
        </div>
        <p className="mb-3 text-xs leading-relaxed text-muted">
          确定删除会话{" "}
          <span className="font-medium text-ink">{convTitle}</span>{" "}
          吗？网关侧会话记录与驾驶舱消息将一并清除；若该会话有进行中的任务，
          删除会被拒绝（请先 <code className="rounded bg-panel2 px-1 py-px text-[11px]">/stop</code>）。
        </p>
        <div className="mt-4 flex gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="press flex-1 rounded-xl border border-line-strong py-2 text-sm text-muted transition-colors hover:border-line-strong hover:text-ink"
          >
            取消
          </button>
          <button
            type="button"
            onClick={onConfirm}
            disabled={busy}
            className="press flex-1 rounded-xl bg-rose-600 py-2 text-sm font-semibold text-white transition-colors hover:bg-rose-500 active:bg-rose-700 disabled:cursor-wait disabled:opacity-60"
          >
            删除
          </button>
        </div>
      </div>
    </div>
  );
}
