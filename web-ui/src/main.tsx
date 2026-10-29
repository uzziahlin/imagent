import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import "./index.css";

/*
 * 移动端键盘高度跟随：iOS Safari / Android Chrome（默认 resizes-visual）下
 * 软键盘只缩小 visualViewport，100dvh 不收缩，聚焦底部输入框时页面被顶起、
 * 失焦弹回。这里把 visualViewport 实际高度写入 --vvh（index.css 满高容器
 * 消费），键盘收展时布局高度平滑跟随。双指缩放（scale≠1，页面 meta 未禁用
 * 的无障碍能力）时不改布局高度，避免捏合缩放导致容器塌缩。
 * index.css 已锁 document 滚动（overflow:hidden），这里再在键盘收展瞬间
 * scrollTo(0,0) 防御性归位：任何原因导致 document 出现 scrollY 残留即清零。
 */
function trackViewportHeight(): void {
  const vv = window.visualViewport;
  if (!vv) {
    return; // 无 visualViewport 的旧浏览器：CSS 侧回退 100dvh / 100vh
  }
  const update = () => {
    if (vv.scale === 1) {
      document.documentElement.style.setProperty("--vvh", `${vv.height}px`);
      // 防御性归位：任何原因导致 document 出现 scrollY 残留，键盘收展瞬间即清零
      window.scrollTo(0, 0);
    } else {
      document.documentElement.style.removeProperty("--vvh");
    }
  };
  vv.addEventListener("resize", update);
  update();
}
trackViewportHeight();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
