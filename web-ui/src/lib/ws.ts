// WS 连接管理（无 UI 依赖、可单测）：
// connect → 首帧 hello（凭证只在此帧出现，绝不进 URL）→ 等 hello_ack
// （10s 超时视为失败）→ ready → 立即发 sync（含重连场景）→ 25s 应用层 ping
// （relay idle 90s 断连；v1.41.9 起发给全部在线的已加密机器，见文末）→ 断线
// 指数退避重连 1s→30s（成功重置）→ 收 close 1008
// 按 reason 分流（session expired/revoked/auth failed，onAuthFailed 带
// AuthFailReason，调用方清对应凭证回登录页，不再重连）→ 收 close 4001
// 视为本连接被同 token 新窗口顶替（进入 superseded 终态：停心跳停重连，否则
// 两窗口互踢死循环；用户显式 connect() 才重连，反之顶替对方是用户意图）→
// >60s 无任何入站帧视为链路僵死，主动断开重连。
// v1.7.x 切回前台即时恢复（iOS 冻结/掐断后台 WS 的半开问题）：visibilitychange
// → visible 时探活——无连接且退避已排则清 timer 立即重连（退避计数保留防
// 风暴）；连接 OPEN 则立即执行一次心跳体逻辑（lastFrameAt 超过 pingIntervalMs
// 判半开：teardown + 立即重连；未超则发一次 ping 探活，真正死链由下个 tick
// 的 deadLink 判定兜底）。监听随 connect 注册、close/终态移除（防泄漏）。
// v1.1 多终端：业务出站帧（message/approval_response/control/sync/ping）可选带
// machine 字段——调用方逐帧传入，或经 getMachine 注入（内部自动 sync / 心跳
// ping 在发送时取值）。machine 未指定时字段不出现，与旧 relay/旧网关字节级
// 兼容；多机模式下 SPA 必须始终显式带当前机器（否则 relay 回 ambiguous_machine）。
// v1.2 会话层：hello 双形态——构造带 session（localStorage 恢复的会话凭证）
// 或 token+device（登录页首次登录）。hello_ack 携带 session 时经 onSession
// 上抛（App 持久化并删除 USER_TOKEN），此后重连 hello 自动改用 session 形态；
// hello_ack 无 session（旧 relay）→ 回退现行 token 直连模式（字节级一致）。
// v1.4 E2E 中继盲化：ready 后收 machines 帧即为每台在线机器发起 e2e 握手
// （E2eCoordinator，lib/e2e.ts）；出站业务帧（message/approval_response/
// control/sync/ping）按目标机器选会话——active → 包成 enc 帧，握手窗口/
// 显式明文机器 → 原帧明文，confirming/超时未放行/身份告警 → 拒发（fail-
// closed）。入站 enc 帧解密后二次分发为原帧走既有 onFrame；active 机器的
// 明文业务帧（注入面）丢弃。心跳 ping 同包装但永不阻断（保命线）。
// v1.5 准入与管理：管理帧（machine_list/machine_add/machine_remove/
// user_token_rotate/pairing_approve/pairing_deny，均带 adminToken）为 relay
// 自产自消帧，不在 e2e BUSINESS_TYPES 内、恒明文发送；配对模式（opts.pairing）
// hello 无凭证带 pairing:true+device，pairing_pending 清 hello 超时进入等待，
// 批准后同连接 hello_ack.session 走常规就绪；close 1008 "pairing denied"
// 分流为 pairing_denied 上抛。
// v1.6 工作空间：业务帧族新增 workspace_list / workspace_save /
// workspace_remove / conv_workdir_set（网关消费，经 e2e outbound 判定包装；
// machine 路由约定同 message 族——多机模式必须带当前机器）。
// v1.7 媒体上行：sendMessage 可选 media 参数（{name,mime,b64}，canvas 预压缩
// 产物）；帧仍在 message 出站路径上，E2E 加密与 machine 路由零变化。
// v1.8 会话历史重放：queryHistory(convId, since, machine) 出站 history_query
// 业务帧（E2E 加密），按本地高水位增量拉取网关出站日志；入站 history 帧（enc
// 内帧 / 旧网关明文）经 onFrame 二次分发——active 机器的明文 history 同属
// 注入面，与 text/card 族同规丢弃 + 降级告警。
// v1.40 语音转写 whisper 主路径：sendAsrRequest(audioB64, durationMs, machine)
// 出站 asr_request 业务帧（E2E 加密，同 message 族路由）；入站 asr_result 为
// Composer UI 服务的回包，**不进消息流**——连接层在 onFrame 分发前拦截（同
// pairing_pending 的非消息帧模式），经 onAsrResult 回调直达 App → Composer；
// active 机器的明文 asr_result 同属注入面，与 text/card 族同规丢弃 + 降级告警。
// v1.41.3 会话删除：sendConvDelete(convId, machine) 出站 conv_delete 业务帧
// （E2E 加密，同 message 族路由）——侧边栏长按删除确认后调用；网关回
// workspaces 快照（权威收敛）+ 回执 text 帧（落 DEFAULT_CONV），有进行中的
// 轮次回 error{conv_busy}（SPA 本地先行移除的条目由快照带回归位）。
// v1.40.1 前后台切换消息恢复：visible 探活 ping 发出后 visibleProbeMs（默认
// 3s）内无任何入站帧 → 判死链路（半开）立即重连——不再等下个心跳 tick 的
// 僵死判定（25s 间隔 + 60s 阈值），消除 iOS 挂起恢复后最坏 25~60s「收不到
// 消息」窗口；探活定时器随 stopTimers 一并清理（对齐 helloTimer/pingTimer
// 的清理纪律）。
// v1.41.9 E2E 会话探活：网关侧会话表随「网关 ↔ 中继」连接重建（网关重启/
// 重连即清空），浏览器自己的 WS 却可能还活着（桌面长开页面；同机重连顶替
// 时中继甚至不发下线快照）→ 此后浏览器拿作废密钥一直发，网关只 WARN 丢弃，
// 用户消息静默丢失到下一次 WS 重连。machines 快照后对已加密（active）的
// 机器各发一帧 enc-ping（网关每次连上中继必广播一帧快照）：sessionProbeMs
//（默认 3s）内收到该机器可解密的 pong = 网关仍持有我的会话密钥；未收到 =
// 会话已失效 → 立即重连（成熟的恢复路径：清 e2e 条目 → hello_ack → 明文
// sync → machines 帧重新握手，App 层随 status 变化补拉数据）。设计取舍与
// 防环论证见 probeSessions。
// v1.41.9 心跳逐机器：25s 应用层 ping 与切回前台探活不再只发当前机器，而是
// 对全部**在线**的已加密（active）机器各发一帧 enc-ping（当前机器不在其中
// 时照旧发它那一帧；每台每 tick 恰一帧）——网关半开清理只豁免收到过可解密
// 入站帧的会话，非当前机器的会话否则从未向网关证明持钥、60s 必被清，切过去
// sync / history_query 全部解不开而界面无感。理由与「只发在线机器」的取舍
// 见 probeOnce。
// v1.42 停止按钮落到当前会话：sendControl 增可选 convId（control 帧 convId 字段，
// 缺席帧字节不变）——此前 stop 不带会话号被网关落默认会话。
// v1.42 待审批快照：sendApprovalsQuery(machine) 出站 approvals_query 业务帧
// （E2E 加密，同 message 族路由）；入站 approvals 帧（网关回的该机器全部待
// 审批权威集合）经 onFrame 上抛由 App 层转 approvals_snapshot——active 机器的
// 明文 approvals 同属注入面，与 text/card 族同规丢弃 + 降级告警。
// v1.42 中继瞬态关闭不再当认证失败：close 1008 的 reason 若属瞬态白名单
// （rate limited / auth timeout / binary not supported）→ 走指数退避重连而非
// onAuthFailed（此前被限流即静默登出；见 TRANSIENT_CLOSE_REASONS）。
// v1.43 本地窗口看板：sendWindowsQuery(machine) 出站 windows_query 业务帧、
// sendWindowWatch(name, watch, machine) 出站 window_watch 业务帧（均 E2E 加密，
// 同 message 族路由）；入站 windows 帧（网关应答查询 / 处理完关注 / 轮询发现
// 快照变化时下发）经 onFrame 进 protocol reducer——active 机器的明文 windows
// 同属注入面（伪造一帧就能在看板上塞出「等你确认」的假窗口并顶掉状态条提示），
// 与 approvals 同规丢弃 + 降级告警。

