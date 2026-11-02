import { useEffect, useRef, useState } from "react";
import type { ApprovalDecision, PendingApproval } from "../lib/protocol";

/**
 * 审批弹层：任一机器任一 conv 的 pending 非空即置顶显示（置顶 overlay，不挡
 * 消息流滚动以外的交互）。多机模式显示来源机器徽标（App 侧已按机器分组排
 * 序：在线机器在前）；三按钮 → 发 approval_response（convId/requestId 原样
 * 回传、machine = 审批来源机器）→ 本地乐观移除；收到 approval_resolved 在
 * 流里留终态记录。
 * conv 徽标显示名由 App 注入 convTitleOf（自动会话 web:s-xxxx 显示首条消息
 * 派生的自动 title，其余 convId 剥前缀）。
 *
 * v1.37：
 * - 「前往会话」小按钮（App 注入 onJump = select_machine + select_conv）：
 *   仅视口切换，不关闭弹层——审批仍需处理，回来条目还在。
 * - 倒计时：帧带 timeoutSecs（超时预算秒数）时条目渲染剩余秒数——收到时刻
 *   （at）+ timeoutSecs 为截止，1s interval 本地倒数，到 0 显示「即将超时」
 *   态不再负数；缺席（旧网关）不渲染。纯 UI 提示——超时自动拒绝由网关兜底，
 *   前端不替代。
 *
 * 微动效：条目 slide-in（150ms）；0→非空出现时移动端触觉提示
 * navigator.vibrate([80,40,80])（桌面/iOS Safari 无该能力自然跳过；全体动画
 * 在 prefers-reduced-motion 下由 index.css 全局规则禁用）。
 */
export function ApprovalPanel({
  pending,
  showMachine,
  convTitleOf,
  onRespond,
  onJump,
}: {
  pending: PendingApproval[];
  /** 多机模式显示机器徽标；单机模式不显示（行为与升级前一致）。 */
  showMachine: boolean;
  /** conv 显示名解析（App 注入 convTitle；pending 跨机器，须按 machine 取
   *  该机器空间里的 conv 状态）。 */
  convTitleOf: (convId: string, machine: string) => string;
  onRespond: (p: PendingApproval, decision: ApprovalDecision) => void;
  /** v1.37 「前往会话」：App 注入 select_machine + select_conv（先机器后
   *  会话——与深链同序，select_conv 落在该机器会话空间才有效）。 */
  onJump: (p: PendingApproval) => void;
}) {
  // 0→非空沿（弹层「出现」时机）触发一次触觉提示；非空→非空（追加新审批）不震
  const hadPending = useRef(false);
  useEffect(() => {
    const has = pending.length > 0;
    if (has && !hadPending.current) {
      try {
        navigator.vibrate?.([80, 40, 80]);
      } catch {
        // 无振动能力（桌面 / iOS Safari），静默跳过
      }
    }
    hadPending.current = has;
  }, [pending.length]);

  // v1.37 倒计时 tick：存在带 timeoutSecs 的条目才起 1s interval（纯 UI 本地
  // 时钟；无倒计时条目的常态不引入定时器）。now 与收到时刻 at 同用本地时钟，
  // 无跨源偏差；倒计时条目集合变化瞬间对齐一次 now，防首帧用旧 tick 显多 1s。
  const hasCountdown = pending.some((p) => p.timeoutSecs !== undefined);
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!hasCountdown) {
      return;
    }
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      clearInterval(t);
    };
  }, [hasCountdown]);

  /** 剩余秒数（向上取整；到 0 封底不再负数——「即将超时」态由网关超时兜底）。 */
  const remainSecs = (p: PendingApproval): number | null => {
    if (p.timeoutSecs === undefined) {
      return null;
    }
    return Math.max(0, Math.ceil((p.at + p.timeoutSecs * 1000 - now) / 1000));
  };

  if (pending.length === 0) {
    return null;
  }
  return (
    <div className="pointer-events-none absolute inset-x-0 top-0 z-40 space-y-2 p-3">
      {pending.map((p) => {
        const remain = remainSecs(p);
        return (
          <div
            key={p.requestId}
            role="alertdialog"
            aria-label={`审批请求 ${p.tool}`}
            className="pointer-events-auto animate-slide-in rounded-2xl border border-amber-400/70 bg-amber-50 p-3.5 shadow-2xl backdrop-blur dark:border-amber-500/50 dark:bg-amber-950/95"
          >
            <div className="mb-1.5 flex items-center gap-2 text-sm font-semibold text-amber-800 dark:text-amber-200">
              <span aria-hidden="true">⚠</span>
              <span className="truncate">需要审批：{p.tool}</span>
              {showMachine && (
                <span
                  className="shrink-0 rounded-full bg-amber-200/70 px-2 py-0.5 font-mono text-xs font-normal text-amber-800 dark:bg-amber-900/60 dark:text-amber-300/90"
                  title={`机器 ${p.machine}`}
                >
                  {p.machine}
                </span>
              )}
              <span className="ml-auto shrink-0 rounded-full bg-amber-200/70 px-2 py-0.5 font-mono text-xs font-normal text-amber-800 dark:bg-amber-900/60 dark:text-amber-300/90">
                {convTitleOf(p.convId, p.machine)}
              </span>
              {/* v1.37 倒计时徽标（帧带 timeoutSecs 才渲染；0 = 即将超时态） */}
              {remain !== null && (
                <span
                  className={`shrink-0 rounded-full px-2 py-0.5 font-mono text-xs font-normal ${
                    remain > 0
                      ? "bg-amber-200/70 text-amber-800 dark:bg-amber-900/60 dark:text-amber-300/90"
                      : "bg-rose-500/90 font-semibold text-white dark:bg-rose-600/90"
                  }`}
                  title={remain > 0 ? `约 ${remain} 秒后网关自动拒绝` : "即将超时"}
                >
                  {remain > 0 ? `${remain}s` : "即将超时"}
                </span>
              )}
            </div>
            <p className="mb-3 max-h-24 overflow-y-auto rounded-lg bg-amber-100/80 px-2.5 py-1.5 font-mono text-xs break-all text-amber-900 dark:bg-black/30 dark:text-amber-100/90">
              {p.inputSummary}
            </p>
            <div className="flex gap-2">
              <button
                onClick={() => onRespond(p, "allow")}
                className="press flex-1 rounded-xl bg-emerald-600 py-2 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700"
              >
                允许
              </button>
              <button
                onClick={() => onRespond(p, "deny")}
                className="press flex-1 rounded-xl bg-rose-700/90 py-2 text-sm font-semibold text-white transition-colors hover:bg-rose-600 active:bg-rose-800"
              >
                拒绝
              </button>
              <button
                onClick={() => onRespond(p, "always")}
                className="press flex-1 rounded-xl border border-amber-500/50 bg-transparent py-2 text-sm font-medium text-amber-800 transition-colors hover:bg-amber-200/50 dark:text-amber-200 dark:hover:bg-amber-900/60"
              >
                始终允许
              </button>
              {/* v1.37 前往会话：视口切到审批来源机器+会话（弹层不关闭） */}
              <button
                onClick={() => onJump(p)}
                title="切换到该会话查看上下文（审批待处理，不关闭）"
                className="press shrink-0 rounded-xl border border-amber-500/50 bg-transparent px-3 py-2 text-sm font-medium text-amber-800 transition-colors hover:bg-amber-200/50 dark:text-amber-200 dark:hover:bg-amber-900/60"
              >
                前往会话
              </button>
            </div>
          </div>
        );
      })}
    </div>
  );
}
