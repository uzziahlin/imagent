import { useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";

/**
 * 输入区自适应高度上限：6 行（text-base 行高 24px × 6 + 上下 padding 20px =
 * 164px）。text-base（16px）：字号 <16px 时 iOS Safari 聚焦会自动放大整页。
 */
const MAX_TEXTAREA_HEIGHT = 164;

/**
 * composer：多行输入，Enter 发送 / Shift+Enter 换行（中文输入法组词中的
 * Enter 不发送）；「停止」发 control stop。发送即本地回显（上层 send_echo）。
 * 输入区高度随内容自适应（1-6 行，超出滚动）；底部安全区 padding（刘海屏
 * home bar 场景，移动端优先）。
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
  const taRef = useRef<HTMLTextAreaElement>(null);

  // 内容变化即重算高度（发送清空后回缩到 1 行）
  useLayoutEffect(() => {
    const el = taRef.current;
    if (!el) {
      return;
    }
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT)}px`;
  }, [text]);

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
      className="border-t border-line bg-panel px-3 pt-2 backdrop-blur"
      style={{ paddingBottom: "max(env(safe-area-inset-bottom), 0.5rem)" }}
    >
      <div className="mx-auto flex w-full max-w-3xl items-end gap-2">
        <textarea
          ref={taRef}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          rows={1}
          placeholder={disabled ? "连接中…（恢复后可发送）" : "发消息给 Mac…（Enter 发送）"}
          disabled={disabled}
          className="max-h-[164px] min-h-[2.75rem] flex-1 resize-none overflow-y-auto rounded-xl border border-line-strong bg-surface px-3 py-2.5 text-base leading-6 text-ink outline-none placeholder:text-ghost focus:border-emerald-500 disabled:opacity-50"
        />
        <button
          onClick={submit}
          disabled={disabled || !text.trim()}
          className="h-11 shrink-0 rounded-xl bg-emerald-600 px-3 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 active:bg-emerald-700 disabled:opacity-40 md:px-4"
        >
          发送
        </button>
        <button
          onClick={onStop}
          disabled={disabled}
          title="停止当前任务（control stop）"
          className="h-11 shrink-0 rounded-xl border border-line-strong px-3 text-sm text-muted transition-colors hover:border-rose-400/60 hover:text-err disabled:opacity-40 dark:border-slate-700"
        >
          停止
        </button>
      </div>
    </div>
  );
}