import {
  DEFAULT_MACHINE,
  encodeFrame,
  normalizeExpiryMs,
  parseFrame,
  type ApprovalDecision,
  type AsrResultFrame,
  type EncFrame,
  type E2eAckFrame,
  type HelloAckFrame,
  type InboundFrame,
  type MediaPayload,
  type OutboundFrame,
  type SessionInfo,
} from "./protocol";
import { E2eCoordinator, type E2eEvent } from "./e2e";

export type ConnStatus = "connecting" | "ready" | "reconnecting" | "superseded";

/** 事件参数的最小形状（open/error 事件无载荷，message/close 各取所需字段）。 */
export type WsEvent = { data?: unknown; code?: number; reason?: string };

/** 最小 WebSocket 接口（可注入 fake 以单测；浏览器原生 WebSocket 满足之）。 */
export interface WebSocketLike {
  addEventListener(
    type: "open" | "message" | "close" | "error",
    listener: (ev: WsEvent) => void,
  ): void;
  removeEventListener(
    type: "open" | "message" | "close" | "error",
    listener: (ev: WsEvent) => void,
  ): void;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  readyState: number;
}

export interface WsManagerOptions {
  url: string;
  /** 首次登录形态：USER_TOKEN（v1.2 起一次性，签发 session 后不再使用）。 */
  token?: string;
  /** 重连形态：会话凭证（与 token 二选一，优先）。 */
  session?: string;
  /** 首次登录可选设备名（hello 帧 device 字段；旧 relay 忽略未知字段安全）。 */
  device?: string;
  /** v1.5 新设备配对模式：hello 带 pairing:true + device（无凭证），relay 回
   *  pairing_pending（连接保持未认证等待态），被批准后 relay 在本连接直接
   *  换发 hello_ack.session。与 token/session 互斥。 */
  pairing?: string;
  /** 全部合法入站帧（已过 parseFrame 校验）。 */
  onFrame: (frame: InboundFrame) => void;
  onStatus: (status: ConnStatus) => void;
  /** close 1008：按 reason 分流的认证失败（调用方应清对应凭证回登录页）。 */
  onAuthFailed: (reason: AuthFailReason) => void;
  /** hello_ack 携带 session（新 relay 签发）：调用方持久化并删除 USER_TOKEN。
   *  旧 relay 不触发（无 session 字段）——保持 USER_TOKEN 直连模式。 */
  onSession?: (session: SessionInfo) => void;
  /** v1.5 配对等待码（pairing_pending 帧，仅 pairing 模式；兼作「握手已应答」
   *  信号——清 hello 超时定时器，连接可无限期等待批准）。 */
  onPairingPending?: (code: string) => void;
  /** v1.4 E2E 事件上抛（TOFU 确认请求 / 身份变化告警 / 不支持加密 / 明文
   *  放行）。旧 relay 单机模式（无 machines 帧）恒不触发。 */
  onE2e?: (ev: E2eEvent) => void;
  /** v1.40 语音转写回包（asr_result 帧，连接层已拦截——不进 onFrame/消息
   *  流）。App 层转发给 Composer 回填输入框；旧网关无此帧（按无响应超时）。 */
  onAsrResult?: (frame: AsrResultFrame) => void;
  /** 注入 WebSocket 构造（单测用 fake）；缺省浏览器原生。 */
  wsFactory?: (url: string) => WebSocketLike;
  /** 当前出站机器（v1.1）：内部自动帧（ready 后首帧 sync / 心跳 ping）发送时
   *  调用取值；返回 undefined = 单机模式（不带 machine 字段，旧 relay 兼容）。 */
  getMachine?: () => string | undefined;
  /** 间隔常量可注入（默认即生产值：hello 10s / ping 25s / 僵死 60s /
   *  visible 探活判死 3s / E2E 会话探活 3s / 退避 1s→30s）。 */
  helloTimeoutMs?: number;
  pingIntervalMs?: number;
  deadLinkMs?: number;
  /** v1.40.1 visible 探活判死时限：探活 ping 发出后此时间内无任何入站帧
   *  → 判死链路立即重连（单测注入小值加速）。 */
  visibleProbeMs?: number;
  /** v1.41.9 E2E 会话探活时限：machines 快照后对已加密（active）机器发出
   *  enc-ping，此时间内未收到该机器可解密的 pong → 判会话在网关侧已失效，
   *  立即重连（单测注入小值加速）。 */
  sessionProbeMs?: number;
  minBackoffMs?: number;
  maxBackoffMs?: number;
}

/** relay 认证失败 close code（crates/relay CLOSE_POLICY = 1008）。 */
export const CLOSE_AUTH_FAILED = 1008;

/** relay 同 token 新连接顶替旧连接 close code（crates/relay CLOSE_SUPERSEDED = 4001）。 */
export const CLOSE_SUPERSEDED = 4001;

// ---------------------------------------------------------------------------
// v1.2 凭证存储（localStorage）
// ---------------------------------------------------------------------------

/** USER_TOKEN 存储位置（安全红线：只存 localStorage、只经 hello 帧发送，绝不
 *  进 URL；v1.2 起一次性——session 签发即删除）。 */
export const USER_TOKEN_KEY = "imagent.userToken";

