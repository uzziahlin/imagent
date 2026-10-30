/**
 * E2E 指纹确认弹层（v1.4 TOFU 首连）：每机器首次握手成功后弹出，展示该
 * 机器的 gk（X25519 网关公钥）与 ed25519（身份公钥）指纹——SHA-256 前
 * 16 hex、4 位一组。用户与终端 `imagent start` 日志中的 E2E 指纹核对：
 * 一致 → 「信任并启用加密」（pin + 会话激活）；不一致/存疑 → 「不信任」
 * （该机器转不支持态，发送禁用，可再显式选择明文）。安全门闩性质：无
 * 进出过渡、不可点遮罩关闭，必须二选一。
 */
export function E2eConfirm({
  machine,
  gkFp,
  idFp,
  onResolve,
}: {
  machine: string;
  gkFp: string;
  idFp: string;
  onResolve: (accept: boolean) => void;
}) {
  return (
    <div
      className="fixed inset-0 z-50 bg-black/50"
      role="dialog"
      aria-modal="true"
      aria-label="确认机器加密指纹"
    >
      <div className="absolute left-1/2 top-1/2 w-[calc(100%-2rem)] max-w-sm -translate-x-1/2 -translate-y-1/2 rounded-2xl border border-line bg-panel p-4 shadow-2xl">
        <div className="mb-1 flex items-center gap-2">
          <svg viewBox="0 0 20 20" className="h-5 w-5 shrink-0 text-amber-500" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
            <rect x="4" y="8.5" width="12" height="8" rx="2" />
            <path d="M7 8.5V6.5a3 3 0 0 1 6 0v2" strokeLinecap="round" />
          </svg>
          <h2 className="text-sm font-semibold text-ink">确认机器加密指纹</h2>
        </div>
        <p className="mb-3 text-xs leading-relaxed text-muted">
          首次连接机器 <span className="font-medium text-ink">{machine}</span>。请与终端{" "}
          <code className="rounded bg-panel2 px-1 py-px text-[11px]">imagent start</code>{" "}
          日志中的 E2E 指纹核对，一致方可信任；不一致说明连接可能被窃听或冒充。
        </p>
        <dl className="space-y-2" aria-label="机器指纹">
          <div className="rounded-xl border border-line bg-surface px-3 py-2">
            <dt className="text-[11px] text-ghost">网关密钥（X25519）指纹</dt>
            <dd className="mt-0.5 font-mono text-sm tracking-wider text-ink">{gkFp}</dd>
          </div>
          <div className="rounded-xl border border-line bg-surface px-3 py-2">
            <dt className="text-[11px] text-ghost">身份密钥（Ed25519）指纹</dt>
            <dd className="mt-0.5 font-mono text-sm tracking-wider text-ink">{idFp}</dd>
          </div>
        </dl>
        <div className="mt-4 flex gap-2">
          <button
            type="button"
            onClick={() => onResolve(false)}
            className="press flex-1 rounded-xl border border-line-strong py-2 text-sm text-muted transition-colors hover:border-rose-400/60 hover:text-err"
          >
            不信任
          </button>
          <button
            type="button"
            onClick={() => onResolve(true)}
            className="press flex-1 rounded-xl bg-emerald-600 py-2 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700"
          >
            信任并启用加密
          </button>
        </div>
      </div>
    </div>
  );
}
