// WS 连接管理（无 UI 依赖、可单测）：
// connect → 首帧 hello（凭证只在此帧出现，绝不进 URL）→ 等 hello_ack
// （10s 超时视为失败）→ ready → 立即发 sync（含重连场景）→ 25s 应用层 ping
// （relay idle 90s 断连）→ 断线指数退避重连 1s→30s（成功重置）→ 收 close 1008
// 按 reason 分流（session expired/revoked/auth failed，onAuthFailed 带
// AuthFailReason，调用方清对应凭证回登录页，不再重连）→ 收 close 4001
// 视为本连接被同 token 新窗口顶替（进入 superseded 终态：停心跳停重连，否则
// 两窗口互踢死循环；用户显式 connect() 才重连，反之顶替对方是用户意图）→
// >60s 无任何入站帧视为链路僵死，主动断开重连。
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
  /** 全部合法入站帧（已过 parseFrame 校验）。 */
  onFrame: (frame: InboundFrame) => void;
  onStatus: (status: ConnStatus) => void;
  /** close 1008：按 reason 分流的认证失败（调用方应清对应凭证回登录页）。 */
  onAuthFailed: (reason: AuthFailReason) => void;
  /** hello_ack 携带 session（新 relay 签发）：调用方持久化并删除 USER_TOKEN。
   *  旧 relay 不触发（无 session 字段）——保持 USER_TOKEN 直连模式。 */
  onSession?: (session: SessionInfo) => void;
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

/** 认证失败细分（App 据此给登录页不同提示 / 清不同凭证）。 */
export type AuthFailReason = "session_expired" | "session_revoked" | "auth_failed";

/** close 1008 reason → AuthFailReason：仅识别协议规定的三种字符串，其余
 *  （含缺席、旧 relay 的任意 reason）一律 auth_failed。 */
export function authFailReason(reason: string | undefined): AuthFailReason {
  if (reason === "session expired") {
    return "session_expired";
  }
  if (reason === "session revoked") {
    return "session_revoked";
  }
  return "auth_failed";
}

export class WsManager {
  private readonly url: string;
  private readonly token: string | undefined;
  private readonly device: string | undefined;
  private readonly onFrame: (frame: InboundFrame) => void;
  private readonly onStatus: (status: ConnStatus) => void;
  private readonly onAuthFailed: (reason: AuthFailReason) => void;
  private readonly onSession: ((session: SessionInfo) => void) | undefined;
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
    this.sessionToken = opts.session;
    this.onFrame = opts.onFrame;
    this.onStatus = opts.onStatus;
    this.onAuthFailed = opts.onAuthFailed;
    this.onSession = opts.onSession;
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
    this.setStatus("connecting");
    this.openSocket();
  }

  /** 手动关闭：停心跳、停重连（组件卸载 / 退出登录用）。 */
  close(): void {
    this.stopped = true;
    this.clearReconnectTimer();
    this.teardownSocket();
  }

  // ---- 发送 API（ready 前返回 false）----
  // machine 传 undefined 时省略该字段（单机模式与 v1 字节级一致）；
  // 多机模式必须传当前选中机器（防 relay 回 ambiguous_machine）。

  /** convId 传 undefined 时省略该字段（网关落默认会话）——「+ 新会话」的
   *  /switch 命令文本走默认会话，由网关解析并切出 web:<名字>。 */
  sendMessage(convId: string | undefined, text: string, machine?: string): boolean {
    return this.send({ v: 1, type: "message", convId, text, machine });
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

  /** hello 双形态（v1.2）：持有会话凭证 → session 形态；否则 token 形态
   *  （+可选 device）。undefined 字段不序列化，token 形态无 device 时与
   *  v1.1 字节级一致（旧 relay 兼容；device 对旧 relay 是被忽略的未知字段）。 */
  private sendHello(): void {
    this.rawSend(
      this.sessionToken
        ? encodeFrame({ v: 1, type: "hello", session: this.sessionToken })
        : encodeFrame({ v: 1, type: "hello", token: this.token, device: this.device }),
    );
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
    // v1.4：握手应答由 e2e 协调器消费（不进 onFrame）
    if (frame.type === "e2e_ack") {
      this.e2e.onAck(frame as E2eAckFrame);
      return;
    }
    // v1.4：enc 帧 → 解密后二次分发为原帧，走既有处理路径
    if (frame.type === "enc") {
      const inner = this.e2e.openEnc(frame as EncFrame);
      if (inner) {
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
    this.pingTimer = setInterval(() => {
      // 链路僵死检测：> deadLinkMs 未收到任何入站帧 → 主动断开重连
      if (Date.now() - this.lastFrameAt > this.deadLinkMs) {
        this.teardownSocket();
        this.scheduleReconnect();
        return;
      }
      // v1.4：当前机器 e2e active → ping 亦密文（其余恒明文，保命线不阻断）
      this.rawSend(
        this.e2e.heartbeat(
          { v: 1, type: "ping", machine: this.machineOf() },
          this.machineOf(),
        ),
      );
    }, this.pingIntervalMs);
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
    if (code === CLOSE_AUTH_FAILED) {
      // relay 认证失败：按 close reason 分流（session expired/revoked/其它），
      // 调用方清对应凭证回登录页；不重连
      this.stopped = true;
      this.onAuthFailed(authFailReason(typeof ev.reason === "string" ? ev.reason : undefined));
      return;
    }
    if (code === CLOSE_SUPERSEDED) {
      // 本连接被同 token 新窗口顶替：进入终态，不自动重连
      // （自动重连会顶替回去，两个窗口互踢死循环）；用户显式 connect() 才重连。
      this.stopped = true;
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