/** 会话凭证存储位置（{id?, token, expiresAt}，30 天）。 */
export const SESSION_KEY = "imagent.session";

/** localStorage 持久化的会话凭证（id 可选——旧条目容错，有 id 才能标记「本机」）。 */
export type StoredSession = { id?: string; token: string; expiresAt: number };

export function saveSession(session: StoredSession): void {
  localStorage.setItem(SESSION_KEY, JSON.stringify(session));
}

export function clearSession(): void {
  localStorage.removeItem(SESSION_KEY);
}

/** 读取会话凭证：形状非法或已过期 → 清掉存储返回 null（过期会话重连必被
 *  relay 拒，本地预判免一次注定失败的握手）。 */
export function loadSession(): StoredSession | null {
  const raw = localStorage.getItem(SESSION_KEY);
  if (!raw) {
    return null;
  }
  const invalid = (): null => {
    localStorage.removeItem(SESSION_KEY);
    return null;
  };
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return invalid();
  }
  if (typeof parsed !== "object" || parsed === null) {
    return invalid();
  }
  const o = parsed as Record<string, unknown>;
  if (typeof o.token !== "string" || o.token === "" || typeof o.expiresAt !== "number") {
    return invalid();
  }
  if (normalizeExpiryMs(o.expiresAt) <= Date.now()) {
    return invalid();
  }
  return {
    id: typeof o.id === "string" ? o.id : undefined,
    token: o.token,
    expiresAt: o.expiresAt,
  };
}

// ---------------------------------------------------------------------------
// close 1008 reason 分流（协议 v1.2）
// ---------------------------------------------------------------------------

/** 认证失败细分（App 据此给登录页不同提示 / 清不同凭证）。v1.5 增
 *  pairing_denied（配对被拒，pending 连接被 relay 关闭）。v1.42 增三项：
 *  too_many_sessions（会话表满，签发被拒——本地凭证仍有效）/ pairing_expired
 *  （配对码过期）/ pairing_busy（同时进行的配对过多）。 */
export type AuthFailReason =
  | "session_expired"
  | "session_revoked"
  | "pairing_denied"
  | "too_many_sessions"
  | "pairing_expired"
  | "pairing_busy"
  | "auth_failed";

/** close 1008 reason → AuthFailReason：仅识别协议规定的字符串，其余
 *  （含缺席、旧 relay 的任意 reason）一律 auth_failed。 */
export function authFailReason(reason: string | undefined): AuthFailReason {
  if (reason === "session expired") {
    return "session_expired";
  }
  if (reason === "session revoked") {
    return "session_revoked";
  }
  if (reason === "pairing denied") {
    return "pairing_denied";
  }
  if (reason === "too many sessions") {
    return "too_many_sessions";
  }
  if (reason === "pairing expired") {
    return "pairing_expired";
  }
  if (reason === "too many pairings") {
    return "pairing_busy";
  }
  return "auth_failed";
}

/** 中继 close 1008 里属于**瞬态**的 reason 白名单（v1.42）。字面量来源：中继
 *  crates/relay/src/lib.rs 各处 close_msg(CLOSE_POLICY, …) 的原文，须逐字一致：
 *  - "rate limited"：该连接入站业务帧超过中继的滑动窗口限流档位——中继按
 *    角色分档，浏览器连接单独一档、远低于网关连接。快速切换会话 / 多机重连
 *    后的补拉突发就可能触到，与凭证无关（窗口长度与帧数上限以中继侧常量
 *    为准，此处不写死数字，免得两侧调参后注释失真）；
 *  - "auth timeout"：中继没在时限内收到 hello（网络抖动），下次连接重来即可；
 *  - "binary not supported"：连接上出现二进制帧——浏览器侧只发文本帧，属对端
 *    / 链路异常，换一条连接即恢复。
 *  这些原因下凭证并没有失效：此前一律按认证失败处理，App 清凭证回登录页且
 *  无提示——用户被限流即「静默登出」。改为走指数退避重连。
 *  **白名单而非黑名单**：没列出的 reason（含未来新增的）仍按认证失败处理——
 *  可见的失败优于对真实凭证问题的静默重连循环。 */
const TRANSIENT_CLOSE_REASONS: ReadonlySet<string> = new Set([
  "rate limited",
  "auth timeout",
  "binary not supported",
]);

/** close reason 是否属于瞬态（应重连而非认证失败）。精确匹配、大小写敏感
 *  （中继字面量固定；宽松匹配会把未知 reason 误归为瞬态）。 */
export function isTransientCloseReason(reason: string | undefined): boolean {
  return reason !== undefined && TRANSIENT_CLOSE_REASONS.has(reason);
}

/** v1.41.9 在途 E2E 会话探活（machines 快照后发起，见 WsManager.probeSessions）：
 *  socket = 发起时的连接（到点时不符即连接已换代，不动）；pending = 尚未
 *  收到可解密 pong 的机器；timer = 判失效定时器（随 stopTimers 清理）。 */
interface SessionProbe {
  socket: WebSocketLike;
  pending: Set<string>;
  timer: ReturnType<typeof setTimeout>;
}

export class WsManager {
  private readonly url: string;
  private readonly token: string | undefined;
  private readonly device: string | undefined;
  private readonly pairingDevice: string | undefined;
  private readonly onFrame: (frame: InboundFrame) => void;
  private readonly onStatus: (status: ConnStatus) => void;
  private readonly onAuthFailed: (reason: AuthFailReason) => void;
  private readonly onSession: ((session: SessionInfo) => void) | undefined;
  private readonly onPairingPending: ((code: string) => void) | undefined;
  private readonly onE2e: ((ev: E2eEvent) => void) | undefined;
  private readonly onAsrResult: ((frame: AsrResultFrame) => void) | undefined;
  private readonly wsFactory: (url: string) => WebSocketLike;
  private readonly getMachine: (() => string | undefined) | undefined;
  private readonly helloTimeoutMs: number;
  private readonly pingIntervalMs: number;
  private readonly deadLinkMs: number;
  private readonly visibleProbeMs: number;
  private readonly sessionProbeMs: number;
  private readonly minBackoffMs: number;
  private readonly maxBackoffMs: number;

  /** 当前会话凭证（构造注入或 hello_ack 签发）：一旦持有，重连 hello 即改用
   *  session 形态（USER_TOKEN 不再出现于任何后续帧）。 */
  private sessionToken: string | undefined;
  /** v1.4：会话 id（hello_ack.session.id）——e2e_hello 携带以升级定向加密
   *  形态（enc{sid} 只投本连接；旧 relay 无 session → 缺席走广播兼容）。 */
  private sessionId: string | undefined;

