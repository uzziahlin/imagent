/**
 * 剪贴板写入（消息整段复制 / 代码块复制的公共底层）：
 * 1. 优先 navigator.clipboard.writeText（异步剪贴板 API，需安全上下文）；
 * 2. 不可用或失败（非安全上下文 http / 权限拒绝）回退 execCommand 同步复制；
 * 3. 均失败返回 false——调用方据此给「复制失败」反馈，绝不静默吞掉。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 权限拒绝 / 文档失焦等：落到 execCommand 再试一次
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    // 固定定位 + 透明：避免回退路径在页面底部撑出滚动或闪烁
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}
