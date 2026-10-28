import { memo, useState } from "react";
import type {
  CardEntry,
  CardPhase,
  CardTerminal,
  TodoItem,
  TodoStatus,
  ToolCallPayload,
} from "../lib/protocol";
import { MarkdownText } from "./MarkdownText";

const PHASE_META: Record<CardPhase, { label: string; cls: string }> = {
  thinking: { label: "思考中", cls: "bg-violet-500/15 text-think" },
  tool_running: { label: "工具运行", cls: "bg-sky-500/15 text-info" },
  outputting: { label: "输出中", cls: "bg-emerald-500/15 text-ok" },
  waiting_approval: { label: "等待审批", cls: "bg-amber-500/15 text-warn" },
};

const TERMINAL_META: Record<CardTerminal, { label: string; cls: string }> = {
  running: { label: "进行中", cls: "text-muted" },
  done: { label: "已完成", cls: "text-ok" },
  error: { label: "出错", cls: "text-err" },
};

const TODO_GLYPH: Record<TodoStatus, string> = {
  pending: "○",
  in_progress: "◐",
  completed: "✔",
};

const TODO_CLS: Record<TodoStatus, string> = {
  pending: "text-faint",
  in_progress: "text-warn font-medium",
  completed: "text-ok line-through decoration-line-strong",
};

function formatSecs(secs: number): string {
  if (secs < 60) {
    return `${secs}s`;
  }
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s === 0 ? `${m}m` : `${m}m${s}s`;
}

// ---------------------------------------------------------------------------
// 流式渲染优化：子组件各自 memo，card_patch 整卡替换时只有内容变化的区块
// 重渲（text 流式打字不重渲时间线，tool_calls 变化不重渲 markdown 正文）。
// card_patch 每帧都是新 JSON 对象，数组/对象引用必变，故比较器按值比较。
// ---------------------------------------------------------------------------

function toolCallsEqual(a: ToolCallPayload[], b: ToolCallPayload[]): boolean {
  if (a === b) {
    return true;
  }
  if (a.length !== b.length) {
    return false;
  }
  return a.every((x, i) => x.name === b[i].name && x.summary === b[i].summary && x.done === b[i].done);
}

function thoughtsEqual(a: string[], b: string[]): boolean {
  return a === b || (a.length === b.length && a.every((x, i) => x === b[i]));
}

function todosEqual(a: TodoItem[], b: TodoItem[]): boolean {
  if (a === b) {
    return true;
  }
  if (a.length !== b.length) {
    return false;
  }
  return a.every((x, i) => x.text === b[i].text && x.status === b[i].status && x.id === b[i].id);
}

function Spinner() {
  return (
    <svg
      className="h-3.5 w-3.5 animate-spin shrink-0 text-info"
      viewBox="0 0 24 24"
      fill="none"
      aria-hidden="true"
    >
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
      <path
        className="opacity-90"
        d="M12 2a10 10 0 0 1 10 10"
        stroke="currentColor"
        strokeWidth="4"
        strokeLinecap="round"
      />
    </svg>
  );
}

/**
 * 执行链时间线（Claude Code 终端式）：运行中项自动展开（spinner + 工具名 +
 * summary，高亮当前活动项），完成后自动折叠为单行 ✓（低对比度）——完全由
 * done 标志驱动，无需用户交互。
 */
const ToolTimeline = memo(
  function ToolTimeline({ calls }: { calls: ToolCallPayload[] }) {
    if (calls.length === 0) {
      return null;
    }
    return (
      <ol className="relative my-2 space-y-0.5 border-l-2 border-line pl-3" aria-label="执行链">
        {calls.map((c, i) =>
          c.done ? (
            <li
              key={c.id ?? i}
              className="flex items-baseline gap-1.5 rounded-md px-1.5 py-0.5 font-mono text-xs text-faint"
            >
              <span className="shrink-0 text-ok" aria-hidden="true">
                ✓
              </span>
              <span className="shrink-0">{c.name}</span>
              {c.summary && (
                <>
                  <span className="shrink-0 text-ghost" aria-hidden="true">
                    ·
                  </span>
                  <span className="min-w-0 truncate">{c.summary}</span>
                </>
              )}
            </li>
          ) : (
            <li
              key={c.id ?? i}
              className="rounded-lg bg-sky-500/10 px-2 py-1.5"
              aria-current="step"
            >
              <div className="flex items-center gap-2">
                <Spinner />
                <span className="font-mono text-xs font-semibold text-ink">{c.name}</span>
              </div>
              {c.summary && (
                <p className="mt-1 break-all font-mono text-xs leading-5 text-muted">{c.summary}</p>
              )}
            </li>
          ),
        )}
      </ol>
    );
  },
  (prev, next) => toolCallsEqual(prev.calls, next.calls),
);