  private socket: WebSocketLike | null = null;
  private status: ConnStatus = "connecting";
  private ready = false;
  private attempts = 0; // 连续失败次数（成功后清零）
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private helloTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  /** v1.40.1 visible 探活判死定时器（一次性）：探活 ping 发出后无入站帧
   *  到点判死重连；随 stopTimers 清理（对齐 helloTimer/pingTimer 纪律）。 */
  private visibleProbeTimer: ReturnType<typeof setTimeout> | null = null;
  /** v1.41.9 在途 E2E 会话探活（null = 无在途探活；见 SessionProbe）。 */
  private sessionProbe: SessionProbe | null = null;
  /** v1.41.9 最近一帧 machines 快照的在线机器集：心跳只对**在线**的 active
   *  机器发 enc-ping（见 probeOnce）。无需随连接重置——e2e 条目随连接销毁
   *  清空，且只由 machines 快照之后的握手产生，故本集合恒不旧于条目。 */
  private onlineMachines: ReadonlySet<string> = new Set();
  private lastFrameAt = 0;
  private stopped = false; // 手动 close / 认证失败：不再重连
  private disconnectHandled = false; // 单次连接的 close 事件只处理一回
  /** v1.4 逐机器 e2e 状态机（会话临时密钥绑定当前连接，reset 随连接销毁）。 */
  private readonly e2e: E2eCoordinator;

  constructor(opts: WsManagerOptions) {
    this.url = opts.url;
    this.token = opts.token;
    this.device = opts.device;
    this.pairingDevice = opts.pairing;
    this.sessionToken = opts.session;
    this.onFrame = opts.onFrame;
    this.onStatus = opts.onStatus;
    this.onAuthFailed = opts.onAuthFailed;
    this.onSession = opts.onSession;
    this.onPairingPending = opts.onPairingPending;
    this.onE2e = opts.onE2e;
    this.onAsrResult = opts.onAsrResult;
    this.wsFactory = opts.wsFactory ?? ((u) => new WebSocket(u) as unknown as WebSocketLike);
    this.getMachine = opts.getMachine;
    this.helloTimeoutMs = opts.helloTimeoutMs ?? 10_000;
    this.pingIntervalMs = opts.pingIntervalMs ?? 25_000;
    this.deadLinkMs = opts.deadLinkMs ?? 60_000;
    this.visibleProbeMs = opts.visibleProbeMs ?? 3_000;
    this.sessionProbeMs = opts.sessionProbeMs ?? 3_000;
    this.minBackoffMs = opts.minBackoffMs ?? 1_000;
    this.maxBackoffMs = opts.maxBackoffMs ?? 30_000;
    this.e2e = new E2eCoordinator(
      {
        sendHello: (hello) => this.rawSend(encodeFrame(hello)),
        emit: (ev) => this.onE2e?.(ev),
      },
      { getSid: () => this.sessionId },
    );
  }

  /** 建立连接（幂等：重复调用无效果）。superseded 终态后可再次调用——
   *  用户显式重连（新连接会反过来顶替另一窗口，是用户意图）。 */
  connect(): void {
    if (this.socket) {
      return;
    }
    this.stopped = false;
    this.attachVisibility();
    this.setStatus("connecting");
    this.openSocket();
  }

  /** 手动关闭：停心跳、停重连（组件卸载 / 退出登录用）。 */
  close(): void {
    this.stopped = true;
    this.clearReconnectTimer();
    this.detachVisibility();
    this.teardownSocket();
  }

  // ---- 发送 API（ready 前返回 false）----
  // machine 传 undefined 时省略该字段（单机模式与 v1 字节级一致）；
  // 多机模式必须传当前选中机器（防 relay 回 ambiguous_machine）。

  /** convId 传 undefined 时省略该字段（网关落默认会话）——「+ 新会话」的
   *  /switch 命令文本走默认会话，由网关解析并切出 web:<名字>。
   *  v1.7：可选 media 附件（图片经 canvas 压缩，见 lib/imageCompress）；纯图
   *  消息 text 传 ""（契约 A 段：text 与 media 至少其一非空）。media 随
   *  message 帧自动走 E2E 加密（message 本就在 BUSINESS_TYPES）。 */
  sendMessage(
    convId: string | undefined,
    text: string,
    machine?: string,
    media?: MediaPayload,
  ): boolean {
    return this.send({ v: 1, type: "message", convId, text, machine, media });
  }

  /** convId / requestId 原样回传 approval_request 帧里的值；machine =
   *  审批来源机器（非当前选中机器的审批也回对应机器）。 */
  sendApprovalResponse(
    convId: string,
    requestId: string,
    decision: ApprovalDecision,
    machine?: string,
  ): boolean {
    return this.send({ v: 1, type: "approval_response", convId, requestId, decision, machine });
  }

  /** v1.42：convId 为目标会话（完整 conv 形态）。非空字符串才带上字段——
   *  缺席 / 空串时帧字节与升级前完全一致（缺省 = 网关落默认会话；旧网关
   *  忽略该字段）。停止按钮必须传当前会话，否则停的是默认会话的任务。 */
  sendControl(
    cmd: "stop" | "resume" | "switch",
    args?: string,
    machine?: string,
    convId?: string,
  ): boolean {
    return this.send({ v: 1, type: "control", cmd, args, ...(convId ? { convId } : {}), machine });
  }

  /** 机器切换后拉该机器卡片缓存（App 层调用；machine 缺省不带字段）。 */
  sendSync(machine?: string): boolean {
    return this.send({ v: 1, type: "sync", machine });
  }

  // ---- v1.6 工作空间（网关消费的业务帧；machine 缺省不带字段，多机模式
  //      必须传当前机器——同 message 族路由约定，经 e2e outbound 判定包装）----

  /** 拉取工作空间全量快照（网关回 workspaces；连接 ready / 切机后调用）。 */
  listWorkspaces(machine?: string): boolean {
    return this.send({ v: 1, type: "workspace_list", machine });
  }

  /** 保存工作空间（成功/失败均回 workspaces 快照，失败附 error 帧）。 */
  saveWorkspace(name: string, path: string, machine?: string): boolean {
    return this.send({ v: 1, type: "workspace_save", name, path, machine });
  }

  /** 移除工作空间（按 name；回 workspaces 快照）。 */
  removeWorkspace(name: string, machine?: string): boolean {
    return this.send({ v: 1, type: "workspace_remove", name, machine });
  }

  /** 绑定会话工作目录（成功回 conv_workdir 增量 + workspaces 快照）。 */
  setConvWorkdir(convId: string, path: string, machine?: string): boolean {
    return this.send({ v: 1, type: "conv_workdir_set", convId, path, machine });
  }

  // ---- v1.8 会话历史重放（网关消费的业务帧；同 message 族路由约定，经 e2e
  //      outbound 判定包装）----

  /** 增量拉取会话出站日志：since = 本地高水位（0 = 从头）；网关回 history 帧
   *  （重放条目经 reducer 合并，高水位由 App 层维护）。 */
  queryHistory(convId: string, since: number, machine?: string): boolean {
    return this.send({ v: 1, type: "history_query", convId, since, machine });
  }

