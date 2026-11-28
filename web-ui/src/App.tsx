import { useEffect, useMemo, useReducer, useRef, useState, type FormEvent } from "react";
import {
  ACK_TIMEOUT_TICK_MS,
  DEFAULT_CONV,
  DEFAULT_MACHINE,
  applyApprovalsSnapshot,
  convDisplayName,
  convTitle,
  deviceSummary,
  initialState,
  newAutoConvName,
  newClientId,
  peerConvOf,
  reducer,
  spaceOf,
  timedOutAcks,
  type Action,
  type ApprovalDecision,
  type ApprovalSnapshotItem,
  type AppState,
  type AsrUiEvent,
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
  type RestoredConv,
} from "./lib/historyStore";
import { createDraftStore, draftKey } from "./lib/drafts";
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
import {
  PUSH_ENABLED_KEY,
  disablePush,
  enablePush,
  initServiceWorker,
  pushSupported,
  validConvParam,
  validMachineParam,
  type PushErrorCode,
} from "./lib/push";
import { applyTheme, loadThemePref, saveThemePref, type ThemePref } from "./lib/theme";
import { Login } from "./components/Login";
import { StatusBar } from "./components/StatusBar";
import { Sidebar, type ConvItem } from "./components/Sidebar";
import { AdminPanel, RotatedTokenModal } from "./components/AdminPanel";
import { PairingWait } from "./components/PairingWait";
import { PairingRequestCard } from "./components/PairingRequestCard";
import { E2eConfirm } from "./components/E2eConfirm";
import { ConvDeleteConfirm } from "./components/ConvDeleteConfirm";
import { WorkspacePicker, chipCls, loadLastDir, saveLastDir } from "./components/WorkspacePicker";
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
  // v1.40 语音转写（whisper 主路径；文案与 Composer 行内 ASR_ERROR_TEXT 同款
  // ——toast 全局兜底 + 行内贴近操作位置双显示）
  asr_unconfigured: "网关未启用语音转写",
  asr_failed: "语音转写失败，请重试",
  asr_too_large: "录音超过大小上限，请缩短后重试",
  asr_invalid: "录音数据无效，请重试",
  // v1.5 管理面错误（machine_invalid 带 message 时 toast 只显 code，详情在
  // 管理面板场景由 adminError/表单提示承担）
  machine_invalid: "机器名或公钥无效，或名称已存在",
  admin_auth_required: "管理验证缺失或已过期，请重试操作",
  // v1.41 peer 通道（网关伪装 Claude Code peer）：peer: 会话文本投递失败
  peer_unreachable: "窗口不在线或已退出，消息未送达（/peers 查看在线窗口）",
  // v1.41.3 会话删除：删除目标有进行中的轮次被网关拒删（快照照回，本地先行
  // 移除的条目归位）
  conv_busy: "该会话有进行中的任务，请先 /stop",
  // v1.41.3 会话删除：网关侧落库失败（重试即可）
  conv_delete_failed: "会话删除失败，请重试",
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

/** v1.10 通知开启失败引导文案（按错误码分流）。not_supported 覆盖两类：
 *  浏览器能力缺失与 iOS 未添加主屏（pushManager 缺失）。 */
function pushErrorText(code: PushErrorCode): string {
  switch (code) {
    case "not_supported":
      return "此环境不支持通知（iOS 请先将本应用添加到主屏幕）";
    case "permission_denied":
      return "通知权限被拒绝，请在浏览器设置中允许后重试";
    case "network":
      return "网络错误，通知开启失败";
    default:
      return "通知订阅失败，请稍后重试";
  }
}

const EMPTY_CONV: ConvState = { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 };

/** 页面标题基准（index.html title 原文；未读徽标 effect 恢复此值，改标题两处同改）。 */
const BASE_TITLE = "imagent · Web 驾驶舱";

// ---------------------------------------------------------------------------
// v1.36 上次视图记忆（localStorage）：跨登录记住上次打开的机器+会话。恢复在
// useReducer lazy initializer 完成（App 本地函数——protocol.initialState 保持
// 纯函数：protocol.test.ts 在 node 环境跑，无 localStorage）；持久化在渲染后
// effect。恢复值与既有 machines 帧校正天然配合：恢复的机器在线则保持（校正
// 逻辑 no-op），不在线由既有逻辑切 online[0]（不新增校正分支）。
// ---------------------------------------------------------------------------

/** localStorage 键：上次视图（JSON {machine, conv}）。无敏感内容，登出不清除
 *  （用户诉求即跨登录记住）。 */
const LAST_VIEW_KEY = "imagent.lastView";

/** 启动视图推导：initialState() 打底；lastView 合法（JSON 解析成功且
 *  machine/conv 均为非空 string）才覆盖 currentMachine/currentConv。畸形
 *  （坏 JSON/字段缺失/非 string）或存储不可用回落默认——恢复的机器不在线等
 *  校正由既有 machines 帧逻辑承担。 */
function initialView(): AppState {
  const base = initialState();
  try {
    const raw = localStorage.getItem(LAST_VIEW_KEY);
    if (!raw) {
      return base;
    }
    const v = JSON.parse(raw) as { machine?: unknown; conv?: unknown };
    if (
      typeof v.machine === "string" &&
      v.machine.length > 0 &&
      typeof v.conv === "string" &&
      v.conv.length > 0
    ) {
      return { ...base, currentMachine: v.machine, currentConv: v.conv };
    }
  } catch {
    // 坏 JSON / localStorage 不可用（隐私模式等）→ 默认视图
  }
  return base;
}

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

/** App 扩展 action：挂载恢复（loadAll 输出 → spaces 预填，见 restoreHistory）。
 *  v1.8 起入参为 loadAll 输出形态（每 conv {entries, lastSeq?}）。
 *  v1.42 approvals_snapshot：待审批快照（approvals 帧在 onFrame 拦截后转入，
 *  见 applyApprovalsSnapshot）——对齐需要「查询发出时刻」这个 App 层才有的
 *  上下文，故与 restore_history 一样留在 App 级、不进 protocol reducer。 */
type AppAction =
  | Action
  | { type: "restore_history"; restored: Record<string, Record<string, RestoredConv>> }
  | {
      type: "approvals_snapshot";
      machine: string;
      items: ApprovalSnapshotItem[];
      /** 该机器最近一次成功发出 approvals_query 的时刻；undefined = 完全权威
       *  （从未发出过查询就收到了应答——只剩防御意义）。 */
      queriedAt: number | undefined;
    };

