import { memo, useEffect, useRef, useState, type ReactNode, type UIEvent } from "react";
import type { StreamEntry } from "../lib/protocol";
import { CardView } from "./CardView";
import { CopyButton } from "./CopyButton";
import { MarkdownText } from "./MarkdownText";

/** 距底小于该值视为「在底部附近」，新内容自动跟随。 */
const NEAR_BOTTOM_PX = 120;

/** 空态示例 prompt（会话无消息时展示，点击即发送）。 */
const SAMPLE_PROMPTS = ["看看这个项目结构", "帮我写个脚本", "解释一下最近的改动"];

/** 消息时间戳：HH:MM（与侧栏同风格；缺席不渲染——旧条目兼容）。 */
function fmtTime(at: number | undefined): string {
  if (!at) {
    return "";
  }
  return new Date(at).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/** agent 头像标识（左侧小圆徽：emerald 底机器人轮廓，与用户右气泡分层）。 */
function AgentBadge() {
  return (
    <span
      className="mt-5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-emerald-600/15 text-emerald-600 dark:text-emerald-400"
      aria-hidden="true"
    >
      <svg viewBox="0 0 16 16" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="1.4">
        <rect x="2.5" y="5.5" width="11" height="7.5" rx="2.5" />
        <circle cx="6.2" cy="9.2" r="0.9" fill="currentColor" stroke="none" />
        <circle cx="9.8" cy="9.2" r="0.9" fill="currentColor" stroke="none" />
        <path d="M8 5.5V3.2" strokeLinecap="round" />
        <circle cx="8" cy="2.4" r="0.9" fill="currentColor" stroke="none" />
      </svg>
    </span>
  );
}

/**
 * 整段复制的显隐包裹：移动端（无 hover）常显半透明可点；桌面 hover 显现。
 * focus-within 兜底键盘操作可见。
 */
function CopyReveal({ children }: { children: ReactNode }) {
  return (
    <span className="ml-auto shrink-0 opacity-60 transition-opacity duration-100 focus-within:opacity-100 md:opacity-0 md:group-hover:opacity-100">
      {children}
    </span>
  );
}

/** 单条消息：memo 按条目引用比较——card_patch 只换对应 card 条目的引用，
 *  其余条目（text/user/approval_note，reducer 不可变更新保留引用）跳过重渲。 */
const EntryView = memo(function EntryView({ entry }: { entry: StreamEntry }) {
  switch (entry.kind) {
    case "user":
      // 用户消息：右对齐主题色气泡 + 上方浅色时间戳
      return (
        <div className="flex flex-col items-end">
          {entry.at ? (
            <span className="mb-0.5 pr-1 text-[11px] leading-4 text-ghost">{fmtTime(entry.at)}</span>
          ) : null}
          <div className="max-w-[80%] rounded-2xl rounded-br-md bg-emerald-600 px-3 py-2 text-sm text-white shadow-sm">
            <p className="whitespace-pre-wrap break-words">{entry.text}</p>
          </div>
        </div>
      );
    case "text":
      // agent 纯文本：左对齐 + 头像/名称/时间标识行（与用户消息明确分层），
      // 右上角整段复制（桌面 hover / 移动常显可长按）
      return (
        <div className="group flex items-start gap-2">
          <AgentBadge />
          <div className="min-w-0 max-w-[calc(100%-2rem)] flex-1">
            <div className="flex items-center gap-1.5">
              <span className="text-[11px] font-medium leading-4 text-faint">imagent</span>
              {entry.at ? (
                <span className="text-[11px] leading-4 text-ghost">{fmtTime(entry.at)}</span>
              ) : null}
              <CopyReveal>
                <CopyButton getText={() => entry.text} label="复制整段" />
              </CopyReveal>
            </div>
            <div className="rounded-2xl rounded-bl-md bg-panel2 px-3 py-2 text-sm text-ink">
              <MarkdownText source={entry.text} />
            </div>
          </div>
        </div>
      );
    case "card":
      return <CardView entry={entry} />;
    case "approval_note":
      return (
        <div className="flex justify-center">
          <span className="rounded-full border border-line bg-panel px-3 py-1 text-xs text-muted">
            ⚑ 审批{entry.text}
            {entry.message ? ` · ${entry.message}` : ""}
          </span>
        </div>
      );
  }
});

/**
 * 当前 conv 的消息流：按到达顺序渲染（用户回显 / text / card / 审批记录）。
 * 滚动跟随：用户在底部附近（距底 <120px）时新内容自动滚到底；上滚查看历史
 * 即停止跟随，右下角浮出「回到最新」按钮（点击回底并恢复跟随）。
 * 空态（onSample 传入时）：3 个可点示例 prompt，点击即发送。
 */
export function MessageList({
  entries,
  onSample,
}: {
  entries: StreamEntry[];
  /** 空态示例 prompt 点击发送（App 仅连接 ready 时传入）。 */
  onSample?: (text: string) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // 跟随态双份：ref 供 scroll/effect 免重渲读取，state 驱动按钮显隐
  const stickRef = useRef(true);
  const [following, setFollowing] = useState(true);

  const nearBottom = (el: HTMLElement): boolean =>
    el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_BOTTOM_PX;

  const onScroll = (e: UIEvent<HTMLDivElement>) => {
    const near = nearBottom(e.currentTarget);
    stickRef.current = near;
    setFollowing(near);
  };

  const backToLatest = () => {
    const el = ref.current;
    stickRef.current = true;
    setFollowing(true);
    if (!el) {
      return;
    }
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    // 兜底：个别 webview 的 smooth 滚动是 no-op，500ms 后仍未到底则瞬时回底
    window.setTimeout(() => {
      const cur = ref.current;
      if (stickRef.current && cur && cur.scrollHeight - cur.scrollTop - cur.clientHeight > NEAR_BOTTOM_PX) {
        cur.scrollTop = cur.scrollHeight;
      }
    }, 500);
  };

  useEffect(() => {
    const el = ref.current;
    if (el && stickRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [entries]);

  return (
    <div className="relative h-full">
      <div ref={ref} onScroll={onScroll} className="h-full overflow-y-auto overscroll-contain px-3 py-4 md:px-6">
        <div className="mx-auto max-w-3xl space-y-3">
          {entries.length === 0 && (
            <div className="pt-16 text-center text-sm text-ghost">
              <p>向你的 Mac 发送第一条消息</p>
              <p className="mt-1 text-xs">Enter 发送 · Shift+Enter 换行</p>
              {onSample && (
                <div className="mx-auto mt-6 flex max-w-xs flex-col gap-2">
                  {SAMPLE_PROMPTS.map((p) => (
                    <button
                      key={p}
                      type="button"
                      onClick={() => onSample(p)}
                      className="press rounded-full border border-line-strong bg-panel px-4 py-2 text-sm text-muted transition-colors hover:border-emerald-500/60 hover:text-ink"
                    >
                      {p}
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
          {entries.map((entry) => (
            <EntryView key={entry.key} entry={entry} />
          ))}
        </div>
      </div>
      {!following && (
        <button
          type="button"
          onClick={backToLatest}
          className="press absolute bottom-4 right-4 z-30 flex items-center gap-1.5 rounded-full border border-line-strong bg-panel px-3.5 py-2 text-xs font-medium text-muted shadow-lg transition-colors hover:text-ink"
          aria-label="回到最新消息"
        >
          <span aria-hidden="true">↓</span> 回到最新
        </button>
      )}
    </div>
  );
}