  // ---- v1.40 语音转写（网关消费的业务帧；同 message 族路由约定，经 e2e
  //      outbound 判定包装；回包 asr_result 由连接层 onAsrResult 上抛）----

  /** 录音上行（网关 whisper 转写）：audio = 16kHz 单声道 16-bit WAV 的
   *  base64（SPA 纯前端编码，解码后 ≤8MB / 录音上限 60s 截停）；durationMs =
   *  实际录音时长。网关回 asr_result（新网关）或 error{asr_*}；旧网关无此
   *  帧——调用方按无响应超时处理。 */
  sendAsrRequest(audio: string, durationMs: number, machine?: string): boolean {
    return this.send({ v: 1, type: "asr_request", audio, mime: "audio/wav", durationMs, machine });
  }

  // ---- v1.41.3 会话删除（网关消费的业务帧；同 message 族路由约定，经 e2e
  //      outbound 判定包装）----

  /** 删除会话（侧边栏长按确认后调用）：convId 恒完整 conv 形态。网关删
   *  sessions/outbound_log/卡片缓存三处后回 workspaces 快照 + 回执文本帧
   *  （落 DEFAULT_CONV）；有进行中的轮次回 error{conv_busy} 拒删（快照照回
   *  ——调用方的本地先行移除由快照带回归位）。旧网关忽略未知帧 type，删除
   *  仅本地生效（调用方按帧返回值决定是否本地先行）。 */
  sendConvDelete(convId: string, machine?: string): boolean {
    return this.send({ v: 1, type: "conv_delete", convId, machine });
  }

  // ---- v1.42 待审批快照（网关消费的业务帧；同 message 族路由约定，经 e2e
  //      outbound 判定包装；应答 approvals 帧经 onFrame 上抛，由 App 层对齐）----

  /** 查询某机器当前全部待审批：网关回 approvals{machine, items}（权威集合，
   *  可空）。ready 后 / 该机器加密会话建立后各发一次（App 层）；旧网关不
   *  认识该帧、不回任何帧。返回 false = 未发出（未 ready / E2E fail-closed
   *  拒发）——调用方据此不记录查询时刻（没有查询就不会有权威应答）。 */
  sendApprovalsQuery(machine?: string): boolean {
    return this.send({ v: 1, type: "approvals_query", machine });
  }

  // ---- v1.43 本地窗口看板（网关消费的业务帧；同 message 族路由约定，经 e2e
  //      outbound 判定包装；应答 windows 帧经 onFrame 进 reducer）----

  /** 请求某机器当前在线的本地窗口快照：网关回 windows{machine, hookInstalled,
   *  items}。ready 后 / 该机器加密会话建立后各发一次（App 层）；peer 通道未启用
   *  或旧网关不回任何帧——调用方以「有没有收到过快照」决定显不显示看板。 */
  sendWindowsQuery(machine?: string): boolean {
    return this.send({ v: 1, type: "windows_query", machine });
  }

  /** 关注 / 取消关注一个本地窗口（name = 快照条目的窗口名）。关注归网关所有并
   *  由网关持久化；网关处理完会重发一帧 windows 让所有设备的 watched 收敛。
   *  返回 false = 未发出（未 ready / E2E fail-closed 拒发）——调用方此时不做乐观
   *  更新：前端没有名单兜底，界面若抢先说「已关注」，要到下一帧快照才会被拨回。
   *  键序照契约示例：v / type / machine / name / watch。 */
  sendWindowWatch(name: string, watch: boolean, machine?: string): boolean {
    return this.send({ v: 1, type: "window_watch", machine, name, watch });
  }

  /** v1.2 拉取已登录设备列表（设备面板打开时；仅会话模式对新 relay 使用）。 */
  sendDeviceList(): boolean {
    return this.send({ v: 1, type: "device_list" });
  }

  /** v1.2 吊销设备（relay 回更新后的 devices 帧）。 */
  sendDeviceRevoke(deviceId: string): boolean {
    return this.send({ v: 1, type: "device_revoke", deviceId });
  }

  /** v1.3 拉取已注册通行钥匙列表（设备面板打开时）。 */
  sendCredList(): boolean {
    return this.send({ v: 1, type: "cred_list" });
  }

  /** v1.3 删除通行钥匙（relay 回更新后的 creds 帧）。 */
  sendCredRevoke(credId: string): boolean {
    return this.send({ v: 1, type: "cred_revoke", credId });
  }

  // ---- v1.5 管理帧（浏览器 → relay 自产自消；adminToken 由调用方经
  //      lib/admin 的 ensureAdminToken 取得。管理帧恒明文——e2e 层只包装
  //      BUSINESS_TYPES 业务帧，管理帧不在其列，原样发送）----

  /** 拉取机器配置列表（relay 回 machines_cfg）。 */
  sendMachineList(adminToken: string): boolean {
    return this.send({ v: 1, type: "machine_list", adminToken });
  }

  /** 添加机器（成功回 machines_cfg；重名/格式错回 error{machine_invalid}）。 */
  sendMachineAdd(adminToken: string, name: string, pubkey: string): boolean {
    return this.send({ v: 1, type: "machine_add", adminToken, name, pubkey });
  }

  /** 移除机器（回 machines_cfg；该机器网关连接被 relay 断开）。 */
  sendMachineRemove(adminToken: string, name: string): boolean {
    return this.send({ v: 1, type: "machine_remove", adminToken, name });
  }

  /** 轮换 USER_TOKEN（回 user_token_rotated，新值仅此一次返回明文）。 */
  sendUserTokenRotate(adminToken: string): boolean {
    return this.send({ v: 1, type: "user_token_rotate", adminToken });
  }

  /** 配对批准（管理级，adminToken 须先经 re-auth 取得）。 */
  sendPairingApprove(adminToken: string, code: string): boolean {
    return this.send({ v: 1, type: "pairing_approve", adminToken, code });
  }

  /** 配对拒绝（管理级，同上）。 */
  sendPairingDeny(adminToken: string, code: string): boolean {
    return this.send({ v: 1, type: "pairing_deny", adminToken, code });
  }

  // ---- v1.4 E2E（TOFU 决断 / 明文放行 / pin 重置后重试）----

  /** TOFU 弹层决断：accept = 信任并 pin，随后以 pin 身份重新握手（v1.41.9：
   *  active 由新握手的 e2e_ack 触发，在途期间出站业务帧阻断）；false = 拒绝
   *  （转 unsupported，可再显式选择明文）。 */
  resolveTofu(machine: string, accept: boolean): void {
    this.e2e.resolveTofu(machine, accept);
  }

  /** 「仍以明文连接」显式选择：放行该机器发送（明文）并持久记录。 */
  allowPlaintextE2e(machine: string): void {
    this.e2e.allowPlaintext(machine);
  }

  /** 设备管理重置 pin 后重试握手（重新 TOFU 确认）。 */
  retryE2e(machine: string): void {
    this.e2e.retry(machine);
  }

