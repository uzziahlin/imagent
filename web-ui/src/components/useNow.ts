import { useEffect, useState } from "react";

/**
 * 每秒跳动的当前时刻（Date.now()）：配对等待码 / 配对请求卡的倒计时用。
 * 无消费者倒计时语义时避免滥用——纯展示 ticking。
 */
export function useNow(intervalMs = 1_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(t);
  }, [intervalMs]);
  return now;
}

/** 剩余毫秒 → "m:ss" 倒计时文案（负值按 0 处理）。 */
export function countdownText(remainMs: number): string {
  const total = Math.max(0, Math.floor(remainMs / 1_000));
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}
