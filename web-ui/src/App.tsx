import { useEffect, useMemo, useReducer, useRef, useState, type FormEvent } from "react";
import {
  DEFAULT_CONV,
  DEFAULT_MACHINE,
  convTitle,
  deviceSummary,
  initialState,
  newAutoConvName,
  reducer,
  spaceOf,
  type Action,
  type ApprovalDecision,
  type AppState,
  type ConvState,
  type DeviceInfo,
  type MachineNotice,
  type MachineWorkspaces,
  type MediaPayload,
  type PairingRequest,
  type PasskeyInfo,
  type PendingApproval,
  type StreamEntry,
} from "./lib/protocol";
import {
  historyStore,
  restoreHistory,
} from "./lib/historyStore";
import {
  clearSession,
  loadSession,
  saveSession,
  USER_TOKEN_KEY,
  WsManager,
  type AuthFailReason,
  type ConnStatus,
  type StoredSession,
} from "./lib/ws";
import {
  loadPins,
  removePin,
  type E2eEvent,
  type E2ePin,
} from "./lib/e2e";
import {
  PasskeyError,
  passkeyLogin,
  passkeyRegister,
  webauthnSupported,
} from "./lib/webauthn";
import {
  adminTokenExpiresAt,
  clearAdminToken,
  ensureAdminToken,
  getAdminToken,
  reauthErrorText,
} from "./lib/admin";
import { Login } from "./components/Login";
import { StatusBar } from "./components/StatusBar";
import { Sidebar, type ConvItem } from "./components/Sidebar";
import { AdminPanel, RotatedTokenModal } from "./components/AdminPanel";
import { PairingWait } from "./components/PairingWait";
import { PairingRequestCard } from "./components/PairingRequestCard";
import { E2eConfirm } from "./components/E2eConfirm";
import { WorkspacePicker, loadLastDir, saveLastDir } from "./components/WorkspacePicker";
import { useExitMounted } from "./components/useExitMounted";
import { MessageList } from "./components/MessageList";
import { ApprovalPanel } from "./components/ApprovalPanel";
import { Composer } from "./components/Composer";

