import type { CardEntry, CardPhase, CardTerminal, TodoStatus } from "../lib/protocol";
import { MarkdownText } from "./MarkdownText";

const PHASE_META: Record<CardPhase, { label: string; cls: string }> = {
  thinking: { label: "思考中", cls: "bg-violet-500/15 text-violet-300" },
  tool_running: { label: "执行工具", cls: "bg-sky-500/15 text-sky-300" },
  outputting: { label: "输出中", cls: "bg-emerald-500/15 text-emerald-300" },
  waiting_approval: { label: "等待审批", cls: "bg-amber-500/15 text-amber-300" },
};

const TERMINAL_META: Record<CardTerminal, { label: string; cls: string }> = {
  running: { label: "进行中", cls: "text-slate-400" },
  done: { label: "已完成", cls: "text-emerald-400" },
  error: { label: "出错", cls: "text-rose-400" },
};

const TODO_GLYPH: Record<TodoStatus, string> = {
  pending: "○",
  in_progress: "◐",
  completed: "✔",
};

const TODO_CLS: Record<TodoStatus, string> = {
  pending: "text-slate-500",
  in_progress: "text-amber-300",
  completed: "text-emerald-400 line-through decoration-slate-600",
};

function formatSecs(secs: number): string {
  if (secs < 60) {
    return `${secs}s`;
  }
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s === 0 ? `${m}m` : `${m}m${s}s`;
}

/**
 * 工具/任务卡片：card.text 过 DOMPurify 后按 markdown 渲染；footer 显示
 * phase / 终态 / run_secs；todos 非空时显示勾选列表；streaming 态带光标与
 * 呼吸描边。流式更新 = 同 messageId 的 card_patch 整卡替换（上层已归并）。
 *
 * M3b 留位：tool_calls / thoughts / usage_display 已保留在 state
 * （CardPayload 全字段），富渲染（代码高亮 / 工具时间线 / diff）在此扩展。
 */
export function CardView({ entry }: { entry: CardEntry }) {
  const { card, state } = entry;
  const streaming = state === "streaming";
  const terminal = TERMINAL_META[card.terminal];
  const phase = PHASE_META[card.phase];
  const borderCls = streaming
    ? "border-emerald-500/40"
    : card.terminal === "error"
      ? "border-rose-500/40"
      : "border-slate-700";

  return (
    <div className={`rounded-2xl border bg-slate-900/70 p-3 ${borderCls}`}>
      <div className="mb-2 flex items-center gap-2">
        <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${phase.cls}`}>
          {phase.label}
        </span>
        {card.task_digest && (
          <span className="min-w-0 truncate text-xs text-slate-500">{card.task_digest}</span>
        )}
        {streaming && (
          <span className="ml-auto animate-pulse text-emerald-400" aria-label="生成中">
            ▍
          </span>
        )}
      </div>

      {card.error && (
        <p className="mb-2 rounded-lg border border-rose-500/30 bg-rose-950/40 px-2.5 py-1.5 text-xs text-rose-300">
          错误：{card.error}
        </p>
      )}

      {card.text && <MarkdownText source={card.text} />}

      {card.todos.length > 0 && (
        <ul className="mt-2 space-y-1 rounded-xl bg-slate-950/50 p-2.5">
          {card.todos.map((t, i) => (
            <li key={t.id ?? i} className={`flex gap-2 text-xs ${TODO_CLS[t.status]}`}>
              <span aria-hidden="true">{TODO_GLYPH[t.status]}</span>
              <span className="min-w-0 break-words">{t.text}</span>
            </li>
          ))}
        </ul>
      )}

      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
        <span className={terminal.cls}>{terminal.label}</span>
        <span className="text-slate-500">{formatSecs(card.run_secs)}</span>
        {card.usage_display && <span className="text-slate-500">{card.usage_display}</span>}
        {card.queued_hint && <span className="text-amber-300/80">{card.queued_hint}</span>}
      </div>
    </div>
  );
}
