import { useCallback, useEffect, useRef, useState } from "react";
import { copyText } from "../lib/clipboard";

/**
 * 复制按钮（消息整段复制 / 代码块复制共用）：
 * - 点击即复制；按住 ≥500ms 亦触发（移动端长按主路径）；
 * - 结果反馈 1.5s（已复制 / 复制失败）后回默认文案；
 * - 桌面 hover 显隐由外层容器 group + 包裹 span 的 opacity 类控制（本组件不管）。
 */
export function CopyButton({
  getText,
  label = "复制",
  copiedLabel = "已复制",
  className = "",
}: {
  /** 惰性取值：点击瞬间才读最新文本（流式内容复制到当刻）。 */
  getText: () => string;
  /** 无障碍名（aria-label，恒定不随状态变）。 */
  label?: string;
  copiedLabel?: string;
  className?: string;
}) {
  const [state, setState] = useState<"idle" | "ok" | "fail">("idle");
  const resetTimer = useRef<number>(0);
  const pressTimer = useRef<number>(0);

  // 卸载清定时器（防 setState 到已卸载组件 + 长按残留）
  useEffect(() => {
    return () => {
      window.clearTimeout(resetTimer.current);
      window.clearTimeout(pressTimer.current);
    };
  }, []);

  const doCopy = useCallback(async () => {
    const ok = await copyText(getText());
    setState(ok ? "ok" : "fail");
    window.clearTimeout(resetTimer.current);
    resetTimer.current = window.setTimeout(() => setState("idle"), 1500);
  }, [getText]);

  const startPress = () => {
    window.clearTimeout(pressTimer.current);
    pressTimer.current = window.setTimeout(() => void doCopy(), 500);
  };
  const endPress = () => window.clearTimeout(pressTimer.current);

  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation(); // 不冒泡触发宿主（会话项/卡片）的点击语义
        void doCopy();
      }}
      onPointerDown={startPress}
      onPointerUp={endPress}
      onPointerLeave={endPress}
      onPointerCancel={endPress}
      // 长按会弹出系统文本选择菜单遮住按钮，长按路径下抑制（点击不受影响）
      onContextMenu={(e) => e.preventDefault()}
      aria-label={label}
      className={
        "press shrink-0 rounded-md px-1.5 py-0.5 text-[11px] leading-4 transition-colors hover:bg-panel2 hover:text-ink " +
        className
      }
    >
      {state === "ok" ? copiedLabel : state === "fail" ? "复制失败" : label}
    </button>
  );
}