/** 思考过程：默认折叠单行（运行时「思考中…」/ 完成后「思考过程（N 条）」），点击展开。 */
const Thoughts = memo(
  function Thoughts({ thoughts, active }: { thoughts: string[]; active: boolean }) {
    const [open, setOpen] = useState(false);
    if (thoughts.length === 0) {
      return null;
    }
    return (
      <div className="my-2">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
          className="flex w-full items-center gap-1.5 rounded-lg px-1.5 py-1 text-xs text-faint transition-colors hover:bg-panel2 hover:text-muted"
        >
          <span aria-hidden="true">💭</span>
          <span>{active ? "思考中…" : `思考过程（${thoughts.length} 条）`}</span>
          <span className="ml-auto text-ghost" aria-hidden="true">
            {open ? "▾" : "▸"}
          </span>
        </button>
        {open && (
          <div className="mt-1 space-y-1.5 rounded-xl bg-panel2 p-2.5">
            {thoughts.map((t, i) => (
              <p key={i} className="break-words text-xs leading-relaxed text-muted">
                {t}
              </p>
            ))}
          </div>
        )}
      </div>
    );
  },
  (prev, next) => prev.active === next.active && thoughtsEqual(prev.thoughts, next.thoughts),
);

/** 任务清单：紧凑样式（小字号，进行中高亮）。 */
const Todos = memo(
  function Todos({ todos }: { todos: TodoItem[] }) {
    if (todos.length === 0) {
      return null;
    }
    return (
      <ul className="mt-2 space-y-0.5 rounded-xl bg-code/60 p-2">
        {todos.map((t, i) => (
          <li key={t.id ?? i} className={`flex gap-1.5 text-xs leading-5 ${TODO_CLS[t.status]}`}>
            <span aria-hidden="true">{TODO_GLYPH[t.status]}</span>
            <span className="min-w-0 break-words">{t.text}</span>
          </li>
        ))}
      </ul>
    );
  },
  (prev, next) => todosEqual(prev.todos, next.todos),
);

/** markdown 正文：按 text 值 memo（字符串值比较，流式时间线/页脚变化不重渲）。 */
const MarkdownBody = memo(function MarkdownBody({
  text,
  streaming,
}: {
  text: string;
  streaming: boolean;
}) {
  if (!text) {
    return null;
  }
  return (
    <div>
      <MarkdownText source={text} />
      {streaming && (
        <span className="ml-0.5 inline-block animate-pulse align-baseline text-ok" aria-label="生成中">
          ▍
        </span>
      )}
    </div>
  );
});

const Footer = memo(function Footer({
  terminal,
  runSecs,
  usage,
  queuedHint,
}: {
  terminal: CardTerminal;
  runSecs: number;
  usage?: string;
  queuedHint?: string;
}) {
  const meta = TERMINAL_META[terminal];
  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
      <span className={meta.cls}>{meta.label}</span>
      <span className="text-faint">{formatSecs(runSecs)}</span>
      {usage && <span className="text-faint">{usage}</span>}
      {queuedHint && <span className="text-warn">{queuedHint}</span>}
    </div>
  );
});

/**
 * 工具/任务卡片：头部 phase 徽标 + task_digest；正文上方为执行链时间线
 * （tool_calls）与折叠的思考过程（thoughts）；markdown 正文过 DOMPurify
 * 渲染，流式光标在正文末尾；footer 为终态/时长/用量/排队提示。流式更新 =
 * 同 messageId 的 card_patch 整卡替换（上层已归并），子区块 memo 按值比较。
 */
export function CardView({ entry }: { entry: CardEntry }) {
  const { card, state } = entry;
  const streaming = state === "streaming";
  const phase = PHASE_META[card.phase];
  const borderCls = streaming
    ? "border-emerald-500/40"
    : card.terminal === "error"
      ? "border-rose-500/40"
      : "border-line";

  return (
    <div className={`rounded-2xl border bg-panel p-3 ${borderCls}`}>
      <div className="mb-2 flex items-center gap-2">
        <span className={`shrink-0 rounded-full px-2 py-0.5 text-xs font-medium ${phase.cls}`}>
          {phase.label}
        </span>
        {card.task_digest && (
          <span className="min-w-0 truncate text-xs text-faint">{card.task_digest}</span>
        )}
      </div>

      {card.error && (
        <p className="mb-2 rounded-lg border border-rose-400/50 bg-rose-50 px-2.5 py-1.5 text-xs text-err dark:border-rose-500/30 dark:bg-rose-950/40">
          错误：{card.error}
        </p>
      )}

      <ToolTimeline calls={card.tool_calls} />
      <Thoughts thoughts={card.thoughts} active={streaming} />
      <MarkdownBody text={card.text} streaming={streaming} />
      <Todos todos={card.todos} />
      <Footer
        terminal={card.terminal}
        runSecs={card.run_secs}
        usage={card.usage_display}
        queuedHint={card.queued_hint}
      />
    </div>
  );
}
