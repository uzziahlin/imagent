import type { ApprovalDecision, PendingApproval } from "../lib/protocol";
import { convDisplayName } from "../lib/protocol";

/**
 * 审批弹层：任一 conv 的 pending 非空即置顶显示（置顶 overlay，不挡消息流
 * 滚动以外的交互）。三按钮 → 发 approval_response（convId/requestId 由调用
 * 方原样回传）→ 本地乐观移除；收到 approval_resolved 在流里留终态记录。
 */
export function ApprovalPanel({
  pending,
  onRespond,
}: {
  pending: PendingApproval[];
  onRespond: (convId: string, requestId: string, decision: ApprovalDecision) => void;
}) {
  if (pending.length === 0) {
    return null;
  }
  return (
    <div className="pointer-events-none absolute inset-x-0 top-0 z-40 space-y-2 p-3">
      {pending.map((p) => (
        <div
          key={p.requestId}
          role="alertdialog"
          aria-label={`审批请求 ${p.tool}`}
          className="pointer-events-auto rounded-2xl border border-amber-400/70 bg-amber-50 p-3.5 shadow-2xl backdrop-blur dark:border-amber-500/50 dark:bg-amber-950/95"
        >
          <div className="mb-1.5 flex items-center gap-2 text-sm font-semibold text-amber-800 dark:text-amber-200">
            <span aria-hidden="true">⚠</span>
            <span>需要审批：{p.tool}</span>
            <span className="ml-auto rounded-full bg-amber-200/70 px-2 py-0.5 font-mono text-xs font-normal text-amber-800 dark:bg-amber-900/60 dark:text-amber-300/90">
              {convDisplayName(p.convId)}
            </span>
          </div>
          <p className="mb-3 max-h-24 overflow-y-auto rounded-lg bg-amber-100/80 px-2.5 py-1.5 font-mono text-xs break-all text-amber-900 dark:bg-black/30 dark:text-amber-100/90">
            {p.inputSummary}
          </p>
          <div className="flex gap-2">
            <button
              onClick={() => onRespond(p.convId, p.requestId, "allow")}
              className="flex-1 rounded-xl bg-emerald-600 py-2 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700"
            >
              允许
            </button>
            <button
              onClick={() => onRespond(p.convId, p.requestId, "deny")}
              className="flex-1 rounded-xl bg-rose-700/90 py-2 text-sm font-semibold text-white transition-colors hover:bg-rose-600 active:bg-rose-800"
            >
              拒绝
            </button>
            <button
              onClick={() => onRespond(p.convId, p.requestId, "always")}
              className="flex-1 rounded-xl border border-amber-500/50 bg-transparent py-2 text-sm font-medium text-amber-800 transition-colors hover:bg-amber-200/50 dark:text-amber-200 dark:hover:bg-amber-900/60"
            >
              始终允许
            </button>
          </div>
        </div>
      ))}
    </div>
  );
}