  private send(frame: OutboundFrame): boolean {
    if (!this.ready || !this.socket) {
      return false;
    }
    // v1.4：业务帧按目标机器选会话（active → enc；fail-closed 态拒发）
    const wire = this.e2e.outbound(this.targetMachineOf(frame), frame);
    if (wire === null) {
      return false;
    }
    this.rawSend(wire);
    return true;
  }

  /** 出站帧归属机器：显式 machine 字段优先，业务帧缺省时注入当前机器
   *  （undefined = 单机模式，e2e 层恒明文直发）。 */
  private targetMachineOf(frame: OutboundFrame): string | undefined {
    if ("machine" in frame && typeof frame.machine === "string") {
      return frame.machine;
    }
    return this.machineOf();
  }

  private rawSend(data: string): void {
    try {
      this.socket?.send(data);
    } catch {
      // 发送异常视作断链：close 事件随后驱动重连
    }
  }

  // ---- 内部生命周期 ----

  private openSocket(): void {
    this.disconnectHandled = false;
    this.ready = false;
    const sock = this.wsFactory(this.url);
    this.socket = sock;
    sock.addEventListener("open", this.handleOpen);
    sock.addEventListener("message", this.handleMessage);
    sock.addEventListener("close", this.handleClose);
    sock.addEventListener("error", this.handleError);
  }

  private handleOpen = (): void => {
    if (!this.socket || this.stopped) {
      return;
    }
    // 连接后第一帧 hello（relay 认证；成功收 hello_ack，失败 close 1008）
    this.sendHello();
    this.helloTimer = setTimeout(() => {
      // 10s 无 hello_ack：按失败处理（先摘监听再关 socket，close 事件不再触发）
      this.teardownSocket();
      this.scheduleReconnect();
    }, this.helloTimeoutMs);
  };

  /** hello 三形态（v1.2 + v1.5）：配对模式 → pairing+device（无凭证，relay
   *  回 pairing_pending，批准后同连接换发 hello_ack.session）；持有会话凭证
   *  → session 形态；否则 token 形态（+可选 device）。undefined 字段不序列化，
   *  token 形态无 device 时与 v1.1 字节级一致（旧 relay 兼容）。 */
  private sendHello(): void {
    const hello: OutboundFrame = this.pairingDevice
      ? { v: 1, type: "hello", pairing: true, device: this.pairingDevice }
      : this.sessionToken
        ? { v: 1, type: "hello", session: this.sessionToken }
        : { v: 1, type: "hello", token: this.token, device: this.device };
    this.rawSend(encodeFrame(hello));
  }

  private handleMessage = (ev: WsEvent): void => {
    if (typeof ev.data !== "string") {
      return;
    }
    const frame = parseFrame(ev.data);
    if (!frame) {
      return;
    }
    this.lastFrameAt = Date.now();
    if (frame.type === "hello_ack" && !this.ready) {
      this.onHelloAck(frame);
      return;
    }
    // v1.5：配对等待码由配对模式连接层消费（清 hello 超时，进入无限期等待；
    // 批准后 relay 在本连接发 hello_ack 走下方常规就绪路径）
    if (frame.type === "pairing_pending") {
      this.clearHelloTimer();
      this.onPairingPending?.(frame.code);
      return;
    }
    // v1.4：握手应答由 e2e 协调器消费（不进 onFrame）
    if (frame.type === "e2e_ack") {
      this.e2e.onAck(frame as E2eAckFrame);
      return;
    }
    // v1.40：语音转写回包是 Composer UI 服务的回包，不进消息流（不落
    // entries/概要/未读）——连接层直接回调消费（同 pairing_pending 的非消息
    // 帧模式）；active 机器的明文 asr_result 同属注入面（网关回包恒 E2E 加
    // 密），与 text/card 族同规丢弃 + 降级告警。
    if (frame.type === "asr_result") {
      if (!this.e2e.acceptPlaintextBusiness(frame.machine ?? DEFAULT_MACHINE)) {
        this.e2e.notePlaintextDowngrade(frame.machine ?? DEFAULT_MACHINE);
        return;
      }
      this.onAsrResult?.(frame as AsrResultFrame);
      return;
    }
    // v1.4：enc 帧 → 解密后二次分发为原帧，走既有处理路径
    if (frame.type === "enc") {
      const enc = frame as EncFrame;
      const inner = this.e2e.openEnc(enc);
      if (inner) {
        // v1.41.9：会话探活应答在此判定（见 probeSessions）——只认可解密的
        // pong；解不开的 enc 帧（含网关按别的密钥密封的 pong）在 openEnc
        // 已返回 null，不算应答。
        if (inner.type === "pong") {
          this.noteSessionPong(enc.machine);
        }
        // v1.1：enc 信封 machine 是路由权威来源（relay 按它路由、E2E 会话
        // 按它索引）；内帧漏带 machine（网关新帧型忘带）时注入兜底，防
        // reducer 按 DEFAULT_MACHINE 归错机器——内帧已带则不覆盖（自报优先）。
        (inner as { machine?: string }).machine ??= enc.machine;
        this.onFrame(inner);
      }
      return;
    }
    // v1.4：machines 快照驱动逐机器握手；加密会话已立（active/alert）的机器
    // 发来明文业务帧 = 注入面 → 丢弃 + 降级告警（active 时；该降级的唯一
    // 可观测点）。缺省 machine 归 DEFAULT_MACHINE。
    // v1.42：approvals（待审批快照）同列——它会让面板上的待审批集合被整体
    // 替换，伪造一帧就能凭空塞进 / 抹掉审批按钮，注入面必须与 text/card 族
    // 同规封住（网关应答恒经 send_business 加密，active 后到明文即降级）。
    // v1.43：windows（本地窗口快照）同理——它会被整块渲染成侧栏看板，并决定
    // 状态条出不出「窗口在等你」；伪造一帧就能诱导用户点进一个不存在的会话。
    if (frame.type === "machines") {
      // v1.41.9：记下在线机器集（心跳只对在线的 active 机器发 enc-ping，见
      // probeOnce）→ 让新出现的机器进入握手 → 对已加密会话探活（此刻新机器
      // 是 pending，不会被探——防环）。
      this.onlineMachines = new Set(frame.online);
      this.e2e.onMachines(frame.online);
      this.probeSessions(frame.online);
    } else if (
      (frame.type === "text" ||
        frame.type === "card" ||
        frame.type === "card_patch" ||
        frame.type === "approval_request" ||
        frame.type === "approval_resolved" ||
        frame.type === "history" ||
        frame.type === "approvals" ||
        frame.type === "windows") &&
      !this.e2e.acceptPlaintextBusiness(frame.machine ?? DEFAULT_MACHINE)
    ) {
      this.e2e.notePlaintextDowngrade(frame.machine ?? DEFAULT_MACHINE);
      return;
    }
    this.onFrame(frame);
  };

