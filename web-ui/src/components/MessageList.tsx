import { useEffect, useRef } from "react";
import type { StreamEntry } from "../lib/protocol";
import { CardView } from "./CardView";
import { MarkdownText } from "./MarkdownText";

function EntryView({ entry }: { entry: StreamEntry }) {
  switch (entry.kind) {
    case "user":
      return (
        <div className="flex justify-end">
          <div className="max-w-[85%] rounded-2xl rounded-br-md bg-emerald-900/50 px-3 py-2 text-sm text-emerald-50">
            <p className="whitespace-pre-wrap break-words">{entry.text}</p>
          </div>
        </div>
      );
    case "text":
      return (
        <div className="flex justify-start">
          <div className="max-w-[92%] rounded-2xl rounded-bl-md bg-slate-800/60 px-3 py-2 text-sm text-slate-100">
            <MarkdownText source={entry.text} />
          </div>
        </div>
      );
    case "card":
      return <CardView entry={entry} />;
    case "approval_note":
      return (
        <div className="flex justify-center">
          <span className="rounded-full border border-slate-700 bg-slate-900 px-3 py-1 text-xs text-slate-400">
            ⚑ 审批{entry.text}
            {entry.message ? ` · ${entry.message}` : ""}
          </span>
        </div>
      );
  }
}

/** 当前 conv 的消息流：按到达顺序渲染（用户回显 / text / card / 审批记录）。 */
export function MessageList({ entries }: { entries: StreamEntry[] }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [entries]);

  return (
    <div ref={ref} className="h-full overflow-y-auto overscroll-contain px-3 py-4">
      <div className="mx-auto max-w-3xl space-y-3">
        {entries.length === 0 && (
          <div className="pt-16 text-center text-sm text-slate-600">
            <p>向你的 Mac 发送第一条消息</p>
            <p className="mt-1 text-xs">Enter 发送 · Shift+Enter 换行</p>
          </div>
        )}
        {entries.map((entry) => (
          <EntryView key={entry.key} entry={entry} />
        ))}
      </div>
    </div>
  );
}
