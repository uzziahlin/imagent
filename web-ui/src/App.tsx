import { useEffect, useMemo, useReducer, useRef, useState } from "react";
import {
  DEFAULT_CONV,
  initialState,
  reducer,
  type ApprovalDecision,
  type ConvState,
} from "./lib/protocol";
import { WsManager, type ConnStatus } from "./lib/ws";
import { Login } from "./components/Login";
import { StatusBar } from "./components/StatusBar";
import { Sidebar, type ConvItem } from "./components/Sidebar";
import { MessageList } from "./components/MessageList";
import { ApprovalPanel } from "./components/ApprovalPanel";
import { Composer } from "./components/Composer";

/** token 存储位置（安全红线：只存 localStorage、只经 hello 帧发送，绝不进 URL）。 */
const TOKEN_KEY = "imagent.userToken";

/** WS 地址同源推导：https 页面 → wss，BASE_URL（/imagent/）+ ws。 */
function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${import.meta.env.BASE_URL}ws`;
}

const ERROR_HINTS: Record<string, string> = {
  gateway_offline: "Mac 网关离线且中继缓冲已满，消息未送达",
  rate_limited: "发送过快，已被限流断开",
};

const EMPTY_CONV: ConvState = { entries: [], pending: [], lastPreview: "", lastAt: 0 };

/** conv 全名（用户输入的名字统一补 web: 前缀，幂等）。 */
function toConvId(name: string): string {
  return name.startsWith("web:") ? name : `web:${name}`;
}

export default function App() {
  const [token, setToken] = useState<string | null>(() => localStorage.getItem(TOKEN_KEY));
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [currentConv, setCurrentConv] = useState(DEFAULT_CONV);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const wsRef = useRef<WsManager | null>(null);

  // token 变化（登录/退出/认证失败）→ 重建 WS 连接
  useEffect(() => {
    if (!token) {
      return;
    }
    const mgr = new WsManager({
      url: wsUrl(),
      token,
      onFrame: (frame) => dispatch({ type: "frame", frame }),
      onStatus: setStatus,
      onAuthFailed: () => {
        // relay close 1008：token 无效 → 清 token 回登录页
        localStorage.removeItem(TOKEN_KEY);
        setToken(null);
      },
    });
    wsRef.current = mgr;
    mgr.connect();
    return () => {
      wsRef.current = null;
      mgr.close();
    };
  }, [token]);

  // error 帧 toast：4s 自动消退
  useEffect(() => {
    if (!state.lastError) {
      return;
    }
    const t = setTimeout(() => dispatch({ type: "clear_error" }), 4000);
    return () => clearTimeout(t);
  }, [state.lastError]);

  // 侧边栏会话项：state 收集的全部 conv；刚切到尚未收到帧的 conv（新会话）
  // 不在 convOrder 里，前端补一项空预览，避免当前高亮落空。
  // 注意：hook 必须在下方 token 早退之前（hooks 数量不可随登录态变化）。
  const convItems = useMemo<ConvItem[]>(() => {
    const items = state.convOrder.map((c) => ({
      convId: c,
      preview: state.convs[c]?.lastPreview ?? "",
      at: state.convs[c]?.lastAt ?? 0,
    }));
    if (!items.some((i) => i.convId === currentConv)) {
      items.unshift({ convId: currentConv, preview: "", at: 0 });
    }
    return items;
  }, [state.convOrder, state.convs, currentConv]);

  if (!token) {
    return (
      <Login
        onSave={(t) => {
          localStorage.setItem(TOKEN_KEY, t);
          setToken(t);
        }}
      />
    );
  }

  const sendMessage = (text: string): boolean => {
    const ok = wsRef.current?.sendMessage(currentConv, text) ?? false;
    if (ok) {
      dispatch({ type: "send_echo", convId: currentConv, text });
    }
    return ok;
  };

  /** 新会话：/switch <名字> 命令文本走 message 帧（convId 留空落网关默认
   *  会话），同时本地把当前 conv 切到 web:<名字>；命令回显留在默认会话。 */
  const createConv = (name: string): boolean => {
    const ok = wsRef.current?.sendMessage(undefined, `/switch ${name}`) ?? false;
    if (ok) {
      dispatch({ type: "send_echo", convId: DEFAULT_CONV, text: `/switch ${name}` });
      setCurrentConv(toConvId(name));
      setDrawerOpen(false);
    }
    return ok;
  };

  const respondApproval = (convId: string, requestId: string, decision: ApprovalDecision) => {
    // convId / requestId 原样回传 approval_request 帧里的值
    if (wsRef.current?.sendApprovalResponse(convId, requestId, decision)) {
      dispatch({ type: "approval_sent", convId, requestId });
    }
  };

  const logout = () => {
    localStorage.removeItem(TOKEN_KEY);
    setToken(null);
  };

  // superseded 态用户显式重连：复用同一 WsManager（connect 会重置终态并新建
  // 连接），新连接会反过来顶替另一窗口——用户显式意图，允许
  const reconnect = () => {
    wsRef.current?.connect();
  };

  const conv = state.convs[currentConv] ?? EMPTY_CONV;
  const allPending = state.convOrder.flatMap((c) => state.convs[c]?.pending ?? []);

  const sidebar = (onSelect: (conv: string) => void) => (
    <Sidebar
      convs={convItems}
      current={currentConv}
      machineOnline={state.machineOnline}
      onSelect={onSelect}
      onCreate={createConv}
    />
  );

  return (
    <div className="flex h-dvh bg-surface text-ink">
      {/* 桌面（≥768px）：左侧固定侧边栏 240px */}
      <aside className="hidden w-60 shrink-0 border-r border-line bg-panel md:block">
        {sidebar(setCurrentConv)}
      </aside>

      {/* 移动（<768px）：侧边栏变抽屉（遮罩点击关闭） */}
      {drawerOpen && (
        <div className="fixed inset-0 z-50 md:hidden">
          <div
            className="absolute inset-0 bg-black/50"
            onClick={() => setDrawerOpen(false)}
            aria-hidden="true"
          />
          <aside className="absolute inset-y-0 left-0 flex w-72 max-w-[85vw] flex-col border-r border-line bg-surface shadow-2xl">
            {sidebar((c) => {
              setCurrentConv(c);
              setDrawerOpen(false);
            })}
          </aside>
        </div>
      )}

      {/* 右侧聊天区（移动全屏单列） */}
      <div className="flex min-w-0 flex-1 flex-col">
        <StatusBar
          status={status}
          machineOnline={state.machineOnline}
          currentConv={currentConv}
          onLogout={logout}
          onReconnect={reconnect}
          onOpenDrawer={() => setDrawerOpen(true)}
        />
        <main className="relative min-h-0 flex-1">
          <ApprovalPanel pending={allPending} onRespond={respondApproval} />
          <MessageList entries={conv.entries} />
        </main>
        <Composer
          disabled={status !== "ready"}
          onSend={sendMessage}
          onStop={() => {
            wsRef.current?.sendControl("stop");
          }}
        />
      </div>

      {state.lastError && (
        <div
          role="status"
          className="fixed inset-x-0 bottom-24 z-50 mx-auto w-fit max-w-[90vw] rounded-full border border-rose-400/60 bg-rose-50 px-4 py-2 text-center text-xs text-rose-800 shadow-xl dark:border-rose-500/40 dark:bg-rose-950/95 dark:text-rose-200"
        >
          {ERROR_HINTS[state.lastError.code] ?? `中继错误：${state.lastError.code}`}
        </div>
      )}
    </div>
  );
}
