// WS 连接管理（无 UI 依赖、可单测）：
// connect → 首帧 hello（凭证只在此帧出现，绝不进 URL）→ 等 hello_ack
// （10s 超时视为失败）→ ready → 立即发 sync（含重连场景）→ 25s 应用层 ping
// （relay idle 90s 断连）→ 断线指数退避重连 1s→30s（成功重置）→ 收 close 1008
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

import {
  DEFAULT_MACHINE,
  encodeFrame,
  normalizeExpiryMs,
  parseFrame,
  type ApprovalDecision,
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
  /** 注入 WebSocket 构造（单测用 fake）；缺省浏览器原生。 */
  wsFactory?: (url: string) => WebSocketLike;
  /** 当前出站机器（v1.1）：内部自动帧（ready 后首帧 sync / 心跳 ping）发送时
   *  调用取值；返回 undefined = 单机模式（不带 machine 字段，旧 relay 兼容）。 */
  getMachine?: () => string | undefined;
  /** 间隔常量可注入（默认即生产值：hello 10s / ping 25s / 僵死 60s / 退避 1s→30s）。 */
  helloTimeoutMs?: number;
  pingIntervalMs?: number;
  deadLinkMs?: number;
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
 * pairing_denied（配对被拒，pending 连接被 relay 关闭）。 */
export type AuthFailReason =
  | "session_expired"
  | "session_revoked"
  | "pairing_denied"
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
  return "auth_failed";
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
  private readonly wsFactory: (url: string) => WebSocketLike;
  private readonly getMachine: (() => string | undefined) | undefined;
  private readonly helloTimeoutMs: number;
  private readonly pingIntervalMs: number;
  private readonly deadLinkMs: number;
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
    this.wsFactory = opts.wsFactory ?? ((u) => new WebSocket(u) as unknown as WebSocketLike);
    this.getMachine = opts.getMachine;
    this.helloTimeoutMs = opts.helloTimeoutMs ?? 10_000;
    this.pingIntervalMs = opts.pingIntervalMs ?? 25_000;
    this.deadLinkMs = opts.deadLinkMs ?? 60_000;
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

  sendControl(cmd: "stop" | "resume" | "switch", args?: string, machine?: string): boolean {
    return this.send({ v: 1, type: "control", cmd, args, machine });
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

  /** TOFU 弹层决断：accept = 信任并 pin + 启用加密；false = 拒绝（转
   *  unsupported，可再显式选择明文）。 */
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
    // v1.4：enc 帧 → 解密后二次分发为原帧，走既有处理路径
    if (frame.type === "enc") {
      const enc = frame as EncFrame;
      const inner = this.e2e.openEnc(enc);
      if (inner) {
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
    if (frame.type === "machines") {
      this.e2e.onMachines(frame.online);
    } else if (
      (frame.type === "text" ||
        frame.type === "card" ||
        frame.type === "card_patch" ||
        frame.type === "approval_request" ||
        frame.type === "approval_resolved") &&
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
   *  thresholdMs 判链路失活 → 主动断开 + 排重连；否则发一次 ping（经 e2e
   *  按当前机器选路包装，保命线永不阻断）。tick 用 deadLinkMs（僵死判定），
   *  前台探活用 pingIntervalMs（半开判定——iOS 后台掐链后 readyState 仍
   *  OPEN，等 deadLink 太迟）。 */
  private probeOnce(thresholdMs: number): void {
    if (Date.now() - this.lastFrameAt > thresholdMs) {
      this.teardownSocket();
      this.scheduleReconnect();
      return;
    }
    this.rawSend(
      this.e2e.heartbeat({ v: 1, type: "ping", machine: this.machineOf() }, this.machineOf()),
    );
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
   *     半开；新鲜则发 ping 探活）。非 OPEN（连接中/hello 等待）不动——
   *     hello 超时与 close 事件自会驱动状态机。 */
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
    }
  };

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
    if (code === CLOSE_AUTH_FAILED) {
      // relay 认证失败：按 close reason 分流（session expired/revoked/其它），
      // 调用方清对应凭证回登录页；不重连
      this.stopped = true;
      this.detachVisibility();
      this.onAuthFailed(authFailReason(typeof ev.reason === "string" ? ev.reason : undefined));
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

  private stopTimers(): void {
    this.clearHelloTimer();
    this.stopHeartbeat();
  }

  private clearHelloTimer(): void {
    if (this.helloTimer !== null) {
      clearTimeout(this.helloTimer);
      this.helloTimer = null;
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
