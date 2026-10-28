import { useEffect, useState } from "react";

/**
 * 退出过渡的延迟卸载：open 落 false 后保留挂载 exitMs（让退出动画播完）再卸载。
 * 用于「恒挂载 + 显隐类过渡」与「条件渲染 + 进出动画」的折中——关闭时内容
 * （含可聚焦元素/重复 a11y 名）最终会真正离开 DOM，只多留一个动画周期。
 */
export function useExitMounted(open: boolean, exitMs: number): boolean {
  const [mounted, setMounted] = useState(open);
  useEffect(() => {
    if (open) {
      setMounted(true);
      return;
    }
    const t = window.setTimeout(() => setMounted(false), exitMs);
    return () => window.clearTimeout(t);
  }, [open, exitMs]);
  return mounted;
}
