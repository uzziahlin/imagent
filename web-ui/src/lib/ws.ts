// WS 连接管理（无 UI 依赖、可单测）：
// connect → 首帧 hello（token 只在此帧出现，绝不进 URL）→ 等 hello_ack
// （10s 超时视为失败）→ ready → 立即发 sync（含重连场景）→ 25s 应用层 ping
// （relay idle 90s 断连）→ 断线指数退避重连 1s→30s（成功重置）→ 收 close 1008
// 视为 token 无效（onAuthFailed，回登录页清 token，不再重连）→ 收 close 4001
// 视为本连接被同 token 新窗口顶替（进入 superseded 终态：停心跳停重连，否则
// 两窗口互踢死循环；用户显式 connect() 才重连，反之顶替对方是用户意图）→
// >60s 无任何入站帧视为链路僵死，主动断开重连。

import {
  encodeFrame,
  parseFrame,
  type ApprovalDecision,
  type InboundFrame,
  type OutboundFrame,
} from "./protocol";

export type ConnStatus = "connecting" | "ready" | "reconnecting" | "superseded";

/** 事件参数的最小形状（open/error 事件无载荷，message/close 各取所需字段）。 */
export type WsEvent = { data?: unknown; code?: number };

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
  token: string;
  /** 全部合法入站帧（已过 parseFrame 校验）。 */
  onFrame: (frame: InboundFrame) => void;
  onStatus: (status: ConnStatus) => void;
  /** close 1008：token 无效，调用方应清 token 回登录页。 */
  onAuthFailed: () => void;
  /** 注入 WebSocket 构造（单测用 fake）；缺省浏览器原生。 */
  wsFactory?: (url: string) => WebSocketLike;
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

export class WsManager {
  private readonly url: string;
  private readonly token: string;
  private readonly onFrame: (frame: InboundFrame) => void;
  private readonly onStatus: (status: ConnStatus) => void;
  private readonly onAuthFailed: () => void;
  private readonly wsFactory: (url: string) => WebSocketLike;
  private readonly helloTimeoutMs: number;
  private readonly pingIntervalMs: number;
  private readonly deadLinkMs: number;
  private readonly minBackoffMs: number;
  private readonly maxBackoffMs: number;

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

  constructor(opts: WsManagerOptions) {
    this.url = opts.url;
    this.token = opts.token;
    this.onFrame = opts.onFrame;
    this.onStatus = opts.onStatus;
    this.onAuthFailed = opts.onAuthFailed;
    this.wsFactory = opts.wsFactory ?? ((u) => new WebSocket(u) as unknown as WebSocketLike);
    this.helloTimeoutMs = opts.helloTimeoutMs ?? 10_000;
    this.pingIntervalMs = opts.pingIntervalMs ?? 25_000;
    this.deadLinkMs = opts.deadLinkMs ?? 60_000;
    this.minBackoffMs = opts.minBackoffMs ?? 1_000;
    this.maxBackoffMs = opts.maxBackoffMs ?? 30_000;
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

  /** convId 传 undefined 时省略该字段（网关落默认会话）——「+ 新会话」的
   *  /switch 命令文本走默认会话，由网关解析并切出 web:<名字>。 */
  sendMessage(convId: string | undefined, text: string): boolean {
    return this.send({ v: 1, type: "message", convId, text });
  }

  /** convId / requestId 原样回传 approval_request 帧里的值。 */
  sendApprovalResponse(convId: string, requestId: string, decision: ApprovalDecision): boolean {
    return this.send({ v: 1, type: "approval_response", convId, requestId, decision });
  }

  sendControl(cmd: "stop" | "resume" | "switch", args?: string): boolean {
    return this.send(
      args === undefined ? { v: 1, type: "control", cmd } : { v: 1, type: "control", cmd, args },
    );
  }

  private send(frame: OutboundFrame): boolean {
    if (!this.ready || !this.socket) {
      return false;
    }
    this.rawSend(encodeFrame(frame));
    return true;
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
    this.rawSend(encodeFrame({ v: 1, type: "hello", token: this.token }));
    this.helloTimer = setTimeout(() => {
      // 10s 无 hello_ack：按失败处理（先摘监听再关 socket，close 事件不再触发）
      this.teardownSocket();
      this.scheduleReconnect();
    }, this.helloTimeoutMs);
  };

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
      this.onHelloAck();
      return;
    }
    this.onFrame(frame);
  };

  private onHelloAck(): void {
    this.clearHelloTimer();
    this.ready = true;
    this.attempts = 0; // 成功即重置退避
    this.setStatus("ready");
    // 连接成功（含重连）后必须立即发 sync：网关重发全部会话最近卡片终态
    this.rawSend(encodeFrame({ v: 1, type: "sync" }));
    this.lastFrameAt = Date.now();
    this.startHeartbeat();
  };

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.pingTimer = setInterval(() => {
      // 链路僵死检测：> deadLinkMs 未收到任何入站帧 → 主动断开重连
      if (Date.now() - this.lastFrameAt > this.deadLinkMs) {
        this.teardownSocket();
        this.scheduleReconnect();
        return;
      }
      this.rawSend(encodeFrame({ v: 1, type: "ping" }));
    }, this.pingIntervalMs);
  }

  private handleClose = (ev: WsEvent): void => {
    this.stopTimers();
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
      // relay 认证失败：token 无效 → 回登录页，不重连
      this.stopped = true;
      this.onAuthFailed();
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

  /** 摘监听并关 socket（不发 close 事件，重连由调用方决定）。 */
  private teardownSocket(): void {
    this.stopTimers();
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