/** WS 地址同源推导：https 页面 → wss，BASE_URL（/imagent/）+ ws。 */
function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${import.meta.env.BASE_URL}ws`;
}

/**
 * 连接凭证（v1.2 会话层）：登录/登出/认证失败时整体替换（connect effect 的
 * 依赖）；session 签发【不】替换它——WsManager 收到 hello_ack.session 后内部
 * 自持凭证（此后重连 hello 自动用 session 形态），重建连接反而会无谓断开
 * 刚建立好的连接。
 * - user：登录页输入的 USER_TOKEN（一次性，签发 session 即从 localStorage 删除）
 * - session：localStorage 恢复的会话凭证（优先于遗留 userToken）
 */
type Cred = { kind: "user"; token: string; device?: string } | { kind: "session"; token: string };

/** 启动凭证推导：有效 session 优先（并清理遗留 userToken）；否则 userToken
 *  直连（旧 relay 兼容路径）；均无 → 登录页。 */
function loadCred(): Cred | null {
  const session = loadSession();
  if (session) {
    localStorage.removeItem(USER_TOKEN_KEY); // 签发时已删，此处兜底清理旧升级残留
    return { kind: "session", token: session.token };
  }
  const t = localStorage.getItem(USER_TOKEN_KEY);
  return t ? { kind: "user", token: t } : null;
}

const ERROR_HINTS: Record<string, string> = {
  gateway_offline: "Mac 网关离线且中继缓冲已满，消息未送达",
  rate_limited: "发送过快，已被限流断开",
  ambiguous_machine: "多台机器在线，未指定目标机器",
  machine_offline: "该机器离线，消息未送达",
  // v1.7 媒体上行（契约 A 段原文文案；经既有 lastError toast 链路可达）
  media_too_large: "图片超过 8MB 上限",
  media_invalid: "图片无效（解码或保存失败）",
  // v1.5 管理面错误（machine_invalid 带 message 时 toast 只显 code，详情在
  // 管理面板场景由 adminError/表单提示承担）
  machine_invalid: "机器名或公钥无效，或名称已存在",
  admin_auth_required: "管理验证缺失或已过期，请重试操作",
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

const EMPTY_CONV: ConvState = { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 };

/** v1.8 /model 命令发出后延时重拉 workspace_list 的间隔（ms）：留出网关
 *  处理命令 + backend.model() 热设落位的窗口，刷新面板「当前模型」高亮。 */
const MODEL_REFETCH_DELAY_MS = 800;

/** conv 全名（用户输入的名字统一补 web: 前缀，幂等）。 */
function toConvId(name: string): string {
  return name.startsWith("web:") ? name : `web:${name}`;
}

/** v1.4 e2e UI 态（WsManager onE2e 事件驱动）：tofu = 待确认指纹队列（逐台
 *  弹层）；alerts = 身份变化机器（红条）；blocked = fail-closed 机器（发送
 *  停用 + 明文按钮）；active = 加密会话已激活（StatusBar 锁标）；downgrade
 *  = 收到未加密业务帧的机器（黄条——链路可能被降级）。 */
type E2eUi = {
  tofu: { machine: string; gkFp: string; idFp: string }[];
  alerts: Record<string, string>;
  blocked: Record<string, true>;
  active: Record<string, true>;
  downgrade: Record<string, true>;
};

const EMPTY_E2E_UI: E2eUi = { tofu: [], alerts: {}, blocked: {}, active: {}, downgrade: {} };

function omitKey<T extends Record<string, unknown>>(rec: T, key: string): T {
  if (!(key in rec)) {
    return rec;
  }
  const next = { ...rec } as T;
  delete next[key];
  return next;
}

// ---------------------------------------------------------------------------
// v1.7.x 消息本地缓存（IndexedDB，lib/historyStore）：App 级扩展 action +
// 包装 reducer——protocol reducer 既有语义不动，恢复预填的归并在本层完成。
// ---------------------------------------------------------------------------

/** App 扩展 action：挂载恢复（loadAll 输出 → spaces 预填，见 restoreHistory）。 */
type AppAction = Action | { type: "restore_history"; restored: Record<string, Record<string, StreamEntry[]>> };

function appReducer(state: AppState, action: AppAction): AppState {
  if (action.type === "restore_history") {
    return restoreHistory(state, action.restored);
  }
  return reducer(state, action);
}

/** 本地缓存落盘 debounce（ms）：state 稳定 2s 后写脏 conv（高频流式帧不逐帧落盘）。 */
const HISTORY_SAVE_DEBOUNCE_MS = 2000;

/** 本地缓存键（与 historyStore 存储键同构：machine||conv）。 */
const historyKey = (machine: string, convId: string): string => `${machine}||${convId}`;

export default function App() {
  const [auth, setAuth] = useState<Cred | null>(loadCred);
  // 会话凭证（v1.2）：hello_ack.session 签发/恢复时置值；驱动过期横幅、设备
  // 面板「本机」标记与入口显隐（null = 旧 relay 直连模式）。
  const [session, setSession] = useState<StoredSession | null>(() => loadSession());
  // 登录页提示（认证失败分流：会话过期 / 设备被吊销 / 吊销本机登出）
  const [authNotice, setAuthNotice] = useState("");
  // v1.3 passkey：本机 WebAuthn 能力（常量——会话期间不变化）；登录页双入口
  // 与设备管理通行钥匙区的显隐。
  const waSupported = webauthnSupported();
  const [passkeyBusy, setPasskeyBusy] = useState(false);
  const [passkeyError, setPasskeyError] = useState<string | null>(null);
  const [passkeyMessage, setPasskeyMessage] = useState<string | null>(null);
  // v1.5 管理面板（设备管理弹层升级）：打开即拉 devices/creds，机器列表经
  // re-auth（requireAdmin）后 machine_list。
  const [adminOpen, setAdminOpen] = useState(false);
  // re-auth（passkey 管理验证）进行中 / 失败提示 / adminToken 当前有效
  const [adminBusy, setAdminBusy] = useState(false);
  const [adminError, setAdminError] = useState<string | null>(null);
  const [adminActive, setAdminActive] = useState(false);
  const adminExpiryTimer = useRef<number>(0);
  // v1.5 新设备配对（登录页入口）：pairing 非空 → WS 配对模式 + 等待屏
  const [pairing, setPairing] = useState<{ device: string } | null>(null);
  const [pairingCode, setPairingCode] = useState<string | null>(null);
  const [pairingCodeAt, setPairingCodeAt] = useState<number | null>(null);
  const [status, setStatus] = useState<ConnStatus>("connecting");
  const [drawerOpen, setDrawerOpen] = useState(false);
  // v1.6 工作空间：新会话弹层（会话名 + 目录选择）与目录条切换弹层
  const [newConvOpen, setNewConvOpen] = useState(false);
  const [newConvName, setNewConvName] = useState("");
  /** 新会话绑定目录（undefined = 未选；打开弹层时预选 lastdir）。 */
  const [newConvDir, setNewConvDir] = useState<string | undefined>(undefined);
  const [dirPickerOpen, setDirPickerOpen] = useState(false);
  // v1.4 E2E：pin 表快照（设备管理指纹区）+ UI 态（TOFU 队列/告警/阻断/激活）
  const [e2ePins, setE2ePins] = useState<E2ePin[]>(() => loadPins());
  const [e2eUi, setE2eUi] = useState<E2eUi>(EMPTY_E2E_UI);
  const [state, dispatch] = useReducer(appReducer, undefined, initialState);
  const wsRef = useRef<WsManager | null>(null);
  // state 的渲染后镜像：WsManager 回调（getMachine）需要读最新 machine 维度，
  // 不能闭包进构建时的旧 state。
  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  });
  // v1.7.x 本地缓存：上次已落盘（或已恢复）的 conv entries 引用——脏检查用
  // （reducer 不可变更新，引用比较即“内容是否变过”）；退出清空后同步重置。
  const savedRefs = useRef<Map<string, StreamEntry[]>>(new Map());

  /** 挂载恢复：loadAll → spaces 预填（initialState 之后、WS 帧之前；异步竞态
   *  时恢复条目垫在已到帧之前，见 restoreHistory）。加载完成前渲染空态
   *  （现状），加载后一次 setState。 */
  useEffect(() => {
    let cancelled = false;
    void historyStore.loadAll().then((restored) => {
      if (cancelled || Object.keys(restored).length === 0) {
        return;
      }
      for (const [machine, convs] of Object.entries(restored)) {
        for (const [convId, entries] of Object.entries(convs)) {
          // 恢复内容近似“已保存”——首次 flush 可能因 restoreHistory 重建
          // entries 数组而多写一次同内容（幂等、无害，且顺带刷新 savedAt）。
          savedRefs.current.set(historyKey(machine, convId), entries);
        }
      }
      dispatch({ type: "restore_history", restored });
    });
    return () => {
      cancelled = true;
    };
  }, []);

  /** 落盘 debounce：spaces 变化 2s 后写“与上次保存相比 entries 引用有变化”
   *  的 conv（空会话不写）。引用不变（未收到消息的会话）零写入。 */
  useEffect(() => {
    const t = setTimeout(() => {
      for (const [machine, space] of Object.entries(stateRef.current.spaces)) {
        for (const [convId, conv] of Object.entries(space.convs)) {
          if (conv.entries.length === 0) {
            continue;
          }
          const key = historyKey(machine, convId);
          if (savedRefs.current.get(key) === conv.entries) {
            continue;
          }
          savedRefs.current.set(key, conv.entries);
          void historyStore.saveConv(machine, convId, conv.entries);
        }
      }
    }, HISTORY_SAVE_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [state.spaces]);

  /** 退出路径清空缓存后：savedRefs 对齐当前 state 的 entries 引用（否则
   *  debounce flush 会把内存里仍持有的旧会话条目重新写回已清空的存储）。 */
  const syncSavedRefs = () => {
    const next = new Map<string, StreamEntry[]>();
    for (const [machine, space] of Object.entries(stateRef.current.spaces)) {
      for (const [convId, conv] of Object.entries(space.convs)) {
        next.set(historyKey(machine, convId), conv.entries);
      }
    }
    savedRefs.current = next;
  };

  /** 认证失败分流（close 1008 reason → 清对应凭证 + 登录页提示）。v1.5：
   *  adminToken 属会话附属内存态，任何认证失败一并作废。v1.7.x：凭证失效
   *  回登录页 = 退出路径，一并清空本地消息缓存（本机痕迹）。 */
  const authFailed = (reason: AuthFailReason) => {
    clearAdminToken();
    setAdminActive(false);
    setAdminOpen(false);
    setPairing(null);
    setPairingCode(null);
    setPairingCodeAt(null);
    void historyStore.clear();
    syncSavedRefs();
    if (reason === "session_expired") {
      clearSession();
      setSession(null);
      setAuthNotice("会话已过期，请重新登录");
    } else if (reason === "session_revoked") {
      clearSession();
      setSession(null);
      setAuthNotice("此设备已被吊销");
    } else if (reason === "pairing_denied") {
      // v1.5 配对被拒：pending 连接被 relay 关闭，回登录页给明确提示
      setAuthNotice("配对已被拒绝，可重试或改用访问令牌登录");
    } else if (pairing) {
      // 配对中其它认证失败（旧 relay 不识别 pairing hello 等）
      setAuthNotice("配对未成功（中继可能不支持新设备配对）");
    } else {
      // 其它（token 无效等）：全清、无特殊提示（与 v1.1 行为一致）
      clearSession();
      localStorage.removeItem(USER_TOKEN_KEY);
      setSession(null);
      setAuthNotice("");
    }
    setAuth(null);
  };

  /** v1.3 passkey 登录失败文案（登录页回落提示）。 */
  const passkeyErrorText = (e: unknown): string => {
    if (e instanceof PasskeyError) {
      switch (e.code) {
        case "no_credentials":
          return "尚未注册通行钥匙";
        case "no_credential":
          return "未找到可用通行钥匙";
        case "aborted":
          return "已取消或认证器不可用";
        case "rate_limited":
          return "尝试过于频繁，请稍后再试";
        case "network":
          return "网络错误";
        case "verification_failed":
          return "通行钥匙验证失败";
        default:
          return `通行钥匙登录失败（${e.code}）`;
      }
    }
    return "通行钥匙登录失败";
  };

  /** v1.3 passkey 登录：begin → credentials.get → complete → 存 session 直接
   *  进入（与 USER_TOKEN 登录同构：sessionToken 重连，无需再过 hello 换发）。 */
  const handlePasskeyLogin = async () => {
    if (passkeyBusy) {
      return;
    }
    setPasskeyBusy(true);
    setPasskeyError(null);
    try {
      const s = await passkeyLogin();
      saveSession(s);
      setSession(s);
      setAuthNotice("");
      setAuth({ kind: "session", token: s.token });
    } catch (e) {
      setPasskeyError(passkeyErrorText(e));
    } finally {
      setPasskeyBusy(false);
    }
  };

  /** v1.4 e2e 事件 → UI 态（TOFU 队列 / 身份告警 / fail-closed 阻断 / 激活）。 */
  const onE2eEvent = (ev: E2eEvent) => {
    setE2eUi((prev) => {
      switch (ev.kind) {
        case "tofu":
          return {
            ...prev,
            tofu: [...prev.tofu, { machine: ev.machine, gkFp: ev.gkFp, idFp: ev.idFp }],
          };
        case "active":
          return {
            tofu: prev.tofu.filter((t) => t.machine !== ev.machine),
            alerts: omitKey(prev.alerts, ev.machine),
            blocked: omitKey(prev.blocked, ev.machine),
            downgrade: omitKey(prev.downgrade, ev.machine),
            active: { ...prev.active, [ev.machine]: true },
          };
        case "unsupported":
          return {
            ...prev,
            tofu: prev.tofu.filter((t) => t.machine !== ev.machine),
            blocked: { ...prev.blocked, [ev.machine]: true },
            active: omitKey(prev.active, ev.machine),
          };
        case "alert":
          return {
            ...prev,
            tofu: prev.tofu.filter((t) => t.machine !== ev.machine),
            active: omitKey(prev.active, ev.machine),
            alerts: { ...prev.alerts, [ev.machine]: ev.reason },
          };
        case "plaintext":
          return { ...prev, blocked: omitKey(prev.blocked, ev.machine) };
        case "downgrade":
          return { ...prev, downgrade: { ...prev.downgrade, [ev.machine]: true } };
      }
    });
    if (ev.kind === "active") {
      // 首连 pin 落库 / 重试恢复 → 刷新设备管理指纹区快照
      setE2ePins(loadPins());
    }
  };

  // 凭证变化（登录/退出/认证失败）或配对入口开关 → 重建 WS 连接；session 签发
  // 不重建（见 Cred）。v1.5：pairing 非空 = 登录页「我是新设备」配对模式
  // （hello{pairing} 无凭证，等待已登录设备批准；批准后同连接 hello_ack.
  // session 签发 → 切 auth 会话形态重连进主界面）。
  useEffect(() => {
    if (!auth && !pairing) {
      return;
    }
    const mgr = new WsManager({
      url: wsUrl(),
      token: !pairing && auth?.kind === "user" ? auth.token : undefined,
      device: pairing
        ? pairing.device
        : auth?.kind === "user"
          ? auth.device
          : undefined,
      session: !pairing && auth?.kind === "session" ? auth.token : undefined,
      pairing: pairing?.device,
      onFrame: (frame) => dispatch({ type: "frame", frame }),
      onStatus: setStatus,
      onAuthFailed: authFailed,
      // v1.2：hello_ack 携带 session（新 relay）→ 持久化会话凭证，USER_TOKEN
      // 一次性用毕即删（不再长期驻留 localStorage）；旧 relay 不触发。
      // v1.5 配对模式：批准后 relay 在配对连接上换发 session → 存凭证并以
      // 会话形态重连（配对连接由 effect 依赖变化正常收尾）。
      onSession: (s) => {
        saveSession(s);
        localStorage.removeItem(USER_TOKEN_KEY);
        setSession(s);
        if (pairing) {
          setPairing(null);
          setPairingCode(null);
          setPairingCodeAt(null);
          setAuth({ kind: "session", token: s.token });
        }
      },
      // v1.5：配对等待码（大字展示 + 倒计时锚点）
      onPairingPending: (code) => {
        setPairingCode(code);
        setPairingCodeAt(Date.now());
      },
      // v1.1：内部自动帧（ready 后 sync / 心跳 ping）的出站机器。
      // 多机模式恒带当前选中机器；旧 relay 单机模式不带（字节级不变）。
      getMachine: () => {
        const s = stateRef.current;
        return s.machinesKnown ? s.currentMachine : undefined;
      },
      // v1.4：e2e 握手事件上抛（TOFU 弹层 / 身份告警 / fail-closed 横幅）
      onE2e: onE2eEvent,
    });
    wsRef.current = mgr;
    mgr.connect();
    return () => {
      wsRef.current = null;
      mgr.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- authFailed/onE2eEvent 仅用 setState（稳定引用）
  }, [auth, pairing]);

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

  // v1.5：relay 回 admin_auth_required（adminToken 缺失/无效/过期——含本地
  // 时钟偏移导致的提前失效）→ 作废本地 grant 并引导重试（下一次管理操作
  // 自动重新 passkey 验证）。
  useEffect(() => {
    if (state.lastError?.code === "admin_auth_required") {
      clearAdminToken();
      setAdminActive(false);
      setAdminError("管理验证已过期，请重试操作（将重新验证通行钥匙）");
    }
  }, [state.lastError]);

  // 管理面板关闭时的 adminError（配对卡上验证失败等）走底部 toast，4s 消退
  useEffect(() => {
    if (!adminError || adminOpen) {
      return;
    }
    const t = setTimeout(() => setAdminError(null), 4000);
    return () => clearTimeout(t);
  }, [adminError, adminOpen]);

  // 连接 ready（含重连成功）与机器维度变化（首收 machines 帧 / 侧栏切换 /
  // 掉线自动切换）后：发一次带 machine 的 sync 拉该机器卡片缓存 + 一次
  // workspace_list 拉工作空间快照（快照落 reducer，唯一事实源）。重连后的
  // sync 补发与本地 IndexedDB 缓存构成双保险（网关重发在飞卡片终态）。
  // 未 ready 时 send 系列静默失败——由 ws 层内部 sync（getMachine 已带当前
  // 机器）兜底，status 回 ready 后本 effect 再补。仅多机模式（machinesKnown）
  // 发送：旧 relay 单机模式网关不认识这些帧，行为与升级前完全一致。
  useEffect(() => {
    if (state.machinesKnown && status === "ready") {
      wsRef.current?.sendSync(state.currentMachine);
      wsRef.current?.listWorkspaces(state.currentMachine);
    }
  }, [state.machinesKnown, state.currentMachine, status]);

  // 侧边栏会话项：当前选中机器会话空间内的全部 conv；刚切到尚未收到帧的 conv
  // （新会话）不在 convOrder 里，前端补一项空预览，避免当前高亮落空。
  // title = 会话显示名（convTitle：自动会话 web:s-xxxx 与 default 会话用首条
  // 消息派生的自动 title，手动名/local 恒 convId 剥前缀）。
  // 注意：hook 必须在下方 token 早退之前（hooks 数量不可随登录态变化）。
  const space = spaceOf(state, state.currentMachine);
  const currentConv = state.currentConv;
  const convItems = useMemo<ConvItem[]>(() => {
    const items = space.convOrder.map((c) => ({
      convId: c,
      title: convTitle(space.convs[c], c),
      preview: space.convs[c]?.lastPreview ?? "",
      at: space.convs[c]?.lastAt ?? 0,
      unread: space.convs[c]?.unread ?? 0,
    }));
    if (!items.some((i) => i.convId === currentConv)) {
      items.unshift({
        convId: currentConv,
        title: convTitle(space.convs[currentConv], currentConv),
        preview: "",
        at: 0,
        unread: 0,
      });
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

  // 抽屉内容延迟卸载：退出动画播完后真正离开 DOM——避免桌面侧栏与抽屉内
  // 两个 Sidebar 常驻导致重复的可访问名/可聚焦元素（hook 须在 auth 早退前）
  const drawerContentMounted = useExitMounted(drawerOpen, 150);

  if (!auth) {
    // v1.5 配对模式：登录页「我是新设备」→ 等待屏（码大字 + 倒计时）
    if (pairing) {
      return (
        <PairingWait
          device={pairing.device}
          code={pairingCode}
          codeAt={pairingCodeAt}
          onCancel={() => {
            setPairing(null);
            setPairingCode(null);
            setPairingCodeAt(null);
          }}
        />
      );
    }
    return (
      <Login
        notice={authNotice}
        webauthnAvailable={waSupported}
        passkeyBusy={passkeyBusy}
        passkeyError={passkeyError}
        onPasskeyLogin={handlePasskeyLogin}
        onPairing={(deviceName) => {
          setAuthNotice("");
          setPasskeyError(null);
          setPairing({ device: deviceName || deviceSummary(navigator.userAgent) });
          setPairingCode(null);
          setPairingCodeAt(null);
        }}
        onSave={(t, deviceName) => {
          // USER_TOKEN 先落 localStorage（relay 暂不可达时刷新仍可重试握手）；
          // hello_ack 签发 session 后由 onSession 删除（一次性）。
          localStorage.setItem(USER_TOKEN_KEY, t);
          setAuthNotice("");
          setPasskeyError(null);
          setAuth({
            kind: "user",
            token: t,
            device: deviceName || deviceSummary(navigator.userAgent),
          });
        }}
      />
    );
  }

  /** 出站帧 machine 字段：多机模式恒带当前选中机器（防 ambiguous_machine）；
   *  旧 relay 单机模式不带（帧字节与升级前完全一致）。 */
  const outMachine = (): string | undefined =>
    state.machinesKnown ? state.currentMachine : undefined;

  /** v1.7：可选 media（压缩图片）随 message 帧发送；纯图消息（text 空串）
   *  本地回显用「[图片]」占位（网关侧 text 非空则 Some、空则 None）。
   *  v1.8：/model <名> 命令发出后延时重拉一次 workspace_list——网关处理命令
   *  热设 backend.model() 后回快照，刷新命令面板「当前模型」高亮（/model
   *  本身不产生消息流回显之外的可观测反馈）。仅 `/model ` 前缀且非仅
   *  `/model`（无参形态是查看而非切换）。 */
  const sendMessage = (text: string, media?: MediaPayload): boolean => {
    const ok = wsRef.current?.sendMessage(currentConv, text, outMachine(), media) ?? false;
    if (ok) {
      dispatch({
        type: "send_echo",
        convId: currentConv,
        text: text || "[图片]",
        machine: state.currentMachine,
      });
      if (text.startsWith("/model ") && text.trim() !== "/model") {
        const machine = outMachine();
        window.setTimeout(() => {
          wsRef.current?.listWorkspaces(machine);
        }, MODEL_REFETCH_DELAY_MS);
      }
    }
    return ok;
  };

  // v1.6 当前机器工作空间快照（workspaces 帧；null = 无快照（旧网关），目录条
  // 与目录选择不渲染）。当前 conv 工作目录 = convs[convId] ?? default。
  const wsSnapshot: MachineWorkspaces | null = state.workspaces[state.currentMachine] ?? null;
  const convDir = wsSnapshot ? (wsSnapshot.convs[currentConv] ?? wsSnapshot.default) : null;

  /** 新会话：/switch <名字> 命令文本走 message 帧（convId 留空落网关默认
   *  会话），同时本地把当前 conv 切到 web:<名字>；命令回显留在默认会话。
   *  名字留空合法：落机器生成短 ID（newAutoConvName，s-xxxx）走同一流程
   *  （协议零改动），显示名由 reducer 从该会话首条消息派生（convTitle）。
   *  v1.6：选择了目录（≠default）→ 会话创建后发 setConvWorkdir(新 convId,
   *  path) 绑定并记录 lastdir 预选；未选/选 default 不发（新会话天然落网关
   *  默认目录）。 */
  const createConv = (name: string, dir: string | undefined): boolean => {
    const convName = name.trim() || newAutoConvName();
    const ok = wsRef.current?.sendMessage(undefined, `/switch ${convName}`, outMachine()) ?? false;
    if (!ok) {
      return false;
    }
    dispatch({
      type: "send_echo",
      convId: DEFAULT_CONV,
      text: `/switch ${convName}`,
      machine: state.currentMachine,
    });
    dispatch({ type: "select_conv", convId: toConvId(convName) });
    if (dir !== undefined && dir !== wsSnapshot?.default) {
      wsRef.current?.setConvWorkdir(toConvId(convName), dir, outMachine());
      saveLastDir(state.currentMachine, dir);
    }
    setDrawerOpen(false);
    return true;
  };

  /** v1.6 打开新会话弹层：清名 + 预选 lastdir（该机器上次选择的目录）。 */
  const openNewConv = () => {
    setNewConvName("");
    setNewConvDir(loadLastDir(state.currentMachine) ?? undefined);
    setNewConvOpen(true);
    setDrawerOpen(false);
  };

  const submitNewConv = (e: FormEvent) => {
    e.preventDefault();
    // 名字留空合法（createConv 内落 s-xxxx 自动短 ID）
    if (createConv(newConvName, newConvDir)) {
      setNewConvOpen(false);
    }
  };

  /** v1.6 目录条切换：选择即 setConvWorkdir(currentConv, path)——选 default
   *  同样发（语义统一），并记录 lastdir；快照由网关回包刷新（无乐观更新）。 */
  const switchConvDir = (path: string) => {
    if (wsRef.current?.setConvWorkdir(currentConv, path, outMachine())) {
      saveLastDir(state.currentMachine, path);
    }
    setDirPickerOpen(false);
  };

  /** v1.6 保存工作空间（WorkspacePicker 内联入口；网关回 workspaces 快照，
   *  失败 error 帧 toast）。 */
  const saveWorkspace = (name: string, path: string) => {
    wsRef.current?.saveWorkspace(name, path, outMachine());
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

  /** 侧栏机器切换：会话池 per-machine，reducer 把当前 conv 回到该机器默认
   *  会话（并清其未读）；sync 由机器维度 effect 统一发出。 */
  const selectMachine = (m: string) => {
    if (m !== state.currentMachine) {
      dispatch({ type: "select_machine", machine: m });
    }
  };

  /** 登出：清全部本地凭证（会话 + 遗留 USER_TOKEN + 内存态 adminToken）与
   *  本地消息缓存（IndexedDB，用户显式退出即清本机痕迹；普通刷新不清），
   *  回登录页。服务端吊销走管理面板的「吊销并退出」（revokeDevice）。 */
  const logout = () => {
    clearSession();
    localStorage.removeItem(USER_TOKEN_KEY);
    clearAdminToken();
    void historyStore.clear();
    syncSavedRefs();
    setAdminActive(false);
    setAdminOpen(false);
    setSession(null);
    setAuth(null);
  };

  // ---- v1.5 管理门槛：任何管理操作前保证有效 adminToken（无/过期自动
  //      passkey re-auth，5 分钟内免再次验证；并发共享同一次 ceremony）----

  /** adminToken 过期时刻驱动的「已验证」态翻转（免验提示随真实过期收回）。 */
  const markAdminActive = () => {
    setAdminActive(true);
    window.clearTimeout(adminExpiryTimer.current);
    const exp = adminTokenExpiresAt();
    if (exp !== null) {
      adminExpiryTimer.current = window.setTimeout(
        () => setAdminActive(getAdminToken() !== null),
        Math.max(0, exp - Date.now()),
      );
    }
  };

  /** 取有效 adminToken；必要时自动 re-auth。失败/不支持给 adminError 引导。 */
  const requireAdmin = async (): Promise<string | null> => {
    if (!session) {
      setAdminError("管理操作需会话登录（请重新登录后再试）");
      return null;
    }
    const existing = getAdminToken();
    if (existing !== null) {
      return existing;
    }
    if (!waSupported) {
      setAdminError("本浏览器不支持通行钥匙，无法进行管理验证（请先注册通行钥匙或在支持设备上操作）");
      return null;
    }
    setAdminBusy(true);
    setAdminError(null);
    try {
      const token = await ensureAdminToken(session.token);
      markAdminActive();
      return token;
    } catch (e) {
      setAdminError(reauthErrorText(e));
      return null;
    } finally {
      setAdminBusy(false);
    }
  };

  /** 发送管理帧的小封装：未就绪（断线）时给面板反馈。 */
  const sendAdminFrame = (send: (token: string) => boolean): Promise<void> =>
    requireAdmin().then((token) => {
      if (token && !send(token)) {
        setAdminError("连接未就绪，操作未发送（恢复连接后重试）");
      }
    });

  /** v1.5 管理面板：打开即拉设备/通行钥匙列表；机器列表属管理帧（adminToken）
   *  → 自动 re-auth 后 machine_list。 */
  const openAdmin = () => {
    setAdminOpen(true);
    setAdminError(null);
    wsRef.current?.sendDeviceList();
    wsRef.current?.sendCredList();
    void sendAdminFrame((t) => wsRef.current?.sendMachineList(t) ?? false);
  };

  const refreshMachines = () => {
    void sendAdminFrame((t) => wsRef.current?.sendMachineList(t) ?? false);
  };

  const addMachine = (name: string, pubkey: string) => {
    void sendAdminFrame((t) => wsRef.current?.sendMachineAdd(t, name, pubkey) ?? false);
  };

  const removeMachine = (name: string) => {
    void sendAdminFrame((t) => wsRef.current?.sendMachineRemove(t, name) ?? false);
  };

  const rotateToken = () => {
    void sendAdminFrame((t) => wsRef.current?.sendUserTokenRotate(t) ?? false);
  };

  /** v1.5 配对应答（允许/拒绝均管理级，须 re-auth）；帧发出即移除请求卡
   *  （relay 对 pending 的处理对新设备侧生效，无回执帧）。 */
  const respondPairing = (p: PairingRequest, allow: boolean) => {
    void requireAdmin().then((token) => {
      if (!token) {
        return;
      }
      const sent = allow
        ? wsRef.current?.sendPairingApprove(token, p.code)
        : wsRef.current?.sendPairingDeny(token, p.code);
      if (sent) {
        dispatch({ type: "pairing_resolved", code: p.code });
      } else {
        setAdminError("连接未就绪，操作未发送（恢复连接后重试）");
      }
    });
  };

  /** v1.3 添加通行钥匙：注册 label 用本机设备名（deviceSummary）；完成后
   *  cred_list 刷新（relay 回 creds 帧）。 */
  const addPasskey = async () => {
    if (!session || passkeyBusy) {
      return;
    }
    setPasskeyBusy(true);
    setPasskeyMessage(null);
    try {
      const label = deviceSummary(navigator.userAgent);
      await passkeyRegister(session.token, label);
      setPasskeyMessage(`通行钥匙「${label}」已注册`);
      wsRef.current?.sendCredList();
    } catch (e) {
      const text = e instanceof PasskeyError ? passkeyErrorText(e) : "注册失败";
      setPasskeyMessage(text);
    } finally {
      setPasskeyBusy(false);
    }
  };

  /** v1.3 删除通行钥匙（relay 回更新后的 creds 帧刷新列表）。 */
  const revokePasskey = (p: PasskeyInfo) => {
    wsRef.current?.sendCredRevoke(p.id);
    setPasskeyMessage(null);
  };

  /** 吊销设备：本机（id === session.id）= 服务端吊销 + 登出——先发 revoke
   *  再同步登出，mgr.close() 先于网络回包执行，relay 的 close 1008 不会
   *  再触发 onAuthFailed（不会覆盖「已吊销本机」提示）。其它设备吊销后由
   *  relay 回更新 devices 帧刷新列表。 */
  const revokeDevice = (d: DeviceInfo) => {
    if (session && d.id === session.id) {
      wsRef.current?.sendDeviceRevoke(d.id);
      setAuthNotice("本机设备已吊销，请重新登录");
      logout();
      return;
    }
    wsRef.current?.sendDeviceRevoke(d.id);
  };

  // superseded 态用户显式重连：复用同一 WsManager（connect 会重置终态并新建
  // 连接），新连接会反过来顶替另一窗口——用户显式意图，允许
  const reconnect = () => {
    wsRef.current?.connect();
  };

  const conv = space.convs[currentConv] ?? EMPTY_CONV;

  // v1.4 横幅派生：只展示仍在线机器的告警/阻断（掉线机器的横幅无意义）；
  // 单机模式（machinesKnown=false）恒无 e2e 态。
  const onlineMachines = state.machinesKnown ? state.machines : [];
  const e2eAlertBanners = Object.entries(e2eUi.alerts).filter(([m]) =>
    onlineMachines.includes(m),
  );
  const e2eDowngradeBanners = Object.keys(e2eUi.downgrade).filter((m) =>
    onlineMachines.includes(m),
  );
  const e2eBlockedBanners = Object.entries(e2eUi.blocked).filter(
    ([m]) => onlineMachines.includes(m) && !e2eUi.alerts[m],
  );
  /** 当前机器发送被阻（confirming 待确认 / fail-closed 未放行 / 身份告警）。 */
  const e2eBlocked =
    state.machinesKnown &&
    (e2eUi.alerts[state.currentMachine] !== undefined ||
      e2eUi.blocked[state.currentMachine] === true ||
      e2eUi.tofu.some((t) => t.machine === state.currentMachine));
  /** 当前机器加密会话已激活（StatusBar 锁标）。 */
  const e2eSecure = state.machinesKnown && e2eUi.active[state.currentMachine] === true;

  /** v1.4 重置机器 pin：清信任记录 → 重试握手（重新走 TOFU 确认）。 */
  const resetPin = (machine: string) => {
    removePin(machine);
    setE2ePins(loadPins());
    setE2eUi((prev) => ({
      ...prev,
      alerts: omitKey(prev.alerts, machine),
      blocked: omitKey(prev.blocked, machine),
      active: omitKey(prev.active, machine),
    }));
    wsRef.current?.retryE2e(machine);
  };

  /** 侧栏点选会话：仅切本地当前 conv + 清未读（reducer），移动端同时收抽屉。 */
  const selectConv = (c: string) => {
    dispatch({ type: "select_conv", convId: c });
    setDrawerOpen(false);
  };

  const sidebar = (
    <Sidebar
      convs={convItems}
      current={currentConv}
      machineOnline={state.machineOnline}
      machines={state.machines}
      machinesKnown={state.machinesKnown}
      currentMachine={state.currentMachine}
      sessionExpiresAt={session?.expiresAt}
      onRenewSession={logout}
      onOpenAdmin={session ? openAdmin : undefined}
      onSelect={selectConv}
      onSelectMachine={selectMachine}
      onCreate={openNewConv}
    />
  );

  // h-full 跟随 #root（index.css：var(--vvh, 100dvh)，键盘收展平滑跟随）
  return (
    <div className="flex h-full bg-surface text-ink">
      {/* 桌面（≥768px）：左侧固定侧边栏 240px */}
      <aside className="hidden w-60 shrink-0 border-r border-line bg-panel md:block">{sidebar}</aside>

      {/* 移动（<768px）：侧边栏变抽屉——壳层恒挂载，开关走 150ms 滑入滑出过渡
          （关闭态 -translate-x-full + invisible：摘出可聚焦/a11y 树；visibility
          参与过渡，隐藏在动画末尾离散生效；内容再延迟卸载兜底） */}
      <div
        className={`fixed inset-0 z-50 md:hidden ${drawerOpen ? "" : "pointer-events-none"}`}
        aria-hidden={!drawerOpen}
      >
        <div
          className={`absolute inset-0 bg-black/50 transition-opacity duration-150 ease-out ${
            drawerOpen ? "opacity-100" : "opacity-0"
          }`}
          onClick={() => setDrawerOpen(false)}
          aria-hidden="true"
        />
        <aside
          className={`absolute inset-y-0 left-0 flex w-72 max-w-[85vw] flex-col border-r border-line bg-surface shadow-2xl transition-[transform,visibility] duration-150 ease-out ${
            drawerOpen ? "translate-x-0" : "-translate-x-full invisible"
          }`}
        >
          {drawerContentMounted && sidebar}
        </aside>
      </div>

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
          convLabel={convTitle(conv, currentConv)}
          secure={e2eSecure}
          onLogout={logout}
          onReconnect={reconnect}
          onOpenDrawer={() => setDrawerOpen(true)}
        />
        {/* v1.6 会话目录常驻条：当前 conv 的工作目录（convs[convId] ?? default），
            无快照（旧网关）时整条不渲染；点击弹 WorkspacePicker 切换（选择即
            setConvWorkdir，选 default 同样发——语义统一） */}
        {wsSnapshot && (
          <button
            type="button"
            onClick={() => setDirPickerOpen(true)}
            aria-label="切换会话目录"
            title={convDir ?? "默认目录"}
            className="press flex w-full shrink-0 items-center gap-1.5 border-b border-line bg-panel px-3 py-1.5 text-left text-xs text-muted transition-colors hover:bg-panel2 hover:text-ink"
          >
            <span aria-hidden="true">📂</span>
            <span className="min-w-0 flex-1 truncate font-mono">{convDir ?? "默认目录"}</span>
            <svg
              viewBox="0 0 20 20"
              className="h-3.5 w-3.5 shrink-0"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.8"
              aria-hidden="true"
            >
              <path d="M5 8l5 5 5-5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>
        )}
        {/* v1.4 e2e 横幅区：身份变化（红，持久不自动消退）/ 降级告警（黄，
            收到未加密业务帧——链路可能被降级，帧已丢弃）/ 不支持加密（琥珀
            + 显式明文按钮——fail-closed：绝不自动明文降级） */}
        {e2eAlertBanners.length > 0 && (
          <div
            role="alert"
            className="space-y-1 border-b border-rose-500/40 bg-rose-50 px-3 py-2 text-xs leading-relaxed text-rose-800 dark:border-rose-500/30 dark:bg-rose-950/80 dark:text-rose-200"
          >
            {e2eAlertBanners.map(([m]) => (
              <p key={m}>
                机器 <span className="font-semibold">{m}</span>{" "}
                身份变化：加密指纹与已信任记录不一致，可能被中间人。已断开与该机器的加密连接并发送；请与终端{" "}
                <code className="rounded bg-rose-100/70 px-1 dark:bg-rose-900/50">imagent start</code>{" "}
                日志核对指纹，确认无误后在「管理」面板中重置该机器指纹。
              </p>
            ))}
          </div>
        )}
        {e2eDowngradeBanners.length > 0 && (
          <div
            role="alert"
            className="space-y-1 border-b border-amber-500/50 bg-amber-100 px-3 py-2 text-xs leading-relaxed text-amber-900 dark:border-amber-400/30 dark:bg-amber-900/60 dark:text-amber-100"
          >
            {e2eDowngradeBanners.map((m) => (
              <p key={m}>
                机器 <span className="font-semibold">{m}</span>{" "}
                收到未加密业务帧，链路可能被降级（帧已丢弃）。
              </p>
            ))}
          </div>
        )}
        {e2eBlockedBanners.length > 0 && (
          <div
            role="status"
            className="space-y-1 border-b border-amber-400/40 bg-amber-50 px-3 py-2 text-xs leading-relaxed text-amber-800 dark:border-amber-500/30 dark:bg-amber-950/70 dark:text-amber-200"
          >
            {e2eBlockedBanners.map(([m]) => (
              <div key={m} className="flex flex-wrap items-center gap-x-2 gap-y-1">
                <span>
                  机器 <span className="font-semibold">{m}</span>{" "}
                  不支持加密连接（旧网关）或指纹未确认，发送已停用。
                </span>
                <button
                  type="button"
                  onClick={() => wsRef.current?.allowPlaintextE2e(m)}
                  className="press shrink-0 rounded-lg border border-amber-500/60 bg-amber-500/10 px-2 py-0.5 text-xs text-amber-900 transition-colors hover:bg-amber-500/20 dark:border-amber-400/40 dark:text-amber-100"
                >
                  仍以明文连接
                </button>
              </div>
            ))}
          </div>
        )}
        {/* v1.5 配对请求卡（pairing_request 广播 → 消息区顶部逐张弹卡；允许/
            拒绝均管理级 re-auth，帧发出即撤卡） */}
        {state.pairingRequests.length > 0 && (
          <div className="shrink-0 space-y-2">
            {state.pairingRequests.map((p) => (
              <PairingRequestCard
                key={p.code}
                device={p.device}
                code={p.code}
                expiresAt={p.expiresAt}
                busy={adminBusy}
                onAllow={() => respondPairing(p, true)}
                onDeny={() => respondPairing(p, false)}
              />
            ))}
          </div>
        )}
        <main className="relative min-h-0 flex-1">
          <ApprovalPanel
            pending={allPending}
            showMachine={state.machinesKnown}
            convTitleOf={(convId, machine) =>
              convTitle(spaceOf(state, machine).convs[convId], convId)
            }
            onRespond={respondApproval}
          />
          <MessageList
            entries={conv.entries}
            onSample={status === "ready" ? (t) => sendMessage(t) : undefined}
          />
        </main>
        <Composer
          status={status}
          e2eBlocked={e2eBlocked}
          models={wsSnapshot?.models}
          onSend={sendMessage}
          onStop={() => {
            wsRef.current?.sendControl("stop", undefined, outMachine());
          }}
        />
      </div>

      {/* v1.6 新会话弹层：会话名 + 工作目录选择（快照缺席时不渲染目录区，
          创建流程与原侧栏内联表单一致：/switch 命令 + 本地切 conv） */}
      {newConvOpen && (
        <div
          className="fixed inset-0 z-50 bg-black/50"
          role="dialog"
          aria-modal="true"
          aria-label="新会话"
        >
          <div className="absolute left-1/2 top-1/2 flex max-h-[85vh] w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 flex-col rounded-2xl border border-line bg-panel shadow-2xl">
            <div className="flex shrink-0 items-center px-4 pb-2 pt-3.5">
              <h2 className="text-sm font-semibold text-ink">新会话</h2>
              <button
                type="button"
                onClick={() => setNewConvOpen(false)}
                aria-label="关闭新会话"
                className="press ml-auto rounded-lg p-1.5 text-muted transition-colors hover:bg-panel2 hover:text-ink"
              >
                <svg viewBox="0 0 20 20" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
                  <line x1="5" y1="5" x2="15" y2="15" strokeLinecap="round" />
                  <line x1="15" y1="5" x2="5" y2="15" strokeLinecap="round" />
                </svg>
              </button>
            </div>
            <form onSubmit={submitNewConv} className="min-h-0 flex-1 space-y-3 overflow-y-auto overscroll-contain px-4 pb-4">
              {/* text-base（16px）：防 iOS Safari 聚焦 <16px 输入框时自动放大页面 */}
              <input
                autoFocus
                value={newConvName}
                onChange={(e) => setNewConvName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") {
                    setNewConvOpen(false);
                  }
                }}
                placeholder="会话名（留空自动按首条消息命名）"
                aria-label="新会话名称"
                className="w-full rounded-lg border border-line-strong bg-surface px-2.5 py-1.5 text-base text-ink outline-none placeholder:text-ghost focus:border-emerald-500"
              />
              {wsSnapshot && (
                <div>
                  <div className="mb-1 text-[11px] font-medium uppercase tracking-wider text-ghost">
                    工作目录
                  </div>
                  <WorkspacePicker
                    snapshot={wsSnapshot}
                    selected={newConvDir}
                    onSelect={setNewConvDir}
                    onSave={saveWorkspace}
                  />
                </div>
              )}
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => setNewConvOpen(false)}
                  className="press flex-1 rounded-xl border border-line-strong py-2 text-sm text-muted transition-colors hover:text-ink"
                >
                  取消
                </button>
                <button
                  type="submit"
                  className="press flex-1 rounded-xl bg-emerald-600 py-2 text-sm font-semibold text-white transition-colors hover:bg-emerald-500"
                >
                  创建（/switch）
                </button>
              </div>
            </form>
          </div>
        </div>
      )}

      {/* v1.6 目录切换弹层（目录条点击弹出）：选择即 setConvWorkdir */}
      {dirPickerOpen && wsSnapshot && (
        <div
          className="fixed inset-0 z-50"
          role="dialog"
          aria-modal="true"
          aria-label="切换会话目录"
        >
          <div
            className="absolute inset-0 bg-black/50"
            onClick={() => setDirPickerOpen(false)}
            aria-hidden="true"
          />
          <div className="absolute left-1/2 top-1/2 flex max-h-[85vh] w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 flex-col rounded-2xl border border-line bg-panel shadow-2xl">
            <div className="flex shrink-0 items-center px-4 pb-2 pt-3.5">
              <h2 className="text-sm font-semibold text-ink">切换会话目录</h2>
              <button
                type="button"
                onClick={() => setDirPickerOpen(false)}
                aria-label="关闭目录切换"
                className="press ml-auto rounded-lg p-1.5 text-muted transition-colors hover:bg-panel2 hover:text-ink"
              >
                <svg viewBox="0 0 20 20" className="h-4 w-4" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden="true">
                  <line x1="5" y1="5" x2="15" y2="15" strokeLinecap="round" />
                  <line x1="15" y1="5" x2="5" y2="15" strokeLinecap="round" />
                </svg>
              </button>
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 pb-4">
              <p className="mb-2 text-xs text-muted">
                会话 <span className="font-mono text-ink">{convTitle(conv, currentConv)}</span>{" "}
                的工作目录
              </p>
              <WorkspacePicker
                snapshot={wsSnapshot}
                selected={convDir ?? undefined}
                onSelect={switchConvDir}
                onSave={saveWorkspace}
              />
            </div>
          </div>
        </div>
      )}

      {/* v1.4 E2E：TOFU 首连指纹确认弹层（队列逐台弹） */}
      {e2eUi.tofu.length > 0 && (
        <E2eConfirm
          machine={e2eUi.tofu[0].machine}
          gkFp={e2eUi.tofu[0].gkFp}
          idFp={e2eUi.tofu[0].idFp}
          onResolve={(accept) => {
            wsRef.current?.resolveTofu(e2eUi.tofu[0].machine, accept);
          }}
        />
      )}

      {/* v1.5 管理面板（设备/钥匙/指纹区并入；机器/令牌管理区经 adminToken） */}
      <AdminPanel
        open={adminOpen}
        machines={state.machinesCfg}
        adminBusy={adminBusy}
        adminError={adminError}
        adminActive={adminActive}
        webauthnAvailable={waSupported}
        onRefreshMachines={refreshMachines}
        onAddMachine={addMachine}
        onRemoveMachine={removeMachine}
        onRotateToken={rotateToken}
        devices={state.devices}
        passkeys={state.passkeys}
        passkeyBusy={passkeyBusy}
        passkeyMessage={passkeyMessage}
        selfId={session?.id}
        onRevoke={revokeDevice}
        onAddPasskey={addPasskey}
        onRevokePasskey={revokePasskey}
        pins={e2ePins}
        onResetPin={resetPin}
        onClose={() => setAdminOpen(false)}
      />

      {/* v1.5 USER_TOKEN 轮换结果：一次性明文弹层（关闭即作废） */}
      {state.rotatedToken !== null && (
        <RotatedTokenModal
          token={state.rotatedToken}
          onDone={() => dispatch({ type: "clear_rotated_token" })}
        />
      )}

      {(state.lastError || state.lastNotice || (adminError && !adminOpen)) && (
        <div className="fixed inset-x-0 bottom-24 z-50 mx-auto flex w-fit max-w-[90vw] flex-col items-center gap-2">
          {adminError && !adminOpen && (
            <div
              role="alert"
              className="rounded-full border border-rose-400/60 bg-rose-50 px-4 py-2 text-center text-xs text-rose-800 shadow-xl dark:border-rose-500/40 dark:bg-rose-950/95 dark:text-rose-200"
            >
              {adminError}
            </div>
          )}
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
