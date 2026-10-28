import { memo, useEffect, useRef, useState, type UIEvent } from "react";
import type { StreamEntry } from "../lib/protocol";
import { CardView } from "./CardView";
import { MarkdownText } from "./MarkdownText";

/** 距底小于该值视为「在底部附近」，新内容自动跟随。 */
const NEAR_BOTTOM_PX = 120;

/** 单条消息：memo 按条目引用比较——card_patch 只换对应 card 条目的引用，
 *  其余条目（text/user/approval_note，reducer 不可变更新保留引用）跳过重渲。 */
const EntryView = memo(function EntryView({ entry }: { entry: StreamEntry }) {
  switch (entry.kind) {
    case "user":
      return (
        <div className="flex justify-end">
          <div className="max-w-[85%] rounded-2xl rounded-br-md bg-emerald-100 px-3 py-2 text-sm text-emerald-900 dark:bg-emerald-900/50 dark:text-emerald-50">
            <p className="whitespace-pre-wrap break-words">{entry.text}</p>
          </div>
        </div>
      );
    case "text":
      return (
        <div className="flex justify-start">
          <div className="max-w-[92%] rounded-2xl rounded-bl-md bg-panel2 px-3 py-2 text-sm text-ink">
            <MarkdownText source={entry.text} />
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
 */
export function MessageList({ entries }: { entries: StreamEntry[] }) {
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
          className="absolute bottom-4 right-4 z-30 flex items-center gap-1.5 rounded-full border border-line-strong bg-panel px-3.5 py-2 text-xs font-medium text-muted shadow-lg transition-colors hover:text-ink"
          aria-label="回到最新消息"
        >
          <span aria-hidden="true">↓</span> 回到最新
        </button>
      )}
    </div>
  );
}