  private onHelloAck(frame: HelloAckFrame): void {
    this.clearHelloTimer();
    // v1.2：hello_ack 携带 session（新 relay 签发）→ 记住凭证（此后重连 hello
    // 改用 session 形态）并上抛给调用方持久化；缺席（旧 relay）→ 无操作，
    // 保持 token 直连模式（字节级回退现行行为）。
    if (frame.session) {
      this.sessionToken = frame.session.token;
      this.sessionId = frame.session.id;
      this.onSession?.(frame.session);
    }
    this.ready = true;
    this.attempts = 0; // 成功即重置退避
    this.setStatus("ready");
    // 连接成功（含重连）后必须立即发 sync：网关重发全部会话最近卡片终态。
    // v1.1：重连时多机模式经 getMachine 带上当前机器；首连尚未收到 machines
    // 帧（getMachine 为 undefined）不带字段——machines 帧到达后由 App 层再发
    // 一次带 machine 的 sync（幂等，卡片按 messageId 覆盖）。
    this.rawSend(encodeFrame({ v: 1, type: "sync", machine: this.machineOf() }));
    this.lastFrameAt = Date.now();
    this.startHeartbeat();
  };

  /** 内部自动帧（sync/ping）的出站机器：注入取值，undefined 不带字段。 */
  private machineOf(): string | undefined {
    return this.getMachine?.();
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.pingTimer = setInterval(() => this.probeOnce(this.deadLinkMs), this.pingIntervalMs);
  }

  /** 心跳体逻辑（周期 tick 与切回前台探活共用）：距最后入站帧超过
   *  thresholdMs 判链路失活 → 主动断开 + 排重连；否则发 ping（经 e2e
   *  按机器选路包装，保命线永不阻断）。tick 用 deadLinkMs（僵死判定），
   *  前台探活用 pingIntervalMs（半开判定——iOS 后台掐链后 readyState 仍
   *  OPEN，等 deadLink 太迟）。
   *
   *  v1.41.9 ping 发给**全部在线的已加密（active）机器**，不再只发当前机器
   *  （勿改回）：网关半开清理（crates/web/src/e2e.rs HALF_OPEN_TTL）只豁免
   *  「收到过可解密入站帧」的会话——非当前机器的会话此前从未向网关证明
   *  持钥，60s 后必被清，切过去 sync / history_query 全部解不开而界面无感
   *  （静默失效到下一次 machines 快照或 WS 重连）。每个 active 会话持续
   *  enc-ping 即持续确认，属预防而非事后恢复。
   *  - 只发在线机器（当前机器除外）：e2e 条目不随机器掉线清除，而中继对
   *    「显式带 machine 且该机器不在线」的帧一律回 machine_offline error
   *    （enc 信封也带 machine，不豁免；App 渲染为红色 toast）——不过滤就是
   *    每 tick（25s）弹一次「该机器离线，消息未送达」。
   *  - 当前机器不在上述集合（无条目 / pending / confirming / unsupported /
   *    明文选择 / 单机模式 / 全部掉线）仍照旧发它那一帧：保命线恒放行，
   *    与 v1.41.8 逐字节一致；已在集合里则不重复发（每台每 tick 恰一帧）。 */
  private probeOnce(thresholdMs: number): void {
    if (Date.now() - this.lastFrameAt > thresholdMs) {
      this.teardownSocket();
      this.scheduleReconnect();
      return;
    }
    const active = this.e2e.activeMachines();
    const current = this.machineOf();
    for (const m of active) {
      if (m !== current && !this.onlineMachines.has(m)) {
        continue; // 机器已掉线（条目未清）：不发，免得中继回 machine_offline
      }
      this.rawSend(this.e2e.heartbeat({ v: 1, type: "ping", machine: m }, m));
    }
    if (current === undefined || !active.includes(current)) {
      this.rawSend(this.e2e.heartbeat({ v: 1, type: "ping", machine: current }, current));
    }
  }

  // ---- v1.7.x 切回前台探活（visibilitychange）----

  /** document 可见性监听（connect 注册 / close 与终态移除；SSR/单测环境无
   *  document 时静默跳过）。同一函数引用重复 addEventListener 幂等。 */
  private attachVisibility(): void {
    if (typeof document === "undefined") {
      return;
    }
    document.addEventListener("visibilitychange", this.handleVisibilityChange);
  }

  private detachVisibility(): void {
    if (typeof document === "undefined") {
      return;
    }
    document.removeEventListener("visibilitychange", this.handleVisibilityChange);
  }

  /** 切回前台即时探活：visible 且未 stopped/superseded 时——
   *  ① socket 为 null 且退避定时器已排：清 timer 立即重连（attempts 计数
   *     保留——立即重试一次不抬退避上限，防连续切前后台风暴）；
   *  ② socket OPEN 且 ready：执行一次心跳体逻辑（pingIntervalMs 阈值判
   *     半开；新鲜则发 ping 探活，并设 visibleProbeMs 一次性定时器——ping
   *     发出后无任何入站帧即判死重连，v1.40.1 不再等下个 tick 的僵死判定，
   *     消除 iOS 挂起恢复后的长「收不到消息」窗口）。非 OPEN（连接中/hello
   *     等待）不动——hello 超时与 close 事件自会驱动状态机。 */
  private handleVisibilityChange = (): void => {
    if (document.visibilityState !== "visible" || this.stopped) {
      return;
    }
    if (this.socket === null) {
      if (this.reconnectTimer !== null) {
        this.clearReconnectTimer();
        this.setStatus("connecting");
        this.openSocket();
      }
      return;
    }
    if (this.ready && this.socket.readyState === 1 /* OPEN */) {
      this.probeOnce(this.pingIntervalMs);
      // probeOnce 内部判半开（>pingIntervalMs 陈旧）已 teardown + 排重连，
      // 此时 socket 为 null——不再设探活定时器（重连已由退避路径接管）。
      if (this.socket === null) {
        return;
      }
      const sentAt = Date.now();
      this.clearVisibleProbeTimer();
      this.visibleProbeTimer = setTimeout(() => {
        this.visibleProbeTimer = null;
        // ping 发出后 visibleProbeMs 内无任何入站帧 → 链路已死（半开），
        // 立即重连；probeOnce 已判死 teardown（socket 变 null）或已非 ready
        // 则不动，由既有状态机接管。
        if (this.socket !== null && this.ready && this.lastFrameAt < sentAt) {
          this.teardownSocket();
          this.scheduleReconnect();
        }
      }, this.visibleProbeMs);
    }
  };

  // ---- v1.41.9 E2E 会话探活（machines 快照后）----

