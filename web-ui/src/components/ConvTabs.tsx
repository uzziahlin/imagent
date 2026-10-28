import { convDisplayName } from "../lib/protocol";

/** 会话切换条：从收到的帧里收集 conv 集合（web:default 恒存在），横向滚动。 */
export function ConvTabs({
  convs,
  current,
  onSelect,
}: {
  convs: string[];
  current: string;
  onSelect: (conv: string) => void;
}) {
  return (
    <nav
      className="flex gap-1 overflow-x-auto border-b border-slate-800 bg-slate-900/50 px-2 py-1.5"
      aria-label="会话列表"
    >
      {convs.map((conv) => {
        const active = conv === current;
        return (
          <button
            key={conv}
            onClick={() => onSelect(conv)}
            className={
              "shrink-0 rounded-full px-3 py-1 font-mono text-xs transition-colors " +
              (active
                ? "bg-emerald-600 font-semibold text-white"
                : "bg-slate-800 text-slate-400 hover:bg-slate-700 hover:text-slate-200")
            }
          >
            {convDisplayName(conv)}
          </button>
        );
      })}
    </nav>
  );
}
