import { useState, type KeyboardEvent } from "react";

/**
 * composer：多行输入，Enter 发送 / Shift+Enter 换行（中文输入法组词中的
 * Enter 不发送）；「停止」发 control stop。发送即本地回显（上层 send_echo）。
 * 底部安全区 padding（刘海屏 home bar 场景，移动端优先）。
 */
export function Composer({
  disabled,
  onSend,
  onStop,
}: {
  disabled: boolean;
  onSend: (text: string) => boolean;
  onStop: () => void;
}) {
  const [text, setText] = useState("");

  const submit = () => {
    const t = text.trim();
    if (!t) {
      return;
    }
    if (onSend(t)) {
      setText("");
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      submit();
    }
  };

  return (
    <div
      className="border-t border-slate-800 bg-slate-900/90 px-3 pt-2 backdrop-blur"
      style={{ paddingBottom: "max(env(safe-area-inset-bottom), 0.5rem)" }}
    >
      <div className="mx-auto flex w-full max-w-3xl items-end gap-2">
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          rows={1}
          placeholder={disabled ? "连接中…（恢复后可发送）" : "发消息给 Mac…（Enter 发送）"}
          disabled={disabled}
          className="max-h-40 min-h-[2.75rem] flex-1 resize-y rounded-xl border border-slate-700 bg-slate-950 px-3 py-2.5 text-sm text-slate-100 outline-none placeholder:text-slate-600 focus:border-emerald-500 disabled:opacity-50"
        />
        <button
          onClick={submit}
          disabled={disabled || !text.trim()}
          className="h-11 shrink-0 rounded-xl bg-emerald-600 px-4 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700 disabled:opacity-40"
        >
          发送
        </button>
        <button
          onClick={onStop}
          disabled={disabled}
          title="停止当前任务（control stop）"
          className="h-11 shrink-0 rounded-xl border border-slate-700 px-3 text-sm text-slate-300 transition-colors hover:border-rose-500/50 hover:text-rose-300 disabled:opacity-40"
        >
          停止
        </button>
      </div>
    </div>
  );
}
