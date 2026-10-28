import { useEffect, useReducer, useRef, useState } from "react";
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
import { ConvTabs } from "./components/ConvTabs";
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

const EMPTY_CONV: ConvState = { entries: [], pending: [] };

export default function App() {
  const [token, setToken] = useState<string | null>(() => localStorage.getItem(TOKEN_KEY));
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [currentConv, setCurrentConv] = useState(DEFAULT_CONV);
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

  const conv = state.convs[currentConv] ?? EMPTY_CONV;
  const allPending = state.convOrder.flatMap((c) => state.convs[c]?.pending ?? []);

  return (
    <div className="flex h-dvh flex-col">
      <StatusBar
        status={status}
        machineOnline={state.machineOnline}
        currentConv={currentConv}
        onLogout={logout}
      />
      <ConvTabs convs={state.convOrder} current={currentConv} onSelect={setCurrentConv} />
      <main className="relative min-h-0 flex-1">
        <ApprovalPanel pending={allPending} onRespond={respondApproval} />
        <MessageList entries={conv.entries} />
      </main>
      <Composer disabled={status !== "ready"} onSend={sendMessage} onStop={() => {
        wsRef.current?.sendControl("stop");
      }} />
      {state.lastError && (
        <div
          role="status"
          className="fixed inset-x-0 bottom-24 z-50 mx-auto w-fit max-w-[90vw] rounded-full border border-rose-500/40 bg-rose-950/95 px-4 py-2 text-center text-xs text-rose-200 shadow-xl"
        >
          {ERROR_HINTS[state.lastError.code] ?? `中继错误：${state.lastError.code}`}
        </div>
      )}
    </div>
  );
}
