import { useEffect, useMemo, useReducer, useRef, useState } from "react";
import {
  DEFAULT_CONV,
  DEFAULT_MACHINE,
  initialState,
  reducer,
  spaceOf,
  type ApprovalDecision,
  type ConvState,
  type MachineNotice,
  type PendingApproval,
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
  ambiguous_machine: "多台机器在线，未指定目标机器",
  machine_offline: "该机器离线，消息未送达",
};

/** 机器通知文案：from 为 default 的引导切换并非真实掉线，单独措辞。 */
function noticeText(n: MachineNotice): string {
  switch (n.kind) {
    case "switched":
      return n.from === DEFAULT_MACHINE
        ? `已连接机器 ${n.to}`
        : `${n.from} 已离线，已切换到 ${n.to}`;
    case "offline":
      return `${n.machine} 已离线（暂无其它在线机器）`;
  }
}

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
  // state 的渲染后镜像：WsManager 回调（getMachine）需要读最新 machine 维度，
  // 不能闭包进构建时的旧 state。
  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  });

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
      // v1.1：内部自动帧（ready 后 sync / 心跳 ping）的出站机器。
      // 多机模式恒带当前选中机器；旧 relay 单机模式不带（字节级不变）。
      getMachine: () => {
        const s = stateRef.current;
        return s.machinesKnown ? s.currentMachine : undefined;
      },
    });
    wsRef.current = mgr;
    mgr.connect();
    return () => {
      wsRef.current = null;
      mgr.close();
    };
  }, [token]);

  // error / notice toast：4s 自动消退
  useEffect(() => {
    if (!state.lastError) {
      return;
    }
    const t = setTimeout(() => dispatch({ type: "clear_error" }), 4000);
    return () => clearTimeout(t);
  }, [state.lastError]);

  useEffect(() => {
    if (!state.lastNotice) {
      return;
    }
    const t = setTimeout(() => dispatch({ type: "clear_notice" }), 4000);
    return () => clearTimeout(t);
  }, [state.lastNotice]);

  // 机器维度变化（首收 machines 帧 / 侧栏切换 / 掉线自动切换）→ 发一次带
  // machine 的 sync 拉该机器卡片缓存。未 ready 时 sendSync 静默失败——重连
  // 场景由 ws 层内部 sync（getMachine 已带当前机器）兜底。旧 relay 单机模式
  // （machinesKnown 恒 false）不触发，行为与升级前完全一致。
  useEffect(() => {
    if (state.machinesKnown) {
      wsRef.current?.sendSync(state.currentMachine);
    }
  }, [state.machinesKnown, state.currentMachine]);

  // 侧边栏会话项：当前选中机器会话空间内的全部 conv；刚切到尚未收到帧的 conv
  // （新会话）不在 convOrder 里，前端补一项空预览，避免当前高亮落空。
  // 注意：hook 必须在下方 token 早退之前（hooks 数量不可随登录态变化）。
  const space = spaceOf(state, state.currentMachine);
  const convItems = useMemo<ConvItem[]>(() => {
    const items = space.convOrder.map((c) => ({
      convId: c,
      preview: space.convs[c]?.lastPreview ?? "",
      at: space.convs[c]?.lastAt ?? 0,
    }));
    if (!items.some((i) => i.convId === currentConv)) {
      items.unshift({ convId: currentConv, preview: "", at: 0 });
    }
    return items;
  }, [space, currentConv]);

  // 全部机器的 pending 审批（弹层按机器分组徽标）：在线机器按 machines 序在
  // 前，其余空间（default / 已离线机器）殿后；组内按各 conv 到达序。
  // 注意：machines 帧先于该机器任何消息帧到达时，spaces 尚无该机器键，必须
  // 经 spaceOf 取只读空空间兜底（否则 undefined.convOrder 直接炸掉整树渲染）。
  const allPending = useMemo<PendingApproval[]>(() => {
    const order: string[] = [...state.machines];
    for (const m of Object.keys(state.spaces)) {
      if (!order.includes(m)) {
        order.push(m);
      }
    }
    const out: PendingApproval[] = [];
    for (const m of order) {
      const sp = spaceOf(state, m);
      for (const c of sp.convOrder) {
        out.push(...(sp.convs[c]?.pending ?? []));
      }
    }
    return out;
  }, [state.machines, state.spaces]);

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

  /** 出站帧 machine 字段：多机模式恒带当前选中机器（防 ambiguous_machine）；
   *  旧 relay 单机模式不带（帧字节与升级前完全一致）。 */
  const outMachine = (): string | undefined =>
    state.machinesKnown ? state.currentMachine : undefined;

  const sendMessage = (text: string): boolean => {
    const ok = wsRef.current?.sendMessage(currentConv, text, outMachine()) ?? false;
    if (ok) {
      dispatch({
        type: "send_echo",
        convId: currentConv,
        text,
        machine: state.currentMachine,
      });
    }
    return ok;
  };

  /** 新会话：/switch <名字> 命令文本走 message 帧（convId 留空落网关默认
   *  会话），同时本地把当前 conv 切到 web:<名字>；命令回显留在默认会话。 */
  const createConv = (name: string): boolean => {
    const ok = wsRef.current?.sendMessage(undefined, `/switch ${name}`, outMachine()) ?? false;
    if (ok) {
      dispatch({
        type: "send_echo",
        convId: DEFAULT_CONV,
        text: `/switch ${name}`,
        machine: state.currentMachine,
      });
      setCurrentConv(toConvId(name));
      setDrawerOpen(false);
    }
    return ok;
  };

  /** 审批回传带审批来源机器（可能是非当前选中机器的审批）。 */
  const outMachineFor = (p: PendingApproval): string | undefined =>
    state.machinesKnown ? p.machine : undefined;

  const respondApproval = (p: PendingApproval, decision: ApprovalDecision) => {
    // convId / requestId 原样回传 approval_request 帧里的值
    if (wsRef.current?.sendApprovalResponse(p.convId, p.requestId, decision, outMachineFor(p))) {
      dispatch({
        type: "approval_sent",
        convId: p.convId,
        requestId: p.requestId,
        machine: p.machine,
      });
    }
  };

  /** 侧栏机器切换：会话池 per-machine，当前 conv 回到该机器默认会话；
   *  sync 由机器维度 effect 统一发出。 */
  const selectMachine = (m: string) => {
    if (m !== state.currentMachine) {
      dispatch({ type: "select_machine", machine: m });
      setCurrentConv(DEFAULT_CONV);
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

  const conv = space.convs[currentConv] ?? EMPTY_CONV;

  const sidebar = (onSelect: (conv: string) => void) => (
    <Sidebar
      convs={convItems}
      current={currentConv}
      machineOnline={state.machineOnline}
      machines={state.machines}
      machinesKnown={state.machinesKnown}
      currentMachine={state.currentMachine}
      onSelect={onSelect}
      onSelectMachine={selectMachine}
      onCreate={createConv}
    />
  );

  // h-full 跟随 #root（index.css：var(--vvh, 100dvh)，键盘收展平滑跟随）
  return (
    <div className="flex h-full bg-surface text-ink">
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
          machineOnline={
            state.machinesKnown
              ? state.machines.includes(state.currentMachine)
              : state.machineOnline
          }
          machineName={state.machinesKnown ? state.currentMachine : undefined}
          currentConv={currentConv}
          onLogout={logout}
          onReconnect={reconnect}
          onOpenDrawer={() => setDrawerOpen(true)}
        />
        <main className="relative min-h-0 flex-1">
          <ApprovalPanel
            pending={allPending}
            showMachine={state.machinesKnown}
            onRespond={respondApproval}
          />
          <MessageList entries={conv.entries} />
        </main>
        <Composer
          disabled={status !== "ready"}
          onSend={sendMessage}
          onStop={() => {
            wsRef.current?.sendControl("stop", undefined, outMachine());
          }}
        />
      </div>

      {(state.lastError || state.lastNotice) && (
        <div className="fixed inset-x-0 bottom-24 z-50 mx-auto flex w-fit max-w-[90vw] flex-col items-center gap-2">
          {state.lastNotice && (
            <div
              role="status"
              className="rounded-full border border-sky-400/60 bg-sky-50 px-4 py-2 text-center text-xs text-sky-800 shadow-xl dark:border-sky-500/40 dark:bg-sky-950/95 dark:text-sky-200"
            >
              {noticeText(state.lastNotice)}
            </div>
          )}
          {state.lastError && (
            <div
              role="status"
              className="rounded-full border border-rose-400/60 bg-rose-50 px-4 py-2 text-center text-xs text-rose-800 shadow-xl dark:border-rose-500/40 dark:bg-rose-950/95 dark:text-rose-200"
            >
              {ERROR_HINTS[state.lastError.code] ?? `中继错误：${state.lastError.code}`}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