  /** machines 快照后对已加密会话逐机器探活——判定「网关侧是否还持有我的
   *  会话密钥」的权威探针：网关对应用层 ping 回 pong（经其 send_business
   *  逐 sid 加密），持有则浏览器能解开，不持有则什么都收不到。只探 active
   *  （pending/confirming/unsupported/alert 的会话未立或已阻断，无「失效」
   *  可言）。设计取舍（勿改回）：
   *  - 为什么探活而不直接重握手：探活健康时零副作用；重握手则每帧快照都让
   *    全部机器的密钥换代（退回 pending 等 ack），而快照因任一网关上下线
   *    就会到来，与「我的会话是否失效」无关。
   *  - 为什么不做协议帧（网关会话失效时通知浏览器）：同一会话（同 sid）多
   *    标签页并存时网关只保留最后一次握手的密钥（「最后握手者有效」是既有
   *    约定），通知被顶替的一方 = 引入多标签互踢机制。
   *  - 为什么只认 pong：pong 只在网关成功解密我的 ping 之后才回；会话表为
   *    空时网关的 pong 回的是明文（解不开 → 不算应答，正是失效形态）；
   *    其它可解密帧（如重启前在途的 text）证明不了网关此刻持有密钥。
   *  防环：探活失败的处理是重连而不是再探活；重连后首帧 machines 到达时
   *  条目都是 pending（握手刚开始），不会被探。 */
  private probeSessions(online: string[]): void {
    // 新快照到达：在途探活作废，按新快照重新发起（避免多次快照叠加）
    this.clearSessionProbe();
    const pending = new Set<string>();
    for (const m of online) {
      if (this.e2e.statusOf(m) !== "active") {
        continue;
      }
      this.rawSend(this.e2e.heartbeat({ v: 1, type: "ping", machine: m }, m));
      pending.add(m);
    }
    const sock = this.socket;
    if (pending.size === 0 || sock === null) {
      return;
    }
    this.sessionProbe = {
      socket: sock,
      pending,
      timer: setTimeout(() => {
        this.sessionProbe = null;
        // 连接已换代（期间重连/关闭——定时器虽随 stopTimers 清理，仍按发起
        // 时连接核对）：不动，由既有状态机接管。
        if (this.socket !== sock || !this.ready) {
          return;
        }
        // 仍有机器未应答：其会话在网关侧已失效 → 立即重连（不走退避）。
        // teardownSocket 已清空 e2e 条目：新连接 machines 帧重新握手，App 层
        // 随 status 变化补拉 sync / workspace_list / history_query。
        this.teardownSocket();
        this.setStatus("reconnecting");
        this.openSocket();
      }, this.sessionProbeMs),
    };
  }

  /** 会话探活应答（v1.41.9）：收到 pending 中某机器可解密的 pong → 移出；
   *  pending 清空即清定时器（网关仍持有全部在探会话的密钥，连接不动）。 */
  private noteSessionPong(machine: string): void {
    const probe = this.sessionProbe;
    if (probe !== null && probe.pending.delete(machine) && probe.pending.size === 0) {
      this.clearSessionProbe();
    }
  }

  private clearSessionProbe(): void {
    if (this.sessionProbe !== null) {
      clearTimeout(this.sessionProbe.timer);
      this.sessionProbe = null;
    }
  }

  private handleClose = (ev: WsEvent): void => {
    this.stopTimers();
    // v1.4：连接已断——e2e 会话密钥绑定本连接，立即作废（对端 close 事件
    // 不经 teardownSocket 路径，须在此显式清；重连后 machines 帧重新握手）
    this.e2e.reset();
    if (this.disconnectHandled) {
      return;
    }
    this.disconnectHandled = true;
    this.socket = null;
    if (this.stopped) {
      return;
    }
    const code = typeof ev.code === "number" ? ev.code : 1006;
    const reason = typeof ev.reason === "string" ? ev.reason : undefined;
    // v1.42：1008 但属瞬态原因（限流等）→ 凭证仍有效，不按认证失败处理，
    // 落到下方常规重连（reconnecting + 指数退避；既有状态机语义不变）。
    if (code === CLOSE_AUTH_FAILED && !isTransientCloseReason(reason)) {
      // relay 认证失败：按 close reason 分流（session expired/revoked/其它），
      // 调用方清对应凭证回登录页；不重连
      this.stopped = true;
      this.detachVisibility();
      this.onAuthFailed(authFailReason(reason));
      return;
    }
    if (code === CLOSE_SUPERSEDED) {
      // 本连接被同 token 新窗口顶替：进入终态，不自动重连
      // （自动重连会顶替回去，两个窗口互踢死循环）；用户显式 connect() 才重连。
      this.stopped = true;
      this.detachVisibility();
      this.setStatus("superseded");
      return;
    }
    this.scheduleReconnect();
  };

  /** error 事件后浏览器必然跟 close 事件，重连统一在 handleClose 处理。 */
  private handleError = (): void => {
    // no-op（占位使监听语义显式）
  };

  private scheduleReconnect(): void {
    if (this.stopped || this.reconnectTimer) {
      return;
    }
    this.setStatus("reconnecting");
    const delay = Math.min(this.minBackoffMs * 2 ** this.attempts, this.maxBackoffMs);
    this.attempts += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.openSocket();
    }, delay);
  }

  /** 摘监听并关 socket（不发 close 事件，重连由调用方决定）。v1.4：同步
   *  清空 e2e 会话（临时密钥绑定本连接；重连后 machines 帧重新握手）。 */
  private teardownSocket(): void {
    this.stopTimers();
    this.e2e.reset();
    const sock = this.socket;
    this.socket = null;
    if (!sock) {
      return;
    }
    sock.removeEventListener("open", this.handleOpen);
    sock.removeEventListener("message", this.handleMessage);
    sock.removeEventListener("close", this.handleClose);
    sock.removeEventListener("error", this.handleError);
    try {
      sock.close(1000);
    } catch {
      // 已断开的 socket 再 close 可能抛错，忽略
    }
  }

  /** 连接终止路径（handleClose / teardownSocket→close()）统一清理：hello
   *  超时、心跳、visible 探活判死定时器（v1.40.1——4001/1008 分流同样先过
   *  此处，终态不漏清）、E2E 会话探活定时器（v1.41.9——会话随连接作废，
   *  探活结论对新连接无意义）。 */
  private stopTimers(): void {
    this.clearHelloTimer();
    this.stopHeartbeat();
    this.clearVisibleProbeTimer();
    this.clearSessionProbe();
  }

  private clearHelloTimer(): void {
    if (this.helloTimer !== null) {
      clearTimeout(this.helloTimer);
      this.helloTimer = null;
    }
  }

  private clearVisibleProbeTimer(): void {
    if (this.visibleProbeTimer !== null) {
      clearTimeout(this.visibleProbeTimer);
      this.visibleProbeTimer = null;
    }
  }

  private stopHeartbeat(): void {
    if (this.pingTimer !== null) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private setStatus(status: ConnStatus): void {
    if (this.status === status) {
      return;
    }
    this.status = status;
    this.onStatus(status);
  }
}
