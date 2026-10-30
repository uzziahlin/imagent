import { memo, useState } from "react";
import type {
  CardEntry,
  CardPhase,
  CardTerminal,
  TodoItem,
  TodoStatus,
  ToolCallPayload,
} from "../lib/protocol";
import { CopyButton } from "./CopyButton";
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
 * 执行链时间线（Claude Code 终端式）：流式中运行项自动展开（spinner + 工具名 +
 * summary，高亮当前活动项），完成后自动折叠为单行 ✓（低对比度）——完全由
 * done 标志驱动，无需用户交互。卡片终态（finished）后整块收起为一行摘要按钮
 * （全部完成「✓ 执行了 N 个工具」；存在未完成即 error 中断场景用 ⚠️
 * 「N 个工具调用 · M 个未完成」），点击展开完整时间线、再点收起——终态后
 * 时间线不再整块占版面；流式期间行为不变。
 */
const ToolTimeline = memo(
  function ToolTimeline({ calls, finished }: { calls: ToolCallPayload[]; finished: boolean }) {
    // 终态折叠的展开态：卡片流式→终态是同一组件实例翻转（key 不变），useState
    // 初值 false 恰为默认收起，无需 effect 同步。
    const [open, setOpen] = useState(false);
    if (calls.length === 0) {
      return null;
    }
    const timeline = (
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
    if (!finished) {
      return timeline;
    }
    const unfinished = calls.filter((c) => !c.done).length;
    return (
      <div className="my-2">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
          className="flex w-full items-center gap-1.5 rounded-lg px-1.5 py-1 text-xs text-faint transition-colors hover:bg-panel2 hover:text-muted"
        >
          {unfinished === 0 ? (
            <>
              <span className="shrink-0 text-ok" aria-hidden="true">
                ✓
              </span>
              <span>执行了 {calls.length} 个工具</span>
            </>
          ) : (
            <>
              <span aria-hidden="true">⚠️</span>
              <span>
                {calls.length} 个工具调用 · {unfinished} 个未完成
              </span>
            </>
          )}
          <span className="ml-auto text-ghost" aria-hidden="true">
            {open ? "▾" : "▸"}
          </span>
        </button>
        {open && timeline}
      </div>
    );
  },
  (prev, next) => prev.finished === next.finished && toolCallsEqual(prev.calls, next.calls),
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

/** markdown 正文：按 text 值 memo（字符串值比较，流式时间线/页脚变化不重渲）。
 *  key={text} + animate-fade-in：card_patch 正文变化时轻微淡入（120ms、起始
 *  0.55——流式高频补丁下是柔和的「呼吸更新」而非闪烁；prefers-reduced-motion
 *  下由全局规则禁用）。 */
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
    <div key={text} className="animate-fade-in">
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
 * 工具/任务卡片：头部 phase 徽标（仅流式渲染——Rust finalize 终态帧 phase
 * 保留最后 chunk 状态不重置，对飞书平台有既有语义故不改协议；终态语义由
 * footer 已完成/出错承载）+ task_digest（右端整段复制，终态保留）；正文上方
 * 为执行链时间线（流式运行项展开高亮；终态整块折叠为一行摘要按钮，点击
 * 展开）与折叠的思考过程（thoughts）；markdown 正文经 MarkdownText 渲染
 * （rehype-sanitize 输出层消毒 + highlight.js 语法高亮），流式光标在正文
 * 末尾；footer 为终态/时长/用量/排队提示。流式更新 = 同 messageId 的
 * card_patch 整卡替换（上层已归并），子区块 memo 按值比较。
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
    <div className={`group rounded-2xl border bg-panel p-3 ${borderCls}`}>
      <div className="mb-2 flex items-center gap-2">
        {streaming && (
          <span className={`shrink-0 rounded-full px-2 py-0.5 text-xs font-medium ${phase.cls}`}>
            {phase.label}
          </span>
        )}
        {card.task_digest && (
          <span className="min-w-0 truncate text-xs text-faint">{card.task_digest}</span>
        )}
        {/* 整段复制卡片正文：桌面 hover 显现 / 移动常显（可长按） */}
        <span className="ml-auto shrink-0 opacity-60 transition-opacity duration-100 focus-within:opacity-100 md:opacity-0 md:group-hover:opacity-100">
          <CopyButton getText={() => card.text} label="复制整段" />
        </span>
      </div>

      {card.error && (
        <p className="mb-2 rounded-lg border border-rose-400/50 bg-rose-50 px-2.5 py-1.5 text-xs text-err dark:border-rose-500/30 dark:bg-rose-950/40">
          错误：{card.error}
        </p>
      )}

      <ToolTimeline calls={card.tool_calls} finished={!streaming} />
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
