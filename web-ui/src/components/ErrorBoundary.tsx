import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * 顶层错误边界（产品加固）：捕获 React 渲染期抛错，显示可读错误页替代静默
 * 白屏（React 18 createRoot 无默认边界，渲染抛错会卸载整树 → #root 空）。
 * 错误信息只在本地 console 与页面显示，不外发任何网络请求。
 * 「重试」按钮整页 reload（SPA 无本地持久状态，重载即恢复到登录/连接首帧）。
 */
type Props = { children: ReactNode };
type State = { error: Error | null };

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // 仅本地 console 保留现场（不外发）
    console.error("[ErrorBoundary] 页面渲染崩溃：", error, info.componentStack);
  }

  render() {
    if (!this.state.error) {
      return this.props.children;
    }
    return (
      <div
        role="alert"
        className="flex h-dvh items-center justify-center bg-surface p-6 text-ink"
      >
        <div className="w-full max-w-md rounded-2xl border border-rose-400/50 bg-panel p-6 shadow-2xl">
          <h1 className="mb-2 text-lg font-semibold">页面渲染出错</h1>
          <p className="mb-4 text-sm leading-relaxed text-muted">
            界面遇到意外错误已停止渲染。点击「重试」重新加载页面；若反复出现，
            请检查网关与中继（relay）版本是否匹配后重试。
          </p>
          <pre className="mb-4 max-h-40 overflow-auto rounded-lg bg-code p-2.5 font-mono text-xs break-all whitespace-pre-wrap text-faint">
            {this.state.error.message}
          </pre>
          <button
            type="button"
            onClick={() => location.reload()}
            className="w-full rounded-xl bg-emerald-600 px-4 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500"
          >
            重试
          </button>
        </div>
      </div>
    );
  }
}