function appReducer(state: AppState, action: AppAction): AppState {
  if (action.type === "restore_history") {
    return restoreHistory(state, action.restored);
  }
  if (action.type === "approvals_snapshot") {
    return applyApprovalsSnapshot(state, action.machine, action.items, action.queriedAt);
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
  /** v1.36 新会话引擎选择（快照 backends 段；undefined = 默认，跟随网关默认
   *  引擎不发命令；打开弹层时重置，同 openNewConv 清名/预选目录的模式）。 */
  const [newConvBackend, setNewConvBackend] = useState<string | undefined>(undefined);
  const [dirPickerOpen, setDirPickerOpen] = useState(false);
  // v1.4 E2E：pin 表快照（设备管理指纹区）+ UI 态（TOFU 队列/告警/阻断/激活）
  const [e2ePins, setE2ePins] = useState<E2ePin[]>(() => loadPins());
  const [e2eUi, setE2eUi] = useState<E2eUi>(EMPTY_E2E_UI);
  const [state, dispatch] = useReducer(appReducer, undefined, initialView);
  // v1.41.3 会话删除：侧边栏长按目标（非空 = 确认弹层开启）+ 发帧进行中。
  // hook 与其余 useState 同区（App 有登录页早退分支，hook 不得落在其后）。
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const wsRef = useRef<WsManager | null>(null);
  // state 的渲染后镜像：WsManager 回调（getMachine）需要读最新 machine 维度，
  // 不能闭包进构建时的旧 state。
  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  });
  // v1.40.1 status 的渲染后镜像：visible 兜底补拉 / queryHistory 重试在事件
  // 与定时器时刻读最新连接态（不能闭包进构建时的旧值）——对齐 stateRef 模式。
  const statusRef = useRef(status);
  useEffect(() => {
    statusRef.current = status;
  });
  // v1.36 上次视图持久化：机器/会话变化即写（try/catch 吞写失败——隐私模式
  // 等）。登出不清除（无敏感内容；用户诉求即跨登录记住）。machines 帧校正、
  // 深链 ?conv= 与 select_conv 的落点同经 currentMachine/currentConv，天然被
  // 记录，无需各自埋点。
  useEffect(() => {
    try {
      localStorage.setItem(
        LAST_VIEW_KEY,
        JSON.stringify({ machine: state.currentMachine, conv: state.currentConv }),
      );
    } catch {
      // 写失败（隐私模式配额等）静默：下次启动回落默认视图
    }
  }, [state.currentMachine, state.currentConv]);
  // v1.10 通知订阅取当前 sessionToken 的镜像（enablePush getToken 回调 /
  // togglePush / logout 在异步与事件时刻取最新值）。
  const sessionRef = useRef(session);
  useEffect(() => {
    sessionRef.current = session;
  });
  // v1.7.x 本地缓存：上次已落盘（或已恢复）的 conv entries 引用——脏检查用
  // （reducer 不可变更新，引用比较即“内容是否变过”）；退出清空后同步重置。
  const savedRefs = useRef<Map<string, StreamEntry[]>>(new Map());
  // v1.8 会话历史重放高水位（per machine||conv：网关 outbound_log 该 conv 已
  // 见的最大 seq）：内存缓存——history 帧到达时更新、loadAll 恢复时预填、随
  // debounce 与 entries 一同落盘（historyStore.saveConv 的 lastSeq 字段）。
  const lastSeqRef = useRef<Map<string, number>>(new Map());
  // 上次已落盘写出的水位（脏检查，对齐 savedRefs 语义——空增量 entries 不变
  // 时仅水位驱动重写）。
  const savedSeqRefs = useRef<Map<string, number | undefined>>(new Map());
  // v1.42 输入草稿存储（按「机器 + 会话」隔离，lib/drafts）。useMemo 保持整棵
  // 树同一份实例。hook 必须在下方登录态早退之前（hook 顺序不随登录态变化）。
  const drafts = useMemo(() => createDraftStore(), []);
  // 水位版本号：空增量（entries 引用不变、reducer 无改动）不驱动 spaces 变化，
  // 用它驱动 debounce flush 重排（水位仍须落盘）。
  const [historyVersion, setHistoryVersion] = useState(0);
  // v1.10 Web Push：用户开关态（localStorage 标志镜像）/ 开关流程进行中 /
  // 本地 toast（更新提示 + 通知开关反馈——不进 protocol 的 lastNotice，避免
  // 机器通知类型污染；渲染复用底部 toast 容器，样式/节奏对齐）。
  const [pushEnabled, setPushEnabled] = useState(
    () => localStorage.getItem(PUSH_ENABLED_KEY) === "1",
  );
  const [pushBusy, setPushBusy] = useState(false);
  // 主题三态偏好（跟随系统/浅色/深色）：localStorage 镜像驱动开关高亮；
  // 生效应用在 main.tsx 渲染前由 initTheme 完成过一次，切换时再增量应用。
  const [themePref, setThemePref] = useState<ThemePref>(() => loadThemePref());
  // v1.39：onClick 可选——可点形态（SW 更新提示「点击刷新」，点击立即重载）；
  // 缺席为既有纯展示形态。渲染见底部 toast 容器。
  // v1.41.7：sticky 可选——常驻形态，不参与 4s 自动消退（SW 更新提示用：
  // 真机上提示闪 4 秒就没了极易错过，新版本一直在等待态却没人点刷新）。
  const [localToast, setLocalToast] = useState<{
    text: string;
    tone: "info" | "error";
    onClick?: () => void;
    sticky?: boolean;
  } | null>(null);
  // v1.40 语音转写 UI 事件（asr_result 回包 / error 帧 asr_* code → AsrUiEvent
  // 归一）：asr_result 由 ws 连接层拦截后经 onAsrResult 回调直达（不进消息流，
  // 不落 entries——reducer 只防御性忽略）；asr_* error 在 onFrame 旁路转发
  // （error 帧本体照常进 reducer 走 lastError 全局 toast）。两者转发 Composer
  // （文本回填输入框 / 行内 alert），每次事件新对象引用变化驱动其 effect。
  const [asrEvent, setAsrEvent] = useState<AsrUiEvent | null>(null);
  // v1.44 草稿重读信号：重发时连接未就绪，文字被放回当前会话的输入框（经草稿
  // store）后递增；Composer 平时只在切换会话时读 store，靠它得知 store 被本层
  // 改过而重新载入。
  const [draftReload, setDraftReload] = useState(0);

  /** 挂载恢复：loadAll → spaces 预填（initialState 之后、WS 帧之前；异步竞态
   *  时恢复条目垫在已到帧之前，见 restoreHistory）。加载完成前渲染空态
   *  （现状），加载后一次 setState。v1.8：顺带预填 lastSeqRef（恢复的水位
   *  供 ready 后首查增量携带）并把 savedSeqRefs 对齐已存值（免无谓重写）。 */
  useEffect(() => {
    let cancelled = false;
    void historyStore.loadAll().then((restored) => {
      if (cancelled || Object.keys(restored).length === 0) {
        return;
      }
      for (const [machine, convs] of Object.entries(restored)) {
        for (const [convId, rec] of Object.entries(convs)) {
          const key = historyKey(machine, convId);
          // 恢复内容近似“已保存”——首次 flush 可能因 restoreHistory 重建
          // entries 数组而多写一次同内容（幂等、无害，且顺带刷新 savedAt）。
          savedRefs.current.set(key, rec.entries);
          savedSeqRefs.current.set(key, rec.lastSeq);
          if (typeof rec.lastSeq === "number") {
            lastSeqRef.current.set(key, rec.lastSeq);
          }
        }
      }
      dispatch({ type: "restore_history", restored });
    });
    return () => {
      cancelled = true;
    };
  }, []);

  /** 落盘 debounce：spaces / 水位变化 2s 后写“entries 引用或水位有变化”的
   *  conv（空会话不写）。引用不变（未收到消息且水位未动）零写入。 */
  useEffect(() => {
    const t = setTimeout(() => {
      for (const [machine, space] of Object.entries(stateRef.current.spaces)) {
        for (const [convId, conv] of Object.entries(space.convs)) {
          if (conv.entries.length === 0) {
            continue;
          }
          const key = historyKey(machine, convId);
          const lastSeq = lastSeqRef.current.get(key);
          if (
            savedRefs.current.get(key) === conv.entries &&
            savedSeqRefs.current.get(key) === lastSeq
          ) {
            continue;
          }
          savedRefs.current.set(key, conv.entries);
          savedSeqRefs.current.set(key, lastSeq);
          void historyStore.saveConv(machine, convId, conv.entries, lastSeq);
        }
      }
    }, HISTORY_SAVE_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [state.spaces, historyVersion]);

  /** 退出路径清空缓存后：savedRefs 对齐当前 state 的 entries 引用（否则
   *  debounce flush 会把内存里仍持有的旧会话条目重新写回已清空的存储）；
   *  水位脏检查记录同步对齐（lastSeqRef 保留——网关侧 seq 跨登录持久，
   *  内存会话条目未清时水位仍有效）。 */
  const syncSavedRefs = () => {
    const next = new Map<string, StreamEntry[]>();
    const nextSeq = new Map<string, number | undefined>();
    for (const [machine, space] of Object.entries(stateRef.current.spaces)) {
      for (const [convId, conv] of Object.entries(space.convs)) {
        const key = historyKey(machine, convId);
        next.set(key, conv.entries);
        nextSeq.set(key, lastSeqRef.current.get(key));
      }
    }
    savedRefs.current = next;
    savedSeqRefs.current = nextSeq;
  };

  /** 认证失败分流（close 1008 reason → 清对应凭证 + 登录页提示）。v1.5：
   *  adminToken 属会话附属内存态，任何认证失败一并作废。v1.7.x：凭证失效
   *  回登录页 = 退出路径，一并清空本地消息缓存（本机痕迹）。v1.42：草稿同属
   *  本机痕迹，与消息缓存并排清空；唯独登录会话数达上限不清本机数据（见内）。 */
  const authFailed = (reason: AuthFailReason) => {
    clearAdminToken();
    setAdminActive(false);
    setAdminOpen(false);
    setPairing(null);
    setPairingCode(null);
    setPairingCodeAt(null);
    // 凭证已不可用的认证失败，回登录页等同退出：清空本地消息缓存（IndexedDB）
    // 与输入草稿（内存 + localStorage），并把落盘脏检查记录对齐当前内存态
    // （防 debounce 把已清空的会话回写）。
    // 例外——登录会话数达上限：这条路径不清凭证（凭证并没有失效，只是签发
    // 被拒），用户要去别的设备吊销一个会话再回来重试；这期间草稿是不可恢复
    // 的用户输入、消息缓存是离线期间的消息，清掉就白丢——三步一并跳过，本机
    // 数据原样保留，重试成功后草稿与缓存仍在。缓存没清，脏检查记录也就不必
    // 重置。
    if (reason !== "too_many_sessions") {
      void historyStore.clear();
      drafts.clearAll();
      syncSavedRefs();
    }
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
    } else if (reason === "too_many_sessions") {
      // v1.42 会话表满、签发被拒：凭证本身仍有效——**不清**本地凭证（腾出
      // 名额后刷新页面即以原凭证自动重试），只给可操作的提示
      setAuthNotice("登录会话数已达上限，请先在已登录设备的「管理 → 已登录设备」里吊销不用的会话，再重新登录");
    } else if (reason === "pairing_expired") {
      // v1.42 配对码 5 分钟过期（等待批准期间超时）：重新发起即可
      setAuthNotice("配对码已过期，请重新发起配对");
    } else if (reason === "pairing_busy") {
      // v1.42 同时进行的配对请求达上限：稍后重试
      setAuthNotice("同时进行的配对请求过多，请稍后再试");
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

  // v1.42 待审批快照：每台机器「最近一次成功发出 approvals_query 的时刻」。
  // 快照对齐以它区分「查询之后才到达的实时审批」（快照不具权威性，须保留）
  // 与「查询之前就在、快照里已没有的旧审批」（移除）。只有 App 层持有这个
  // 上下文，所以 approvals 帧在 onFrame 拦截、不进 protocol reducer。
  const approvalsQueriedRef = useRef<Map<string, number>>(new Map());
  /** 对某机器发一次待审批查询；发出成功才记录时刻（被 E2E fail-closed 拒发
   *  等返回 false 时不记——没有查询就不会有权威应答，留着旧时刻反而会让
   *  之后的快照误判哪些实时条目该保留）。 */
  const queryApprovals = (machine: string): void => {
    if (wsRef.current?.sendApprovalsQuery(machine)) {
      approvalsQueriedRef.current.set(machine, Date.now());
    }
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
      // v1.42 待审批快照的第二个触发点：该机器的加密会话已建立，再查一次。
      // ready 时发的那次多半落在握手窗口里，应答可能按已作废的密钥加密而被
      // 丢弃；握手完成后的这次查询走密文、往返确定，是最终对齐。TOFU 确认后
      // 的重新握手同样会走到 active，多查一次无害（幂等）。
      queryApprovals(ev.machine);
      // v1.43 本地窗口看板的第二个触发点，理由相同：ready 时那次多半落在握手
      // 窗口里，而 active 之后网关下发的快照恒为密文——握手完成后以密文再查
      // 一次，才是看板数据的确定来源。新上线的机器也只经这里补查（machines
      // 帧重排不触发第一个触发点）。
      wsRef.current?.sendWindowsQuery(ev.machine);
      // v1.44 送达回执 / 网关版本，同一时刻的第三个动作：先作废上一代加密会话里
      // 问到的网关能力，再重查。加密会话每次换代（WS 重连 / 网关重启后重新握手）
      // 都必经这里，而网关可能已换成旧二进制——旧网关不回 gateway_info，留着
      // 旧记录就判不出来：该机器的每条新消息都会「发送中」→ 超时 →「未确认送达」
      // 并诱导重发。作废的那一瞬到新应答之间发出的消息不带送达状态，随后到达
      // 的回执照常能把它们标成 sent / failed。TOFU 确认后的重新握手同样走到
      // active，多查一次无害（幂等）。
      dispatch({ type: "gateway_info_forget", machine: ev.machine });
      wsRef.current?.sendGatewayInfoQuery(ev.machine);
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
      onFrame: (frame) => {
        // v1.42 待审批快照：转成 approvals_snapshot（带上该机器最近一次查询
        // 的发出时刻），不进帧归约——reducer 侧收到原始帧只会防御性忽略。
        if (frame.type === "approvals") {
          const machine = frame.machine;
          dispatch({
            type: "approvals_snapshot",
            machine,
            items: frame.items,
            queriedAt: approvalsQueriedRef.current.get(machine),
          });
          return;
        }
        // v1.8 history 帧：先记水位再进 reducer——空增量（entries=[]）reducer
        // 无改动，水位仍须更新并随 debounce 落盘（historyVersion 驱动 flush）。
        if (frame.type === "history") {
          lastSeqRef.current.set(
            historyKey(frame.machine ?? DEFAULT_MACHINE, frame.convId),
            frame.lastSeq,
          );
          setHistoryVersion((v) => v + 1);
        }
        // v1.40 asr_* error 旁路通知 Composer（行内 alert；error 帧本体照常
        // 进 reducer——lastError 全局 toast 管道不变，双显示）
        if (frame.type === "error" && frame.code.startsWith("asr_")) {
          setAsrEvent({ kind: "error", code: frame.code });
        }
        dispatch({ type: "frame", frame });
      },
      // v1.40 语音转写回包（ws 连接层已拦截 asr_result，不进 onFrame/消息流）
      // → Composer 回填输入框；旧网关无此帧（Composer 60s 无回包超时兜底）。
      onAsrResult: (frame) => {
        setAsrEvent({ kind: "result", text: frame.text, durationMs: frame.durationMs });
      },
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
    // v1.42：每建立一个新的连接实例，App 的连接态先显式回到「连接中」。
    // status 平时只由 WsManager 的 onStatus 驱动，而登出 / 认证失败不会把它
    // 从 ready 拉回来；新 WsManager 的内部初值就是 connecting，connect() 里
    // 那次同值 setStatus 不会通知——于是重新登录后 App 的 status 一直停在
    // ready，新连接 hello_ack 带来的 ready 被当作「没有变化」：所有依赖
    // status 的 effect（sync + workspace_list、history_query、待审批查询）在
    // 新连接上一次都不触发，离线期间的消息与审批补不回来。这里显式拉回，
    // 让新连接的 ready 必然表现为一次状态变化。首次挂载时值本就是
    // connecting，React 对同值更新不重渲染。
    setStatus("connecting");
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

  // v1.10 PWA 接线（挂载一次）：SW 注册（仅 https/localhost，含「新版本已就绪，
  // 点击刷新」更新提示与通知点击的 open-conv 消息）+ ?conv= 深链会话定位
  // （SW 无窗口时 openWindow 到 /imagent/?conv=...，挂载读参定位；完整 conv
  // 形态校验后才切）。
  // v1.37：深链可携 ?machine=（推送载荷的来源机器）——先 select_machine 再
  // select_conv（reducer 的 select_conv 落在当前机器会话空间，先切机器才有效；
  // 连续 dispatch 按序过 reducer）；machine 非法忽略仍按 conv 走；深链参数
  // 优先于 lastView 恢复（显式用户意图）。SW open-conv 消息同序透传 machine。
  useEffect(() => {
    const cleanupSw = initServiceWorker({
      // v1.39：更新提示改可点形态——PWA standalone 无下拉刷新手段（iOS），
      // 点击立即重载。新 SW install 已把新 shell 覆写进 SHELL_CACHE（sw.js
      // install handler cache.addAll），普通 reload 经旧 SW cache-first 命中的
      // 即是新 HTML → 一步到位拿新版本；controllerchange 自动重载为双保险。
      // v1.41.7：提示常驻（sticky）——此前随 4s 自动消退，真机上闪一下就没。
      // 覆写之所以成立，前提是 sw.js 的 SHELL_CACHE 名不随构建变（新旧 SW
      // 写同一个 cache，见 sw.js 该常量的迁移注释）。
      // v1.41.9：只有本页面确属旧构建时才会走到这里——发版后首次冷启动时
      // 导航经旧 SW network-first 取到的已是新首页（页面即新版），
      // initServiceWorker 判定「页面已是新 SW 所属构建」后不提示，也不驱动
      // 新 SW 接管（它到下次冷启动自然激活；判定见 push.ts 的
      // pageIsCurrentShell）。
      // 文案不带版本号：页面只知道自己正在跑的 __APP_VERSION__（即**旧**版本），
      // 新版本号在刷新落地前无从得知——写上去等于报错版本。当前实际在跑的
      // 版本看侧栏底部版本行。
      onUpdateAvailable: () =>
        setLocalToast({
          text: "新版本已就绪，点击刷新",
          tone: "info",
          onClick: () => window.location.reload(),
          sticky: true,
        }),
      onOpenConv: (convId, machine) => {
        if (machine) {
          dispatch({ type: "select_machine", machine });
        }
        dispatch({ type: "select_conv", convId });
      },
    });
    const params = new URLSearchParams(window.location.search);
    const machine = validMachineParam(params.get("machine"));
    const conv = validConvParam(params.get("conv"));
    if (conv) {
      if (machine) {
        dispatch({ type: "select_machine", machine });
      }
      dispatch({ type: "select_conv", convId: conv });
    }
    return cleanupSw;
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 仅挂载执行一次（dispatch 为稳定引用）
  }, []);

  // v1.10 本地 toast 自动消退（4s，对齐 error/notice 节奏）；v1.41.7：sticky
  // 形态不起定时器（常驻到被点击或被后续 toast 顶替）
  useEffect(() => {
    if (!localToast || localToast.sticky) {
      return;
    }
    const t = setTimeout(() => setLocalToast(null), 4000);
    return () => clearTimeout(t);
  }, [localToast]);

  // v1.10 登录 ready 后静默重挂：用户开过通知（localStorage 标志）且拿到
  // sessionToken 时自动重新登记——sessionToken 换新后服务端订阅需重挂（重复
  // endpoint 幂等覆盖，token 变化即重试；每 token 只试一次）。失败静默降级：
  // 关闭开关并 toast 一次（下次开启走手动流程）。
  const pushRetriedTokenRef = useRef<string | null>(null);
  useEffect(() => {
    const token = session?.token;
    if (!auth || !token) {
      return;
    }
    if (localStorage.getItem(PUSH_ENABLED_KEY) !== "1") {
      return;
    }
    if (pushRetriedTokenRef.current === token) {
      return;
    }
    pushRetriedTokenRef.current = token;
    void enablePush(() => sessionRef.current?.token).then((r) => {
      if (!r.ok) {
        localStorage.removeItem(PUSH_ENABLED_KEY);
        setPushEnabled(false);
        setLocalToast({ text: "通知订阅已失效，已自动关闭（可在侧栏重新开启）", tone: "error" });
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps -- session token 维度触发
  }, [auth, session?.token]);

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

  // v1.42 待审批快照（第一个触发点）：连接 ready 且多机模式时，对**全部在线
  // 机器**各查一次待审批——审批面板是跨机器的，离线期间到达的审批可能在任何
  // 一台机器上，只查当前机器会让别处的审批按钮恢复不出来。重连后同样走到
  // 这里：离线期间到达的审批由此补回，离开期间已超时 / 已解决的审批由快照
  // （不再包含它们）自动移除。单机模式（旧 relay）不发，与 sync + workspace_list
  // 同一纪律：旧网关不认识该帧。
  //
  // 依赖刻意只有 machinesKnown 与 status——不跟 currentMachine / machines：
  // 待审批集合不随「当前看哪台机器」变化，切机器时对全部在线机器重发纯属
  // 浪费；machines 帧每变一次（任一网关上下线）都重查，则重连后的出站突发
  // （内置 sync + sync + workspace_list + history_query + 每台 e2e_hello +
  // 每台本查询 × 2 个触发点）还要再叠一层，白占中继给**浏览器连接**的限流
  // 额度（滑动窗口按角色分档，浏览器档位远低于网关档位；在线机器越多这笔
  // 突发越大，再叠上用户随后的切换操作就更容易被限流断连）。窗口与帧数的
  // 具体档位以中继侧为准，此处不写死数字。新上线机器的审批由它握手完成时
  // 的第二个触发点（onE2eEvent 的 active）覆盖。
  useEffect(() => {
    if (state.machinesKnown && status === "ready") {
      for (const machine of state.machines) {
        queryApprovals(machine);
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- queryApprovals 仅读 wsRef / ref；依赖刻意不含 machines 与 currentMachine（理由见上）
  }, [state.machinesKnown, status]);

  // v1.43 本地窗口看板（第一个触发点）：与上方待审批查询同一纪律——ready 且多机
  // 模式时，对**全部在线机器**各请求一次窗口快照。侧栏只显示当前机器的看板，
  // 但切机不重发（见下），用户切过去时不该对着一块空白看板。依赖同样刻意只有
  // machinesKnown 与 status：切机 / machines 帧重排不重发——重连后的出站突发不再
  // 叠一层，不白占浏览器连接的限流额度。单机模式（旧 relay）不发：旧网关不认识
  // 该帧。没有快照回来时 state.windows 里就没有该机器的键，侧栏不显示看板
  // （旧网关 / 未启用 peer 通道）。
  useEffect(() => {
    if (state.machinesKnown && status === "ready") {
      for (const machine of state.machines) {
        wsRef.current?.sendWindowsQuery(machine);
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 仅读 wsRef；依赖刻意不含 machines 与 currentMachine（理由见上）
  }, [state.machinesKnown, status]);

  // v1.44 送达回执 / 网关版本（第一个触发点）：与上方两个查询同一纪律——ready 且
  // 多机模式时，对**全部在线机器**各查一次网关版本与能力。旧网关不回，没有回帧
  // 就没有 state.gatewayInfo[machine]，等于「不支持回执」：该机器的本机回显不
  // 带送达状态，管理页也不显示版本。依赖同样刻意只有 machinesKnown 与 status
  // （理由见上，不再叠一层重连后的出站突发）；新上线机器由它握手完成时的第二
  // 个触发点（onE2eEvent 的 active）覆盖。
  useEffect(() => {
    if (state.machinesKnown && status === "ready") {
      for (const machine of state.machines) {
        wsRef.current?.sendGatewayInfoQuery(machine);
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 仅读 wsRef；依赖刻意不含 machines 与 currentMachine（理由见上）
  }, [state.machinesKnown, status]);

  // v1.44 送达回执超时：发出后一直等不到回执的回显，每 2 秒巡检一次，按
  // timedOutAcks（纯函数，protocol.ts）的结论派发 ack_timeout——满 15 秒，或 WS
  // 不在 ready 且满 2 秒（连接断了回执就到不了）。只对支持回执的机器生效
  // （判定在纯函数里）。没有任何待确认的回显时不起定时器（空闲零唤醒）；定时器
  // 存活期间读 ref 取最新状态与连接态，不随渲染重排，也不额外触发渲染。
  const hasPendingAck = useMemo(
    () =>
      Object.values(state.spaces).some((sp) =>
        Object.values(sp.convs).some((c) => c.entries.some((e) => e.kind === "user" && e.ack === "pending")),
      ),
    [state.spaces],
  );
  useEffect(() => {
    if (!hasPendingAck) {
      return;
    }
    const timer = setInterval(() => {
      const s = stateRef.current;
      for (const t of timedOutAcks(s, Date.now(), statusRef.current === "ready")) {
        dispatch({ type: "ack_timeout", ...t });
      }
    }, ACK_TIMEOUT_TICK_MS);
    return () => clearInterval(timer);
  }, [hasPendingAck]);

  // v1.8 会话历史重放：ready / 切机 / 切会话后对当前 conv 按本地高水位增量
  // 拉取（history_query 业务帧，E2E 加密）。触发维度 = 上方 sync+workspace_list
  // 的 ready/机器条件 + currentConv——每次切换都查（空增量便宜，网关按 seq
  // 单调幂等；切机时 reducer 同帧把 currentConv 回默认会话，一次触发覆盖两维）。
  // 旧 relay 单机模式（machinesKnown=false）不发（网关不认识该帧）。
  // v1.40.1：重连 ready 若落在 E2E 握手窗口（confirming → active 数百 ms），
  // queryHistory 会被 fail-closed 拒发（静默 false）且 effect 依赖不再变化、
  // 永不重试 → 重连成功但文本消息永不补齐。改为指数退避重试至多 3 次（覆盖
  // e2e_ack 回来的握手窗口；成功或不再 ready 即停）。sendSync 不重试——ws 层
  // onHelloAck 的内置 sync 已是明文直发兜底，App 层这份丢一次无害（machines
  // 帧后 App 本会再发一次带 machine 的 sync）。
  useEffect(() => {
    if (state.machinesKnown && status === "ready") {
      const key = historyKey(state.currentMachine, state.currentConv);
      let timer: ReturnType<typeof setTimeout> | null = null;
      let attempt = 0;
      const send = (): void => {
        const ok =
          wsRef.current?.queryHistory(
            state.currentConv,
            lastSeqRef.current.get(key) ?? 0,
            state.currentMachine,
          ) ?? false;
        // E2E fail-closed 拒发（握手窗口）→ 静默 false：退避重试至多 3 次
        if (!ok && attempt < 3 && statusRef.current === "ready") {
          attempt += 1;
          timer = setTimeout(send, 800 * attempt);
        }
      };
      send();
      return () => {
        if (timer !== null) {
          clearTimeout(timer);
        }
      };
    }
  }, [state.machinesKnown, state.currentMachine, state.currentConv, status]);

  // v1.40.1 切回前台兜底补拉：ws 层 visible 探活只保连接活性，错过消息的
  // 数据面恢复在此兜底——visible 且 ready 时对当前会话补拉一次卡片快照
  // （sync）+ 文本增量（queryHistory 按本地高水位，空增量便宜幂等），与
  // ws 层探活独立、双保险。仅补当前会话（切会话/切机由上方既有 effect
  // 各自触发）；状态读 ref 最新值（事件时刻不闭包旧 state/status）。
  useEffect(() => {
    if (typeof document === "undefined") {
      return; // SSR/测试环境无 document 安全跳过
    }
    const onVisible = (): void => {
      if (document.visibilityState !== "visible") {
        return;
      }
      const s = stateRef.current;
      if (!s.machinesKnown || statusRef.current !== "ready") {
        return;
      }
      wsRef.current?.sendSync(s.currentMachine);
      wsRef.current?.queryHistory(
        s.currentConv,
        lastSeqRef.current.get(historyKey(s.currentMachine, s.currentConv)) ?? 0,
        s.currentMachine,
      );
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 仅挂载注册一次（读 stateRef/statusRef/lastSeqRef 最新值）
  }, []);

  // v1.42 草稿落盘兜底：草稿平时防抖 500ms 才写 localStorage，页面在这 500ms
  // 内被收走（iOS 把后台 PWA 挂起 / 杀进程、桌面直接关标签页）最后几句就没
  // 了。pagehide（卸载路径上最后的可靠时机）与 visibilitychange 转入不可见时
  // 各同步落盘一次；flush 幂等，没有待写内容时不会产生存储写入。
  useEffect(() => {
    const flushDrafts = (): void => drafts.flush();
    const onVisibilityChange = (): void => {
      // 非 visible 即落盘（含 prerender 等过渡态）：多写一次无害，少写一次丢字
      if (document.visibilityState !== "visible") {
        drafts.flush();
      }
    };
    window.addEventListener("pagehide", flushDrafts);
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      window.removeEventListener("pagehide", flushDrafts);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [drafts]);

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
      // v1.37 运行态：该 conv entries 存在流式中的卡片（pending 审批不算；
      // 只扫本 conv，MAX_STREAM_ENTRIES 200 条护栏内 O(n) 可接受）
      running:
        space.convs[c]?.entries.some((e) => e.kind === "card" && e.state === "streaming") ??
        false,
    }));
    if (!items.some((i) => i.convId === currentConv)) {
      items.unshift({
        convId: currentConv,
        title: convTitle(space.convs[currentConv], currentConv),
        preview: "",
        at: 0,
        unread: 0,
        running: false,
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

  // ---------------------------------------------------------------------------
  // v1.37 系统未读徽标 + 标签页标题未读数：未读总数 = 全部机器全部 conv 的
  // unread 求和（state.spaces 遍历派生）。总数变化时 navigator.setAppBadge(n)
  // / clearAppBadge()（iOS 16.4+ 等环境才有，全程 feature-detect + try/catch，
  // 缺席/失败静默跳过），并同步 document.title（n>0 前缀计数，n=0 恢复基准）。
  // ---------------------------------------------------------------------------
  const totalUnread = useMemo(() => {
    let n = 0;
    for (const sp of Object.values(state.spaces)) {
      for (const c of Object.values(sp.convs)) {
        n += c.unread;
      }
    }
    return n;
  }, [state.spaces]);

  useEffect(() => {
    try {
      if (totalUnread > 0) {
        if (typeof navigator.setAppBadge === "function") {
          void navigator.setAppBadge(totalUnread);
        }
      } else if (typeof navigator.clearAppBadge === "function") {
        void navigator.clearAppBadge();
      }
    } catch {
      // 徽标调用失败（权限/环境异常）：静默降级，不影响页内未读显示
    }
    document.title = totalUnread > 0 ? `(${totalUnread}) ${BASE_TITLE}` : BASE_TITLE;
  }, [totalUnread]);

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
    // v1.44：每次发送生成新的 clientId——message 帧带上它，网关据此回送达回执；
    // 本地回显的键与关联字段取同一个值（一帧对应一条回显）。
    const clientId = newClientId();
    const ok = wsRef.current?.sendMessage(currentConv, text, outMachine(), media, clientId) ?? false;
    if (ok) {
      dispatch({
        type: "send_echo",
        convId: currentConv,
        text: text || "[图片]",
        machine: state.currentMachine,
        clientId,
        // 带图的消息：回显里没有图片本体，标记出来让界面不给「重发」
        ...(media ? { image: true } : {}),
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

  /** v1.44 重发：把一条没送达 / 未确认送达的消息的文字当**新消息**再发一次
   *  （走同一条 sendMessage 路径：新的 clientId、新的回显；原条目保持原样——
   *  它的结局确实是那样，重发是另发一条）。没发出去（连接未就绪 / 握手
   *  fail-closed 拒发）时文字放回**当前**会话的输入框并提示：「重发」按钮只在
   *  当前会话的消息列表里，上抛的就是当前会话的条目。以 sendMessage 的返回值
   *  而不是连接态判断——status 为 ready 但握手待确认时帧同样发不出去，只看
   *  连接态会让点击没有任何反应。追加而不是覆盖：用户可能正在输入别的；
   *  连接恢复后发不发由用户决定。 */
  const resend = (text: string): void => {
    if (sendMessage(text)) {
      return;
    }
    drafts.appendText(draftKey(state.currentMachine, currentConv), (prev) =>
      prev ? `${prev}\n${text}` : text,
    );
    setDraftReload((n) => n + 1);
    setLocalToast({ text: "连接未就绪，已放回输入框", tone: "error" });
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
   *  默认目录）。v1.36：选择了引擎（backend 非 undefined）→ 发
   *  `/backend <引擎>` message 帧到新 convId（select_conv 已切，直发不走
   *  sendMessage——闭包 currentConv 还是旧会话）+ 本地回显同落新会话，命令
   *  文本与网关「✅ 已切换」回显一起落在新会话首屏（因果透明，与手打等价）；
   *  undefined = 跟随网关默认引擎，不发命令。 */
  const createConv = (name: string, dir: string | undefined, backend: string | undefined): boolean => {
    const convName = name.trim() || newAutoConvName();
    // v1.44：/switch 与 /backend 也是 message 帧——同样带 clientId、回显按它关联
    //（网关的送达回执 / 拒绝原因照常落到这两条回显上）。
    const switchId = newClientId();
    const ok =
      wsRef.current?.sendMessage(undefined, `/switch ${convName}`, outMachine(), undefined, switchId) ?? false;
    if (!ok) {
      return false;
    }
    dispatch({
      type: "send_echo",
      convId: DEFAULT_CONV,
      text: `/switch ${convName}`,
      machine: state.currentMachine,
      clientId: switchId,
    });
    dispatch({ type: "select_conv", convId: toConvId(convName) });
    if (backend !== undefined) {
      const newConvId = toConvId(convName);
      const backendId = newClientId();
      if (wsRef.current?.sendMessage(newConvId, `/backend ${backend}`, outMachine(), undefined, backendId)) {
        dispatch({
          type: "send_echo",
          convId: newConvId,
          text: `/backend ${backend}`,
          machine: state.currentMachine,
          clientId: backendId,
        });
      }
    }
    if (dir !== undefined && dir !== wsSnapshot?.default) {
      wsRef.current?.setConvWorkdir(toConvId(convName), dir, outMachine());
      saveLastDir(state.currentMachine, dir);
    }
    setDrawerOpen(false);
    return true;
  };

  /** v1.6 打开新会话弹层：清名 + 预选 lastdir（该机器上次选择的目录）+ 重置
   *  引擎选择（v1.36，回默认形态）。 */
  const openNewConv = () => {
    setNewConvName("");
    setNewConvDir(loadLastDir(state.currentMachine) ?? undefined);
    setNewConvBackend(undefined);
    setNewConvOpen(true);
    setDrawerOpen(false);
  };

  const submitNewConv = (e: FormEvent) => {
    e.preventDefault();
    // 名字留空合法（createConv 内落 s-xxxx 自动短 ID）
    if (createConv(newConvName, newConvDir, newConvBackend)) {
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
   *  回登录页。服务端吊销走管理面板的「吊销并退出」（revokeDevice）。
   *  v1.10：一并通知退订（best-effort，不阻塞登出）+ 清开关标志。
   *  v1.37：清系统未读徽标与标签页标题（badge API 缺席静默跳过）。
   *  v1.42：输入草稿同属本机痕迹，一并清空（内存 + localStorage）。 */
  const logout = () => {
    if (session?.token && localStorage.getItem(PUSH_ENABLED_KEY) === "1") {
      void disablePush(session.token).catch(() => {});
    }
    localStorage.removeItem(PUSH_ENABLED_KEY);
    setPushEnabled(false);
    clearSession();
    localStorage.removeItem(USER_TOKEN_KEY);
    clearAdminToken();
    void historyStore.clear();
    drafts.clearAll();
    syncSavedRefs();
    try {
      if (typeof navigator.clearAppBadge === "function") {
        void navigator.clearAppBadge();
      }
    } catch {
      // 静默（同 badge effect 降级语义）
    }
    document.title = BASE_TITLE;
    setAdminActive(false);
    setAdminOpen(false);
    setSession(null);
    setAuth(null);
  };

  /** v1.10 通知开关（侧栏底部行，仅会话模式）：未开启 → enablePush 全流程，
   *  成功置 localStorage 标志（下次登录静默重挂依据）；已开启 → disablePush
   *  并清标志。结果经 localToast 反馈（失败按错误码给引导文案）。 */
  const togglePush = async () => {
    if (pushBusy || !sessionRef.current) {
      return;
    }
    setPushBusy(true);
    try {
      if (!pushEnabled) {
        const r = await enablePush(() => sessionRef.current?.token);
        if (r.ok) {
          localStorage.setItem(PUSH_ENABLED_KEY, "1");
          setPushEnabled(true);
          setLocalToast({ text: "通知已开启", tone: "info" });
        } else {
          setLocalToast({ text: pushErrorText(r.code), tone: "error" });
        }
      } else {
        await disablePush(sessionRef.current.token);
        localStorage.removeItem(PUSH_ENABLED_KEY);
        setPushEnabled(false);
      }
    } finally {
      setPushBusy(false);
    }
  };

  /** 主题三态切换（侧栏「外观」段选）：落盘偏好 + 应用到 document
   *  （html[data-theme] 属性 + theme-color meta，lib/theme.ts）。 */
  const changeTheme = (pref: ThemePref) => {
    setThemePref(pref);
    saveThemePref(pref);
    applyTheme(pref);
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

  /** v1.43 当前机器卡在 waiting 的本地窗口数（状态条「窗口待确认」提示只看当前
   *  机器——提示点开的是当前机器的看板）。没有快照 = 0。 */
  const windowsWaiting =
    state.windows[state.currentMachine]?.items.filter((w) => w.status === "waiting").length ?? 0;
  /** v1.43 窗口会话的空态说明：当前会话是 peer:<窗口名> 且还没有任何消息时，给
   *  一句怎么用的说明（通用示例 prompt 是给 agent 会话的，发给一个本地窗口没有
   *  意义）；会话一旦有内容（窗口回信 / 历史重放 / 用户发出的第一条）即不再显示。 */
  const peerEmptyHint =
    currentConv.startsWith("peer:") && conv.entries.length === 0
      ? `这是与本地 Claude Code 窗口「${convDisplayName(currentConv)}」的对话。直接输入文字即可派活，窗口空闲时会自动开始处理；窗口的回信会出现在这里。`
      : undefined;

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

  /** v1.43 关注 / 取消关注一个本地窗口。新值取自用户看到并点下的那个快照标志
   *  ——关注是跨设备的全局状态（归网关所有、由网关持久化，重启保留），快照是它
   *  在界面上的唯一来源，本机不保存任何名单。帧被接受（sendWindowWatch 返回
   *  true）才做乐观翻转；被拒发（未 ready / 握手 fail-closed）时不改状态——
   *  否则断线时点一下，界面会一直说「已关注」，直到下一帧快照才被拨回来。
   *  （不要在前端另存关注名单并对账补发：各设备各存一份时，「A 设备取消关注」
   *  会被 B 设备的对账逻辑立刻补发回来，等于取消不了。） */
  const toggleWindowWatch = (machine: string, name: string) => {
    const current =
      stateRef.current.windows[machine]?.items?.find((w) => w.name === name)?.watched ?? false;
    if (wsRef.current?.sendWindowWatch(name, !current, machine)) {
      dispatch({ type: "toggle_window_watch", machine, name });
    }
  };

  /** v1.43 点窗口行：进入该窗口的会话（peer:<窗口名>——v1.41 起本地 select_conv
   *  即可打开尚不存在的会话，由侧栏占位项兜底）。先切机器再切会话（同深链 /
   *  审批「前往会话」的顺序依赖：select_conv 落在当前机器的会话空间；同机器时
   *  reducer 无事可做）；移动端收抽屉。 */
  const openWindow = (machine: string, name: string) => {
    dispatch({ type: "select_machine", machine });
    dispatch({ type: "select_conv", convId: peerConvOf(name) });
    setDrawerOpen(false);
  };

  /** 删除确认：发 conv_delete 帧（成功才本地先行移除——未连接时帧发不出去，
   *  本地删除会被下一次 sync 的卡片重发顶回，弹层留驻可重试）+ reducer 移除 +
   *  本地缓存清理（不清则刷新后 restoreHistory 复活已删会话）。网关权威状态
   *  由回包的 workspaces 快照收敛（busy 拒绝时快照把条目带回归位 + toast）。 */
  const confirmDeleteConv = () => {
    const target = deleteTarget;
    if (!target || deleteBusy) {
      return;
    }
    if (!(wsRef.current?.sendConvDelete(target, outMachine()) ?? false)) {
      return;
    }
    setDeleteBusy(true);
    const machine = state.currentMachine;
    dispatch({ type: "delete_conv", convId: target, machine });
    const key = historyKey(machine, target);
    void historyStore.removeConv(machine, target);
    savedRefs.current.delete(key);
    lastSeqRef.current.delete(key);
    savedSeqRefs.current.delete(key);
    // v1.42：该会话的输入草稿一并清（否则同名会话重建时旧草稿会冒出来）
    drafts.clear(draftKey(machine, target));
    setDeleteBusy(false);
    setDeleteTarget(null);
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
      onTogglePush={session ? togglePush : undefined}
      pushEnabled={pushEnabled}
      pushAvailable={pushSupported()}
      pushBusy={pushBusy}
      themePref={themePref}
      onThemeChange={changeTheme}
      onSelect={selectConv}
      onSelectMachine={selectMachine}
      onCreate={openNewConv}
      onConvDelete={setDeleteTarget}
      // v1.43：看板显示当前机器的快照；没有该机器的键（从未收到过）时 Sidebar
      // 整块不渲染。两个回调按窗口名上抛，机器维度由这里绑定（快照即当前机器的）。
      windows={state.windows[state.currentMachine]}
      onSelectWindow={(name) => openWindow(state.currentMachine, name)}
      onToggleWindowWatch={(name) => toggleWindowWatch(state.currentMachine, name)}
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
          windowsWaiting={windowsWaiting}
          onOpenWindows={() => setDrawerOpen(true)}
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
            onJump={(p) => {
              // v1.37 「前往会话」：先切机器再切会话（同深链的顺序依赖——
              // select_conv 落在当前机器会话空间才有效；同机器 no-op）。
              // 审批弹层不关闭：仅视口切换，回来审批条仍在。
              dispatch({ type: "select_machine", machine: p.machine });
              dispatch({ type: "select_conv", convId: p.convId });
            }}
          />
          <MessageList
            entries={conv.entries}
            onSample={status === "ready" ? (t) => sendMessage(t) : undefined}
            emptyHint={peerEmptyHint}
            onResend={resend}
          />
        </main>
        <Composer
          status={status}
          e2eBlocked={e2eBlocked}
          models={wsSnapshot?.models}
          asr={wsSnapshot?.asr === true}
          asrEvent={asrEvent}
          drafts={drafts}
          draftKey={draftKey(state.currentMachine, currentConv)}
          draftReload={draftReload}
          onAsr={(audioB64, durationMs) =>
            wsRef.current?.sendAsrRequest(audioB64, durationMs, outMachine()) ?? false
          }
          onSend={sendMessage}
          onStop={() => {
            // v1.42：stop 必须带当前会话——不带时网关落默认会话，在
            // web:s-xxxx 里点停止停不掉当前任务（旧网关忽略该字段）。
            wsRef.current?.sendControl("stop", undefined, outMachine(), currentConv);
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
              {/* v1.36 引擎行：快照带 backends 段才渲染（缺席/畸形整段按缺席
                  ——优雅降级，同目录区缺席不渲染的既有模式）。「默认」chip =
                  跟随网关默认引擎（不发 /backend 命令），options 各引擎 chip
                  选中即创建时显式 /backend 切换。 */}
              {wsSnapshot?.backends && (
                <section aria-label="引擎">
                  <div className="mb-1 text-[11px] font-medium uppercase tracking-wider text-ghost">
                    引擎
                  </div>
                  <div className="flex gap-2 overflow-x-auto pb-1">
                    <button
                      type="button"
                      title={`网关默认引擎：${wsSnapshot.backends.current}`}
                      aria-pressed={newConvBackend === undefined}
                      onClick={() => setNewConvBackend(undefined)}
                      className={chipCls(newConvBackend === undefined)}
                    >
                      默认（{wsSnapshot.backends.current}）
                    </button>
                    {wsSnapshot.backends.options.map((b) => (
                      <button
                        key={b}
                        type="button"
                        aria-pressed={newConvBackend === b}
                        onClick={() => setNewConvBackend(b)}
                        className={chipCls(newConvBackend === b)}
                      >
                        {b}
                      </button>
                    ))}
                  </div>
                </section>
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

      {/* v1.41.3 会话删除确认弹层（侧边栏长按 500ms 触发） */}
      {deleteTarget !== null && (
        <ConvDeleteConfirm
          convTitle={convTitle(state.spaces[state.currentMachine]?.convs[deleteTarget], deleteTarget)}
          busy={deleteBusy}
          onConfirm={confirmDeleteConv}
          onCancel={() => setDeleteTarget(null)}
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
        gatewayInfo={state.gatewayInfo}
        onClose={() => setAdminOpen(false)}
      />

      {/* v1.5 USER_TOKEN 轮换结果：一次性明文弹层（关闭即作废） */}
      {state.rotatedToken !== null && (
        <RotatedTokenModal
          token={state.rotatedToken}
          onDone={() => dispatch({ type: "clear_rotated_token" })}
        />
      )}

      {(state.lastError || state.lastNotice || localToast || (adminError && !adminOpen)) && (
        <div className="fixed inset-x-0 bottom-24 z-50 mx-auto flex w-fit max-w-[90vw] flex-col items-center gap-2">
          {/* v1.10 本地 toast（更新提示 / 通知开关反馈）：info 蓝 / error 红，
              节奏与 error/notice 一致（4s 自动消退；v1.41.7 sticky 形态例外，
              常驻不消退）。v1.39：带 onClick 的更新提示渲染为可点 button
              （下划线示意可点，样式随既有 pill），点击立即重载；其余仍为
              纯文本。 */}
          {localToast && (
            <div
              role="status"
              className={`rounded-full border px-4 py-2 text-center text-xs shadow-xl ${
                localToast.tone === "error"
                  ? "border-rose-400/60 bg-rose-50 text-rose-800 dark:border-rose-500/40 dark:bg-rose-950/95 dark:text-rose-200"
                  : "border-sky-400/60 bg-sky-50 text-sky-800 dark:border-sky-500/40 dark:bg-sky-950/95 dark:text-sky-200"
              }`}
            >
              {localToast.onClick ? (
                <button
                  type="button"
                  onClick={localToast.onClick}
                  className="cursor-pointer underline underline-offset-2"
                >
                  {localToast.text}
                </button>
              ) : (
                localToast.text
              )}
            </div>
          )}
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
