import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, hexToBytes, randomBytes, utf8ToBytes } from "@noble/hashes/utils.js";
import {
  authFailReason,
  clearSession,
  loadSession,
  saveSession,
  WsManager,
  type AuthFailReason,
  type WsEvent,
  type WebSocketLike,
} from "./ws";
import { openBox, sealFrame, signatureMessage, type E2eEvent } from "./e2e";
import type { InboundFrame, SessionInfo } from "./protocol";

/** 可控 fake WebSocket：手动 emit 事件、记录出站帧。 */
class FakeWebSocket implements WebSocketLike {
  static created: FakeWebSocket[] = [];
  readonly url: string;
  readyState = 0;
  sent: string[] = [];
  closedWith: (number | undefined)[] = [];
  private listeners = new Map<string, Set<(ev: WsEvent) => void>>();

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.created.push(this);
  }

  addEventListener(type: string, listener: (ev: WsEvent) => void): void {
    const set = this.listeners.get(type) ?? new Set();
    set.add(listener);
    this.listeners.set(type, set);
  }

  removeEventListener(type: string, listener: (ev: WsEvent) => void): void {
    this.listeners.get(type)?.delete(listener);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(code?: number): void {
    this.readyState = 3;
    this.closedWith.push(code);
  }

  emit(type: "open" | "close" | "error", ev: WsEvent = {}): void {
    if (type === "open") {
      this.readyState = 1;
    }
    for (const fn of this.listeners.get(type) ?? []) {
      fn(ev);
    }
  }

  /** 服务端下发一条文本帧。 */
  serverSend(raw: string): void {
    for (const fn of this.listeners.get("message") ?? []) {
      fn({ data: raw });
    }
  }

  sentOfType(t: string): string[] {
    return this.sent.filter((s) => (JSON.parse(s) as { type: string }).type === t);
  }
}

function lastSock(): FakeWebSocket {
  const s = FakeWebSocket.created.at(-1);
  if (!s) {
    throw new Error("尚无 socket 创建");
  }
  return s;
}

const HELLO_ACK = '{"v":1,"type":"hello_ack","role":"browser"}';

interface Harness {
  mgr: WsManager;
  frames: InboundFrame[];
  statuses: string[];
  authFailed: number;
}

function makeManager(token = "tok-1", getMachine?: () => string | undefined): Harness {
  const frames: InboundFrame[] = [];
  const statuses: string[] = [];
  let authFailed = 0;
  const mgr = new WsManager({
    url: "wss://example.test/imagent/ws",
    token,
    onFrame: (f) => frames.push(f),
    onStatus: (s) => statuses.push(s),
    onAuthFailed: () => {
      authFailed += 1;
    },
    wsFactory: (u) => new FakeWebSocket(u),
    getMachine,
  });
  return { mgr, frames, statuses, get authFailed() { return authFailed; } };
}

/** open → hello → hello_ack → ready 的标准就绪流。 */
function readyFlow(h: Pick<Harness, "mgr" | "statuses">): FakeWebSocket {
  const sock = lastSock();
  sock.emit("open");
  sock.serverSend(HELLO_ACK);
  expect(h.statuses.at(-1)).toBe("ready");
  return sock;
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeWebSocket.created = [];
});

afterEach(() => {
  vi.useRealTimers();
});

describe("WsManager", () => {
  it("open 后首帧是 hello 且 token 正确", () => {
    const h = makeManager("secret-token");
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    expect(sock.sent[0]).toBe('{"v":1,"type":"hello","token":"secret-token"}');
  });

  it("hello_ack 后发 sync 进入 ready；业务帧转发给 onFrame", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    expect(sock.sent[1]).toBe('{"v":1,"type":"sync"}');
    sock.serverSend('{"v":1,"type":"text","convId":"web:default","text":"hi"}');
    expect(h.frames).toHaveLength(1);
    expect(h.frames[0]).toMatchObject({ type: "text", text: "hi" });
    // 畸形帧被吞掉不抛
    sock.serverSend("garbage");
    expect(h.frames).toHaveLength(1);
  });

  it("25s 应用层 ping，pong 刷新链路时钟", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    vi.advanceTimersByTime(25_000);
    expect(sock.sentOfType("ping")).toHaveLength(1);
    sock.serverSend('{"v":1,"type":"pong"}');
    vi.advanceTimersByTime(25_000);
    vi.advanceTimersByTime(25_000);
    expect(sock.sentOfType("ping")).toHaveLength(3);
  });

  it("close 1008 触发 onAuthFailed 且不再重连", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    sock.emit("close", { code: 1008 });
    expect(h.authFailed).toBe(1);
    vi.advanceTimersByTime(120_000);
    expect(FakeWebSocket.created).toHaveLength(1);
  });

  it("close 4001 进入 superseded 终态：不重连、心跳停止、不触发 onAuthFailed", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    // 先让心跳跑起来（25s 一个 ping），确认 close 4001 后心跳真的停了
    vi.advanceTimersByTime(25_000);
    expect(sock.sentOfType("ping")).toHaveLength(1);
    sock.emit("close", { code: 4001 });
    expect(h.statuses.at(-1)).toBe("superseded");
    expect(h.authFailed).toBe(0);
    // 心跳停止：此后 50s（两个 ping 周期）无新 ping
    vi.advanceTimersByTime(50_000);
    expect(sock.sentOfType("ping")).toHaveLength(1);
    // 不自动重连：退避定时器不调度，120s 后仍只有初始一个 socket
    vi.advanceTimersByTime(120_000);
    expect(FakeWebSocket.created).toHaveLength(1);
    expect(h.statuses.at(-1)).toBe("superseded");
  });

  it("superseded 后 connect() 显式重连：重发 hello 可再 ready（顶替对方）", () => {
    const h = makeManager();
    h.mgr.connect();
    const s1 = readyFlow(h);
    s1.emit("close", { code: 4001 });
    expect(h.statuses.at(-1)).toBe("superseded");
    // 用户点击「重新连接」
    h.mgr.connect();
    expect(FakeWebSocket.created).toHaveLength(2);
    expect(h.statuses.at(-1)).toBe("connecting");
    const s2 = lastSock();
    s2.emit("open");
    expect(s2.sent[0]).toContain('"hello"');
    s2.serverSend(HELLO_ACK);
    expect(h.statuses.at(-1)).toBe("ready");
  });

  it("普通 close 指数退避重连：1s 起、翻倍、成功后重置", () => {
    const h = makeManager();
    h.mgr.connect();
    const s1 = readyFlow(h);
    s1.emit("close", { code: 1006 });
    expect(h.statuses.at(-1)).toBe("reconnecting");

    // 第一次退避 1s
    vi.advanceTimersByTime(999);
    expect(FakeWebSocket.created).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.created).toHaveLength(2);

    // 第二次退避 2s
    const s2 = lastSock();
    s2.emit("open");
    s2.emit("close", { code: 1006 });
    vi.advanceTimersByTime(1999);
    expect(FakeWebSocket.created).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.created).toHaveLength(3);

    // 重连成功：hello + sync 都要再发（含重连场景的 sync 纪律）
    const s3 = lastSock();
    s3.emit("open");
    expect(s3.sent[0]).toContain('"hello"');
    s3.serverSend(HELLO_ACK);
    expect(s3.sentOfType("sync")).toHaveLength(1);

    // 成功后退避重置：再断开 → 又是 1s
    s3.emit("close", { code: 1006 });
    vi.advanceTimersByTime(1000);
    expect(FakeWebSocket.created).toHaveLength(4);
  });

  it("退避封顶 30s", () => {
    const h = makeManager();
    h.mgr.connect();
    // 连续 6 次失败（hello 超时计一次）：退避 1,2,4,8,16,30s——第 6 次起封顶
    for (let i = 0; i < 6; i++) {
      const sock = lastSock();
      sock.emit("open");
      vi.advanceTimersByTime(10_000); // hello 10s 超时 → 调度下一轮
      vi.advanceTimersByTime(30_000); // 让退避定时器到点，新 socket 就位
    }
    expect(FakeWebSocket.created).toHaveLength(7); // 初始 1 + 重连 6
    // 第 7 次失败：2^6=64s → 封顶 30s
    const sock = lastSock();
    sock.emit("open");
    vi.advanceTimersByTime(10_000); // 触发 hello 超时
    vi.advanceTimersByTime(29_999);
    expect(FakeWebSocket.created).toHaveLength(7); // 30s 未到
    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.created).toHaveLength(8);
    expect(h.statuses).toContain("reconnecting");
  });

  it("hello_ack 10s 超时视为失败并重连", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    vi.advanceTimersByTime(9_999);
    expect(FakeWebSocket.created).toHaveLength(1);
    vi.advanceTimersByTime(1); // t=10s：hello 超时 → 主动关 socket + 调度 1s 退避
    expect(sock.closedWith).toContain(1000);
    expect(FakeWebSocket.created).toHaveLength(1);
    vi.advanceTimersByTime(1000); // t=11s：退避到点重连
    expect(FakeWebSocket.created).toHaveLength(2);
    expect(h.statuses.at(-1)).toBe("reconnecting");
  });

  it("60s 无任何入站帧 → 主动断开重连", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    // 只有出站 ping、没有 pong：75s 时超过 60s 僵死线
    vi.advanceTimersByTime(50_000);
    expect(FakeWebSocket.created).toHaveLength(1);
    vi.advanceTimersByTime(25_000);
    // 僵死断开（主动 close 1000）+ 1s 退避后重连
    expect(sock.closedWith).toContain(1000);
    vi.advanceTimersByTime(1000);
    expect(FakeWebSocket.created).toHaveLength(2);
  });

  it("未 ready 时发送返回 false；ready 后 message/approval/control 可发", () => {
    const h = makeManager();
    h.mgr.connect();
    expect(h.mgr.sendMessage("web:default", "hi")).toBe(false);
    expect(h.mgr.sendControl("stop")).toBe(false);
    const sock = readyFlow(h);
    expect(h.mgr.sendMessage("web:default", "hi")).toBe(true);
    // approval_response 原样回传 convId/requestId
    expect(h.mgr.sendApprovalResponse("web:work", "req-9", "always")).toBe(true);
    expect(h.mgr.sendControl("stop")).toBe(true);
    expect(sock.sent).toContain('{"v":1,"type":"message","convId":"web:default","text":"hi"}');
    expect(sock.sent).toContain(
      '{"v":1,"type":"approval_response","convId":"web:work","requestId":"req-9","decision":"always"}',
    );
    expect(sock.sent).toContain('{"v":1,"type":"control","cmd":"stop"}');
  });

  it("sendMessage convId 传 undefined 时省略 convId（/switch 命令落网关默认会话）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    expect(h.mgr.sendMessage(undefined, "/switch work")).toBe(true);
    expect(sock.sent).toContain('{"v":1,"type":"message","text":"/switch work"}');
  });

  // v1.7 媒体上行（契约 A 段）：media 可选参数经既有 message 出站路径（E2E
  // BUSINESS_TYPES 已含 message，随帧自动加密），字段名逐字一致。
  it("sendMessage 可选 media：帧带 media{name,mime,b64}；纯图消息 text 空串；无 media 字节级不变", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    const media = { name: "shot.jpg", mime: "image/jpeg", b64: "AAAA" };
    expect(h.mgr.sendMessage("web:default", "看图", "home-mac", media)).toBe(true);
    expect(sock.sent).toContain(
      '{"v":1,"type":"message","convId":"web:default","text":"看图","machine":"home-mac","media":{"name":"shot.jpg","mime":"image/jpeg","b64":"AAAA"}}',
    );
    // 纯图消息：text 传空串（契约：text 与 media 至少其一非空）
    expect(h.mgr.sendMessage("web:default", "", "home-mac", media)).toBe(true);
    expect(sock.sent).toContain(
      '{"v":1,"type":"message","convId":"web:default","text":"","machine":"home-mac","media":{"name":"shot.jpg","mime":"image/jpeg","b64":"AAAA"}}',
    );
    // 不带 media：帧与 v1.6 字节级一致（undefined 不序列化）
    expect(h.mgr.sendMessage("web:default", "hi", "home-mac")).toBe(true);
    expect(sock.sent).toContain(
      '{"v":1,"type":"message","convId":"web:default","text":"hi","machine":"home-mac"}',
    );
  });

  // local:xxx 是网关侧本地终端会话的既有 conv 形态（MultiPlatform 镜像路由把
  // local 会话帧同步给浏览器）。SPA 对 local 会话的干预消息/审批回执必须原样
  // 携带 local: convId——被改写（如强加 web: 前缀）则网关侧路由进幽灵会话，
  // 干预与审批闭环断裂。
  it("local:xxx convId 原样出站不改写（SPA 直接寻址 local 终端会话）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    expect(h.mgr.sendMessage("local:work", "继续", "home-mac")).toBe(true);
    expect(h.mgr.sendApprovalResponse("local:work", "req-1", "allow", "home-mac")).toBe(true);
    expect(sock.sent).toContain(
      '{"v":1,"type":"message","convId":"local:work","text":"继续","machine":"home-mac"}',
    );
    expect(sock.sent).toContain(
      '{"v":1,"type":"approval_response","convId":"local:work","requestId":"req-1","decision":"allow","machine":"home-mac"}',
    );
  });

  it("手动 close 后不再重连", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    h.mgr.close();
    expect(sock.closedWith).toContain(1000);
    sock.emit("close", { code: 1006 }); // 模拟 close 事件到达
    vi.advanceTimersByTime(120_000);
    expect(FakeWebSocket.created).toHaveLength(1);
  });
});

describe("WsManager 多机（v1.1）", () => {
  it("send 系列带 machine 参数：帧携带该字段", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    expect(h.mgr.sendMessage("web:default", "hi", "home-mac")).toBe(true);
    expect(h.mgr.sendApprovalResponse("web:work", "req-9", "always", "work-mac")).toBe(true);
    expect(h.mgr.sendControl("stop", undefined, "home-mac")).toBe(true);
    expect(h.mgr.sendSync("home-mac")).toBe(true);
    expect(sock.sent).toContain(
      '{"v":1,"type":"message","convId":"web:default","text":"hi","machine":"home-mac"}',
    );
    expect(sock.sent).toContain(
      '{"v":1,"type":"approval_response","convId":"web:work","requestId":"req-9","decision":"always","machine":"work-mac"}',
    );
    expect(sock.sent).toContain('{"v":1,"type":"control","cmd":"stop","machine":"home-mac"}');
    expect(sock.sent).toContain('{"v":1,"type":"sync","machine":"home-mac"}');
  });

  it("send 系列不传 machine：帧不带该字段（旧 relay/旧网关字节级兼容）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    expect(h.mgr.sendMessage("web:default", "hi")).toBe(true);
    expect(h.mgr.sendApprovalResponse("web:work", "req-9", "always")).toBe(true);
    expect(h.mgr.sendControl("stop")).toBe(true);
    expect(h.mgr.sendSync()).toBe(true);
    expect(sock.sent).toContain('{"v":1,"type":"message","convId":"web:default","text":"hi"}');
    expect(sock.sent).toContain(
      '{"v":1,"type":"approval_response","convId":"web:work","requestId":"req-9","decision":"always"}',
    );
    expect(sock.sent).toContain('{"v":1,"type":"control","cmd":"stop"}');
    expect(sock.sent).toContain('{"v":1,"type":"sync"}');
  });

  it("sendSync 未 ready 返回 false", () => {
    const h = makeManager();
    h.mgr.connect();
    expect(h.mgr.sendSync("home-mac")).toBe(false);
  });

  it("getMachine 注入：hello_ack 后自动 sync 与周期 ping 均带当前机器", () => {
    const h = makeManager("tok-1", () => "home-mac");
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    expect(h.statuses.at(-1)).toBe("ready");
    expect(sock.sent[1]).toBe('{"v":1,"type":"sync","machine":"home-mac"}');
    vi.advanceTimersByTime(25_000);
    expect(sock.sent).toContain('{"v":1,"type":"ping","machine":"home-mac"}');
  });

  it("getMachine 返回 undefined：自动 sync/ping 不带 machine（单机兼容路径）", () => {
    const h = makeManager("tok-1", () => undefined);
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    expect(sock.sent[1]).toBe('{"v":1,"type":"sync"}');
    vi.advanceTimersByTime(25_000);
    expect(sock.sent).toContain('{"v":1,"type":"ping"}');
  });

  it("getMachine 取值随发送时点变化（机器切换后 ping 带新机器）", () => {
    let machine: string | undefined = undefined; // 首连尚未收到 machines 帧
    const h = makeManager("tok-1", () => machine);
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    expect(sock.sent[1]).toBe('{"v":1,"type":"sync"}');
    machine = "work-mac"; // machines 帧到达 → App 更新当前机器
    vi.advanceTimersByTime(25_000);
    expect(sock.sent).toContain('{"v":1,"type":"ping","machine":"work-mac"}');
  });
});

describe("WsManager 会话层（v1.2）", () => {
  const SESSION: SessionInfo = { id: "sid-1", token: "st-1", expiresAt: 1_900_000_000_000 };
  const ACK_WITH_SESSION = JSON.stringify({
    v: 1,
    type: "hello_ack",
    role: "browser",
    session: SESSION,
  });

  interface SessHarness {
    mgr: WsManager;
    statuses: string[];
    authReasons: AuthFailReason[];
    sessions: SessionInfo[];
  }

  function makeSessManager(opts: { token?: string; session?: string; device?: string }): SessHarness {
    const statuses: string[] = [];
    const authReasons: AuthFailReason[] = [];
    const sessions: SessionInfo[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      token: opts.token,
      session: opts.session,
      device: opts.device,
      onFrame: () => {},
      onStatus: (s) => statuses.push(s),
      onAuthFailed: (r) => authReasons.push(r),
      onSession: (s) => sessions.push(s),
      wsFactory: (u) => new FakeWebSocket(u),
    });
    return { mgr, statuses, authReasons, sessions };
  }

  it("首次登录 hello 带 token+device；hello_ack 携带 session → onSession 上抛", () => {
    const h = makeSessManager({ token: "tok-9", device: "Mac/Chrome" });
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    expect(sock.sent[0]).toBe('{"v":1,"type":"hello","token":"tok-9","device":"Mac/Chrome"}');
    sock.serverSend(ACK_WITH_SESSION);
    expect(h.sessions).toEqual([SESSION]);
    expect(h.statuses.at(-1)).toBe("ready");
  });

  it("签发 session 后断线重连：hello 改用 session 形态（token 不再出现）", () => {
    const h = makeSessManager({ token: "tok-9", device: "Mac/Chrome" });
    h.mgr.connect();
    const s1 = lastSock();
    s1.emit("open");
    s1.serverSend(ACK_WITH_SESSION);
    s1.emit("close", { code: 1006 });
    vi.advanceTimersByTime(1_000); // 退避 1s 到点重连
    const s2 = lastSock();
    s2.emit("open");
    expect(s2.sent[0]).toBe('{"v":1,"type":"hello","session":"st-1"}');
    s2.serverSend(ACK_WITH_SESSION);
    expect(h.statuses.at(-1)).toBe("ready");
  });

  it("构造注入 session（localStorage 恢复）：首帧 hello 即 session 形态", () => {
    const h = makeSessManager({ session: "st-2" });
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    expect(sock.sent[0]).toBe('{"v":1,"type":"hello","session":"st-2"}');
    // 重连 hello_ack 回剩余有效期 session → 再次上抛（App 刷新本地过期时间）
    sock.serverSend(
      JSON.stringify({
        v: 1,
        type: "hello_ack",
        role: "browser",
        session: { id: "sid-2", token: "st-2", expiresAt: 1_950_000_000_000 },
      }),
    );
    expect(h.sessions).toHaveLength(1);
    expect(h.sessions[0].expiresAt).toBe(1_950_000_000_000);
  });

  it("旧 relay 兼容：hello_ack 无 session → onSession 不触发，重连仍是 token 形态（字节级回退）", () => {
    const h = makeSessManager({ token: "tok-9" });
    h.mgr.connect();
    const s1 = lastSock();
    s1.emit("open");
    expect(s1.sent[0]).toBe('{"v":1,"type":"hello","token":"tok-9"}');
    s1.serverSend(HELLO_ACK); // 旧 relay：无 session 字段
    expect(h.sessions).toHaveLength(0);
    s1.emit("close", { code: 1006 });
    vi.advanceTimersByTime(1_000);
    const s2 = lastSock();
    s2.emit("open");
    expect(s2.sent[0]).toBe('{"v":1,"type":"hello","token":"tok-9"}');
  });

  it("close 1008 按 reason 分流：expired / revoked / 其它（含缺席）→ auth_failed", () => {
    const h = makeSessManager({ session: "st-1" });
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend(ACK_WITH_SESSION);
    sock.emit("close", { code: 1008, reason: "session expired" });
    expect(h.authReasons).toEqual(["session_expired"]);
    // 不再重连
    vi.advanceTimersByTime(120_000);
    expect(FakeWebSocket.created).toHaveLength(1);

    const h2 = makeSessManager({ session: "st-1" });
    h2.mgr.connect();
    lastSock().emit("open");
    lastSock().serverSend(ACK_WITH_SESSION);
    lastSock().emit("close", { code: 1008, reason: "session revoked" });
    expect(h2.authReasons).toEqual(["session_revoked"]);

    const h3 = makeSessManager({ token: "tok-9" });
    h3.mgr.connect();
    lastSock().emit("open");
    lastSock().serverSend(HELLO_ACK);
    // 缺席 reason 与协议外 reason 均归 auth_failed
    lastSock().emit("close", { code: 1008 });
    expect(h3.authReasons).toEqual(["auth_failed"]);
  });

  it("authFailReason 纯函数：三种协议 reason 精确映射，其余归 auth_failed", () => {
    expect(authFailReason("session expired")).toBe("session_expired");
    expect(authFailReason("session revoked")).toBe("session_revoked");
    expect(authFailReason("auth failed")).toBe("auth_failed");
    expect(authFailReason(undefined)).toBe("auth_failed");
    expect(authFailReason("Session Expired")).toBe("auth_failed"); // 大小写敏感
  });

  it("sendDeviceList / sendDeviceRevoke：ready 前返回 false，ready 后字节精确", () => {
    const h = makeSessManager({ session: "st-1" });
    h.mgr.connect();
    expect(h.mgr.sendDeviceList()).toBe(false);
    expect(h.mgr.sendDeviceRevoke("d1")).toBe(false);
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend(ACK_WITH_SESSION);
    expect(h.mgr.sendDeviceList()).toBe(true);
    expect(h.mgr.sendDeviceRevoke("d1")).toBe(true);
    expect(sock.sent).toContain('{"v":1,"type":"device_list"}');
    expect(sock.sent).toContain('{"v":1,"type":"device_revoke","deviceId":"d1"}');
  });

  it("devices 帧经 onFrame 转发（设备列表归 reducer 消费）", () => {
    const frames: InboundFrame[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      session: "st-1",
      onFrame: (f) => frames.push(f),
      onStatus: () => {},
      onAuthFailed: () => {},
      wsFactory: (u) => new FakeWebSocket(u),
    });
    mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend(ACK_WITH_SESSION);
    sock.serverSend(
      '{"v":1,"type":"devices","devices":[{"id":"d1","name":"Mac/Chrome","connected":true}]}',
    );
    expect(frames).toHaveLength(1);
    expect(frames[0]).toMatchObject({ type: "devices" });
  });
});

describe("会话凭证存储（v1.2）", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("saveSession / loadSession 往返（含 id 字段保留）", () => {
    saveSession({ id: "sid-1", token: "st-1", expiresAt: Date.now() + 20 * 86_400_000 });
    expect(loadSession()).toEqual({ id: "sid-1", token: "st-1", expiresAt: expect.any(Number) });
    expect(loadSession()?.id).toBe("sid-1");
  });

  it("loadSession：无存储 / 畸形 JSON / 形状非法 → null 并清理", () => {
    expect(loadSession()).toBeNull();
    localStorage.setItem("imagent.session", "not-json");
    expect(loadSession()).toBeNull();
    expect(localStorage.getItem("imagent.session")).toBeNull();
    localStorage.setItem("imagent.session", JSON.stringify({ token: "st", expiresAt: "soon" }));
    expect(loadSession()).toBeNull();
    localStorage.setItem("imagent.session", JSON.stringify({ token: "", expiresAt: 1 }));
    expect(loadSession()).toBeNull();
  });

  it("loadSession：已过期 → null 并清理（秒级时间戳同样判定）", () => {
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "s", token: "st", expiresAt: Date.now() - 1_000 }),
    );
    expect(loadSession()).toBeNull();
    // 秒级：normalizeExpiryMs 放大后再判
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "s", token: "st", expiresAt: Math.floor(Date.now() / 1000) - 10 }),
    );
    expect(loadSession()).toBeNull();
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("clearSession 清除存储", () => {
    saveSession({ token: "st", expiresAt: Date.now() + 86_400_000 });
    clearSession();
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// v1.4 E2E：machines 帧驱动逐机器握手 / 业务帧 enc 包装 / 入站解密二次分发 /
// fail-closed（超时禁发 + 显式明文）/ 身份告警 / 断线重连清会话
// ---------------------------------------------------------------------------

describe("WsManager E2E（v1.4）", () => {
  /** 测试网关：长期 X25519 + ed25519 身份密钥（与 e2e.test 同构）。 */
  function makeGateway() {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    return {
      gk,
      ik,
      ackFor: (hello: { ek: string; n1: string }, machine: string, opts?: { gk?: string; ik?: string }) => {
        const gkUsed = opts?.gk ?? gk;
        const n2 = bytesToHex(randomBytes(32));
        const sig = bytesToHex(
          ed25519.sign(signatureMessage(hello.ek, gkUsed, hello.n1, n2), idPriv),
        );
        return { v: 1, type: "e2e_ack", machine, gk: gkUsed, n2, sig, ik: opts?.ik ?? ik };
      },
      gatewayKey: (hello: { ek: string; n1: string }, n2: string) => {
        const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
        const ikm = new Uint8Array(96);
        ikm.set(shared, 0);
        ikm.set(hexToBytes(hello.n1), 32);
        ikm.set(hexToBytes(n2), 64);
        return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
      },
    };
  }

  interface E2eHarness {
    mgr: WsManager;
    frames: InboundFrame[];
    events: E2eEvent[];
    statuses: string[];
  }

  /** E2E harness。machine 传字符串 = 当前机器固定；传函数 = 每次发送时取值
   *  （模拟侧栏切机 / 掉线自动切换后的当前机器变化）。 */
  function makeE2eManager(
    machine: string | (() => string | undefined),
    extra?: { sessionProbeMs?: number },
  ): E2eHarness {
    const frames: InboundFrame[] = [];
    const events: E2eEvent[] = [];
    const statuses: string[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      token: "tok-1",
      onFrame: (f) => frames.push(f),
      onStatus: (s) => statuses.push(s),
      onAuthFailed: () => {},
      onE2e: (e) => events.push(e),
      sessionProbeMs: extra?.sessionProbeMs,
      wsFactory: (u) => new FakeWebSocket(u),
      getMachine: () => (typeof machine === "function" ? machine() : machine),
    });
    return { mgr, frames, events, statuses };
  }

  /** ready 后推 machines 快照（触发 e2e 握手）。 */
  function pushMachines(sock: FakeWebSocket, online: string[]): void {
    sock.serverSend(JSON.stringify({ v: 1, type: "machines", online }));
  }

  /** 测试网关应答某机器**最新**一帧 e2e_hello（ack 经 socket 送入）。返回
   *  该帧握手派生的网关侧会话密钥。 */
  function ackLatest(sock: FakeWebSocket, gw: ReturnType<typeof makeGateway>, machine: string) {
    const hello = sock
      .sentOfType("e2e_hello")
      .map((s) => JSON.parse(s) as { machine: string; ek: string; n1: string })
      .filter((h) => h.machine === machine)
      .at(-1)!;
    const ack = gw.ackFor(hello, machine);
    sock.serverSend(JSON.stringify(ack));
    return { hello, ack, key: gw.gatewayKey(hello, ack.n2) };
  }

  /** v1.41.9 TOFU 确认并走完重新握手：resolveTofu(accept) → 对确认后新发的
   *  e2e_hello 再喂一帧 ack → active。返回网关侧会话密钥（由第二帧握手
   *  派生——弹层期间那把已丢弃，不再使用）。 */
  function acceptTofu(sock: FakeWebSocket, gw: ReturnType<typeof makeGateway>, h: E2eHarness, machine: string) {
    h.mgr.resolveTofu(machine, true);
    return ackLatest(sock, gw, machine);
  }

  /** 把某机器带到 active：首连 ack（未 pin → TOFU）→ 确认 → 重新握手 →
   *  第二帧 ack。返回网关侧会话密钥。 */
  function toActive(sock: FakeWebSocket, gw: ReturnType<typeof makeGateway>, h: E2eHarness, machine: string): Uint8Array {
    ackLatest(sock, gw, machine);
    return acceptTofu(sock, gw, h, machine).key;
  }

  /** 把若干机器逐台带到 active（须先推过含这些机器的 machines 快照）。
   *  返回各机器网关侧会话密钥。 */
  function toActiveAll(
    sock: FakeWebSocket,
    gw: ReturnType<typeof makeGateway>,
    h: E2eHarness,
    machines: string[],
  ): Record<string, Uint8Array> {
    const keys: Record<string, Uint8Array> = {};
    for (const m of machines) {
      keys[m] = toActive(sock, gw, h, m);
    }
    return keys;
  }

  /** 网关侧会话仍有效时的 pong 回包（经 send_business 逐 sid 加密）。 */
  const gatewayPong = (machine: string, key: Uint8Array): string =>
    JSON.stringify(sealFrame(machine, key, { v: 1, type: "pong" }));

  /** 应答 socket 上**当前已发出**的全部 enc ping（逐机器用各自的网关密钥
   *  解出内帧 machine 再回 pong）——让在途的会话探活落定，避免 3s 后重连
   *  干扰后续心跳断言。 */
  function answerEncPings(sock: FakeWebSocket, keys: Record<string, Uint8Array>): void {
    for (const raw of sock.sentOfType("enc")) {
      const enc = JSON.parse(raw) as { machine: string; box: string };
      const plain = openBox(keys[enc.machine], enc.box);
      if (plain !== null && (JSON.parse(plain) as { type: string }).type === "ping") {
        sock.serverSend(gatewayPong(enc.machine, keys[enc.machine]));
      }
    }
  }

  /** socket 上的 enc 帧按 machine 归并解密：{ machine → 内帧 JSON 文本 }
   *  （各机器用**自己的**网关密钥；用错密钥会解不开，记为占位串以便断言
   *  报错时可读）。 */
  function openEncByMachine(
    sock: FakeWebSocket,
    keys: Record<string, Uint8Array>,
  ): Record<string, string> {
    const out: Record<string, string> = {};
    for (const raw of sock.sentOfType("enc")) {
      const enc = JSON.parse(raw) as { machine: string; box: string };
      out[enc.machine] = openBox(keys[enc.machine], enc.box) ?? "<用该机器的密钥解不开>";
    }
    return out;
  }

  beforeEach(() => {
    localStorage.clear();
  });

  it("machines 帧触发逐机器 e2e_hello；TOFU 确认后出站 enc 包装、入站 enc 解密二次分发", () => {
    const gw = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    pushMachines(sock, ["home-mac"]);
    expect(sock.sentOfType("e2e_hello")).toHaveLength(1);
    const hello = JSON.parse(sock.sentOfType("e2e_hello")[0]) as { ek: string; n1: string };

    // ack → TOFU 事件
    const ack = gw.ackFor(hello, "home-mac");
    sock.serverSend(JSON.stringify(ack));
    expect(h.events).toContainEqual({
      kind: "tofu",
      machine: "home-mac",
      gkFp: expect.any(String),
      idFp: expect.any(String),
    });
    // confirming 态：待用户决断，发送被阻
    expect(h.mgr.sendMessage("web:default", "提前发", "home-mac")).toBe(false);
    // v1.41.9：确认 → 以 pin 身份重新握手。在途期间发送仍被阻——线上不出
    // 任何明文业务帧（用户刚点了「信任并启用加密」，不留明文窗口）
    h.mgr.resolveTofu("home-mac", true);
    expect(sock.sentOfType("e2e_hello")).toHaveLength(2);
    expect(h.mgr.sendMessage("web:default", "握手在途", "home-mac")).toBe(false);
    expect(sock.sent.filter((s) => s.includes("握手在途"))).toHaveLength(0);
    // 第二帧 ack → active
    const { hello: hello2, ack: ack2 } = ackLatest(sock, gw, "home-mac");
    expect(h.events).toContainEqual({ kind: "active", machine: "home-mac" });

    // 出站：enc 包装，网关密钥可解回原帧
    expect(h.mgr.sendMessage("web:default", "机密", "home-mac")).toBe(true);
    const encSent = sock.sentOfType("enc");
    expect(encSent).toHaveLength(1);
    const enc = JSON.parse(encSent[0]) as { machine: string; box: string };
    expect(enc.machine).toBe("home-mac");
    const gwKey = gw.gatewayKey(hello2, ack2.n2);
    expect(openBox(gwKey, enc.box)).toBe(
      JSON.stringify({ v: 1, type: "message", convId: "web:default", text: "机密", machine: "home-mac" }),
    );

    // 入站：网关加密的业务帧 → 解密后二次分发为原帧
    sock.serverSend(
      JSON.stringify(
        sealFrame("home-mac", gwKey, {
          v: 1,
          type: "text",
          convId: "web:default",
          text: "加密回复",
          machine: "home-mac",
        }),
      ),
    );
    expect(h.frames).toContainEqual(
      expect.objectContaining({ type: "text", text: "加密回复", machine: "home-mac" }),
    );
  });

  it("enc 解密内帧漏带 machine：注入 enc 信封 machine（归属兜底）；内帧自带不覆盖", () => {
    const gw = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    pushMachines(sock, ["home-mac"]);
    const hello = JSON.parse(sock.sentOfType("e2e_hello")[0]) as { ek: string; n1: string };
    sock.serverSend(JSON.stringify(gw.ackFor(hello, "home-mac")));
    // v1.41.9：确认 → 重新握手 → 第二帧 ack → active
    const { key: gwKey } = acceptTofu(sock, gw, h, "home-mac");
    expect(h.events).toContainEqual({ kind: "active", machine: "home-mac" });

    // 内帧无 machine（网关新帧型忘带的形态，如 v1.6 workspaces 初版）→
    // onFrame 收到的内帧带上 enc 信封的来源 machine（reducer 归属正确）。
    sock.serverSend(
      JSON.stringify(
        sealFrame("home-mac", gwKey, {
          v: 1,
          type: "workspaces",
          default: "/def",
          workspaces: [],
          convs: {},
        }),
      ),
    );
    expect(h.frames).toContainEqual(
      expect.objectContaining({ type: "workspaces", machine: "home-mac" }),
    );

    // 内帧自带 machine → 保留自报值，不被信封 machine 覆盖。
    sock.serverSend(
      JSON.stringify(
        sealFrame("home-mac", gwKey, {
          v: 1,
          type: "conv_workdir",
          convId: "web:work",
          workdir: "/w",
          machine: "work-mac",
        }),
      ),
    );
    expect(h.frames).toContainEqual(
      expect.objectContaining({ type: "conv_workdir", machine: "work-mac" }),
    );
  });

  it("旧网关 10s 无 ack → unsupported（发送禁用，明文入站照收）；仍以明文连接 → 放行并持久", () => {
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    pushMachines(sock, ["home-mac"]);
    // 窗口期内明文发送照发（v1.1 行为）
    expect(h.mgr.sendMessage("web:default", "窗口期", "home-mac")).toBe(true);
    expect(JSON.parse(sock.sent.at(-1) ?? "{}")).toMatchObject({ type: "message", text: "窗口期" });

    vi.advanceTimersByTime(10_000);
    expect(h.events).toContainEqual({ kind: "unsupported", machine: "home-mac", reason: "timeout" });
    // fail-closed：不自动明文降级，发送禁用
    expect(h.mgr.sendMessage("web:default", "x", "home-mac")).toBe(false);
    // 旧网关的明文业务帧照收（不支持加密 ≠ 拒收）
    sock.serverSend('{"v":1,"type":"text","convId":"web:default","text":"旧网关回复","machine":"home-mac"}');
    expect(h.frames).toContainEqual(expect.objectContaining({ text: "旧网关回复" }));

    // 显式按钮放行：明文发送 + 选择持久
    h.mgr.allowPlaintextE2e("home-mac");
    expect(h.mgr.sendMessage("web:default", "明文重试", "home-mac")).toBe(true);
    expect(JSON.parse(sock.sent.at(-1) ?? "{}")).toMatchObject({ type: "message", text: "明文重试" });
    expect(JSON.parse(localStorage.getItem("imagent.e2ePlaintext") ?? "{}")).toEqual({
      "home-mac": true,
    });
  });

  it("active 机器的明文业务帧丢弃（注入面）；无会话机器的 enc 帧丢弃", () => {
    const gw = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    pushMachines(sock, ["home-mac"]);
    const hello = JSON.parse(sock.sentOfType("e2e_hello")[0]);
    sock.serverSend(JSON.stringify(gw.ackFor(hello, "home-mac")));
    acceptTofu(sock, gw, h, "home-mac"); // v1.41.9：确认 → 重新握手 → 第二帧 ack → active

    // 加密会话已立后，同机器的明文业务帧不进消息流，且触发降级告警（一次）
    sock.serverSend('{"v":1,"type":"text","convId":"web:default","text":"注入","machine":"home-mac"}');
    expect(h.frames.filter((f) => f.type === "text")).toHaveLength(0);
    expect(h.events).toContainEqual({ kind: "downgrade", machine: "home-mac" });
    sock.serverSend('{"v":1,"type":"text","convId":"web:default","text":"再来","machine":"home-mac"}');
    expect(h.events.filter((e) => e.kind === "downgrade")).toHaveLength(1); // 每连接每机器一次
    // 无会话机器（work-mac 从未握手）的 enc 帧丢弃（machines 管理帧照常转发）
    sock.serverSend(
      JSON.stringify(sealFrame("work-mac", randomBytes(32), { v: 1, type: "ping" })),
    );
    expect(h.frames.filter((f) => f.type !== "machines")).toHaveLength(0);
    // 管理帧照常放行
    sock.serverSend('{"v":1,"type":"pong"}');
    expect(h.frames).toContainEqual({ v: 1, type: "pong" });
  });

  it("身份变化（已 pin 公钥改变）→ alert：发送阻断、事件上抛", () => {
    const pinned = makeGateway();
    // 预置 pin（上次会话 TOFU 确认过）
    localStorage.setItem(
      "imagent.e2ePins",
      JSON.stringify({ "home-mac": { ik: pinned.ik, gk: pinned.gk, pinnedAt: 1 } }),
    );
    const mitm = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    pushMachines(sock, ["home-mac"]);
    const hello = JSON.parse(sock.sentOfType("e2e_hello")[0]);
    // 中间人用自己密钥应答、冒充 pin 的身份公钥
    sock.serverSend(JSON.stringify(mitm.ackFor(hello, "home-mac", { ik: pinned.ik })));
    expect(h.events).toContainEqual({ kind: "alert", machine: "home-mac", reason: "identity_changed" });
    expect(h.mgr.sendMessage("web:default", "x", "home-mac")).toBe(false);
  });

  it("断线重连清空 e2e 会话：machines 帧再次驱动新握手", () => {
    const gw = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const s1 = readyFlow(h);
    pushMachines(s1, ["home-mac"]);
    const hello1 = JSON.parse(s1.sentOfType("e2e_hello")[0]);
    s1.serverSend(JSON.stringify(gw.ackFor(hello1, "home-mac")));
    acceptTofu(s1, gw, h, "home-mac"); // v1.41.9：确认 → 重新握手 → 第二帧 ack → active
    // 加密发送可用
    expect(h.mgr.sendMessage("web:default", "s", "home-mac")).toBe(true);
    expect(s1.sentOfType("enc")).toHaveLength(1);

    s1.emit("close", { code: 1006 });
    vi.advanceTimersByTime(1_000); // 退避重连
    const s2 = lastSock();
    s2.emit("open");
    expect(s2.sent[0]).toContain('"hello"');
    s2.serverSend(HELLO_ACK);
    // 重连后尚未握手：出站回明文（会话已随连接销毁）
    expect(h.mgr.sendMessage("web:default", "x", "home-mac")).toBe(true);
    expect(JSON.parse(s2.sent.at(-1) ?? "{}").type).toBe("message");
    // machines 帧再次到达 → 新 e2e_hello（第二次握手）
    pushMachines(s2, ["home-mac"]);
    expect(s2.sentOfType("e2e_hello")).toHaveLength(1);
  });

  it("心跳 ping：active 后 25s ping 亦为 enc 帧", () => {
    const gw = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    pushMachines(sock, ["home-mac"]);
    const hello = JSON.parse(sock.sentOfType("e2e_hello")[0]);
    sock.serverSend(JSON.stringify(gw.ackFor(hello, "home-mac")));
    const { hello: hello2, ack: ack2 } = acceptTofu(sock, gw, h, "home-mac");
    vi.advanceTimersByTime(25_000);
    const ping = sock.sent.at(-1) ?? "{}";
    expect(JSON.parse(ping).type).toBe("enc");
    expect(openBox(gw.gatewayKey(hello2, ack2.n2), JSON.parse(ping).box)).toBe(
      JSON.stringify({ v: 1, type: "ping", machine: "home-mac" }),
    );
  });

  it("旧 relay 单机模式（无 machines 帧）：永不发起握手，出站恒明文", () => {
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    vi.advanceTimersByTime(60_000);
    expect(sock.sentOfType("e2e_hello")).toHaveLength(0);
    expect(h.mgr.sendMessage("web:default", "x", "home-mac")).toBe(true);
    expect(JSON.parse(sock.sent.at(-1) ?? "{}").type).toBe("message");
    expect(h.events).toHaveLength(0);
  });

  it("e2e_hello 携带 sid（hello_ack.session.id）：定向加密形态", () => {
    const gw = makeGateway();
    const h = makeE2eManager("home-mac");
    h.mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    // 会话签发 ack（session.id = sid-1）——e2e_hello 应携带该 sid
    sock.serverSend(
      JSON.stringify({
        v: 1,
        type: "hello_ack",
        role: "browser",
        session: { id: "sid-1", token: "st-1", expiresAt: 1_900_000_000_000 },
      }),
    );
    pushMachines(sock, ["home-mac"]);
    const hello = JSON.parse(sock.sentOfType("e2e_hello")[0]);
    expect(hello.sid).toBe("sid-1");
    // 定向形态下握手照常完成
    sock.serverSend(JSON.stringify(gw.ackFor(hello, "home-mac")));
    expect(h.events).toContainEqual({ kind: "tofu", machine: "home-mac", gkFp: expect.any(String), idFp: expect.any(String) });
  });

  // v1.41.9 会话探活：网关侧会话表随「网关 ↔ 中继」连接重建，而浏览器自己的
  // WS 可能还活着——手里的密钥已作废却无感（出站全帧被网关 WARN 丢弃）。
  // machines 快照后逐 active 机器发 enc-ping，窗口内未收到**可解密**的 pong
  // 即判会话失效 → 立即重连（重连后 machines 帧重新握手）。
  describe("v1.41.9 会话探活", () => {
    const PROBE_MS = 50;

    /** 本组专属 harness（注入小探活窗口）；afterEach 统一 close，防在途
     *  定时器/连接跨用例残留。 */
    const probeManagers: WsManager[] = [];
    const makeProbeManager = (probeMs = PROBE_MS): E2eHarness => {
      const h = makeE2eManager("home-mac", { sessionProbeMs: probeMs });
      probeManagers.push(h.mgr);
      return h;
    };

    /** 建连并让 home-mac 走到 active：首连快照（→ 握手 → TOFU 确认 → 重新
     *  握手）。首连快照到达时条目都是 pending，不触发探活。返回当前 socket
     *  与网关侧会话密钥。 */
    const connectActive = (gw: ReturnType<typeof makeGateway>, h: E2eHarness) => {
      h.mgr.connect();
      const sock = readyFlow(h);
      pushMachines(sock, ["home-mac"]);
      const key = toActive(sock, gw, h, "home-mac");
      expect(sock.sentOfType("enc")).toHaveLength(0);
      return { sock, key };
    };

    afterEach(() => {
      for (const m of probeManagers.splice(0)) {
        m.close();
      }
    });

    it("握手完成后再推 machines：只对 active 机器发一帧 enc-ping；confirming / 明文选择的机器不发，也不重握手", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      h.mgr.connect();
      const sock = readyFlow(h);
      pushMachines(sock, ["home-mac", "work-mac", "old-mac"]);
      expect(sock.sentOfType("e2e_hello")).toHaveLength(3);
      // home-mac → active；work-mac 停在 confirming；old-mac 显式选择明文
      const homeKey = toActive(sock, gw, h, "home-mac");
      ackLatest(sock, gw, "work-mac");
      expect(h.events).toContainEqual(expect.objectContaining({ kind: "tofu", machine: "work-mac" }));
      h.mgr.allowPlaintextE2e("old-mac");
      const hellosBefore = sock.sentOfType("e2e_hello").length;

      // 第二帧快照（如网关重连中继后的广播）
      pushMachines(sock, ["home-mac", "work-mac", "old-mac"]);
      expect(sock.sentOfType("e2e_hello")).toHaveLength(hellosBefore); // 探活不等于重握手
      const pings = sock.sentOfType("enc");
      expect(pings).toHaveLength(1);
      const enc = JSON.parse(pings[0]) as { machine: string; box: string };
      expect(enc.machine).toBe("home-mac");
      expect(openBox(homeKey, enc.box)).toBe(JSON.stringify({ v: 1, type: "ping", machine: "home-mac" }));
    });

    it("探活窗口内收到该机器可解密的 pong → 不重连（socket 不变、状态不变）", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock, key } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]); // 网关重连中继后的快照（会话仍有效的形态）
      expect(sock.sentOfType("enc")).toHaveLength(1);
      const statusesBefore = [...h.statuses];
      vi.advanceTimersByTime(10);
      sock.serverSend(gatewayPong("home-mac", key));
      vi.advanceTimersByTime(PROBE_MS * 4); // 远超探活窗口
      expect(sock.closedWith).toHaveLength(0);
      expect(FakeWebSocket.created).toHaveLength(1);
      expect(h.statuses).toEqual(statusesBefore);
      // 应答 pong 照常走既有分发路径（同时刷新链路活性时钟）
      expect(h.frames).toContainEqual(expect.objectContaining({ type: "pong", machine: "home-mac" }));
    });

    it("多台 active 逐机器记账：部分应答不清定时器，仍有机器未应答即重连", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      h.mgr.connect();
      const sock = readyFlow(h);
      pushMachines(sock, ["home-mac", "work-mac"]);
      const keys = {
        "home-mac": toActive(sock, gw, h, "home-mac"),
        "work-mac": toActive(sock, gw, h, "work-mac"),
      };
      pushMachines(sock, ["home-mac", "work-mac"]);
      expect(sock.sentOfType("enc")).toHaveLength(2);
      vi.advanceTimersByTime(10);
      sock.serverSend(gatewayPong("home-mac", keys["home-mac"]));
      vi.advanceTimersByTime(PROBE_MS); // 窗口到点：work-mac 始终未应答
      expect(sock.closedWith).toContain(1000);
      expect(FakeWebSocket.created).toHaveLength(2);
    });

    it("窗口内没收到可解密 pong → 立即重连（不走退避）；新连接 machines 到达后重新发起 e2e_hello", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]); // 网关重连中继：会话表已空，ping 无人能解
      vi.advanceTimersByTime(PROBE_MS - 1);
      expect(FakeWebSocket.created).toHaveLength(1); // 窗口内不动
      vi.advanceTimersByTime(1);
      expect(sock.closedWith).toContain(1000);
      expect(FakeWebSocket.created).toHaveLength(2); // 立即新建——未推进任何退避时间
      expect(h.statuses).toContain("reconnecting");

      const s2 = lastSock();
      s2.emit("open");
      expect(s2.sent[0]).toContain('"hello"');
      s2.serverSend(HELLO_ACK);
      expect(h.statuses.at(-1)).toBe("ready");
      expect(s2.sentOfType("e2e_hello")).toHaveLength(0);
      pushMachines(s2, ["home-mac"]);
      expect(s2.sentOfType("e2e_hello")).toHaveLength(1);
      // 已 pin → 新握手 ack 直接 active（无二次弹层）；出站 enc 用新握手的密钥
      const { key } = ackLatest(s2, gw, "home-mac");
      expect(h.events.filter((e) => e.kind === "tofu")).toHaveLength(1);
      expect(h.mgr.sendMessage("web:default", "重连后", "home-mac")).toBe(true);
      const enc = JSON.parse(s2.sentOfType("enc").at(-1) ?? "null") as { box: string };
      expect(openBox(key, enc.box)).toBe(
        JSON.stringify({ v: 1, type: "message", convId: "web:default", text: "重连后", machine: "home-mac" }),
      );
      // 防环：新连接首帧 machines 时条目是 pending（不被探），此后不再重连
      vi.advanceTimersByTime(PROBE_MS * 4);
      expect(FakeWebSocket.created).toHaveLength(2);
      expect(s2.closedWith).toHaveLength(0);
    });

    it("窗口内只收到可解密的非 pong 帧（如重启前在途的 text）→ 仍判失效重连", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock, key } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]);
      vi.advanceTimersByTime(10);
      sock.serverSend(
        JSON.stringify(
          sealFrame("home-mac", key, {
            v: 1,
            type: "text",
            convId: "web:default",
            text: "旧帧",
            machine: "home-mac",
          }),
        ),
      );
      expect(h.frames).toContainEqual(expect.objectContaining({ type: "text", text: "旧帧" })); // 业务帧照常分发
      vi.advanceTimersByTime(PROBE_MS);
      expect(sock.closedWith).toContain(1000);
      expect(FakeWebSocket.created).toHaveLength(2);
    });

    it("明文 pong 不算应答（会话表为空时网关的 pong 回明文）→ 仍判失效重连", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]);
      vi.advanceTimersByTime(10);
      sock.serverSend('{"v":1,"type":"pong","machine":"home-mac"}');
      vi.advanceTimersByTime(PROBE_MS);
      expect(sock.closedWith).toContain(1000);
      expect(FakeWebSocket.created).toHaveLength(2);
    });

    it("探活在途时连接因别的原因已换代 → 旧定时器到点不动新连接", () => {
      const gw = makeGateway();
      // 探活窗口（2s）刻意长于重连退避（1s）：让旧探活的原到点晚于新连接建立
      const h = makeProbeManager(2_000);
      const { sock } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]);
      expect(sock.sentOfType("enc")).toHaveLength(1); // 探活确已发起（在途）
      vi.advanceTimersByTime(100);
      // 别的原因换代：服务端断开 → 退避 1s 重连 → 新连接就绪（先不喂 machines
      // 快照——排除「新一帧快照顺手清掉旧探活」这条路径，单测连接终止路径）
      sock.emit("close", { code: 1006 });
      vi.advanceTimersByTime(1_000);
      const s2 = lastSock();
      s2.emit("open");
      s2.serverSend(HELLO_ACK);
      expect(h.statuses.at(-1)).toBe("ready");
      // 旧探活的原到点（t=2000，晚于新连接建立）：新连接不受影响
      vi.advanceTimersByTime(2_000);
      expect(s2.closedWith).toHaveLength(0);
      expect(FakeWebSocket.created).toHaveLength(2);
      expect(h.statuses.at(-1)).toBe("ready");
      // 新连接照常运转：快照到达 → 重新握手（已 pin → 直接 active）
      pushMachines(s2, ["home-mac"]);
      ackLatest(s2, gw, "home-mac");
      expect(h.events.filter((e) => e.kind === "active")).toHaveLength(2);
    });

    it("连续两帧 machines → 只有一个在途探活（旧定时器被清），到点只重连一次", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]); // 探活 ①：t=0 发起
      expect(sock.sentOfType("enc")).toHaveLength(1);
      vi.advanceTimersByTime(30);
      pushMachines(sock, ["home-mac"]); // 探活 ②：t=30 重新发起，①的定时器被清
      expect(sock.sentOfType("enc")).toHaveLength(2);
      vi.advanceTimersByTime(20); // t=50：① 的原到点——不重连
      expect(FakeWebSocket.created).toHaveLength(1);
      expect(sock.closedWith).toHaveLength(0);
      vi.advanceTimersByTime(30); // t=80：② 到点——重连一次
      expect(FakeWebSocket.created).toHaveLength(2);
      expect(sock.closedWith).toEqual([1000]);
      vi.advanceTimersByTime(PROBE_MS * 4);
      expect(FakeWebSocket.created).toHaveLength(2);
    });

    it("close() 后探活定时器不再触发任何动作", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]);
      expect(sock.sentOfType("enc")).toHaveLength(1);
      const statusesBefore = [...h.statuses];
      h.mgr.close();
      vi.advanceTimersByTime(PROBE_MS * 4);
      expect(FakeWebSocket.created).toHaveLength(1);
      expect(sock.closedWith).toEqual([1000]); // 仅 close() 自己的那次
      expect(h.statuses).toEqual(statusesBefore);
    });

    it("新快照不再包含在探机器（网关刚上线又掉线）→ 不再探、不重连", () => {
      const gw = makeGateway();
      const h = makeProbeManager();
      const { sock } = connectActive(gw, h);
      pushMachines(sock, ["home-mac"]); // 探活在途
      expect(sock.sentOfType("enc")).toHaveLength(1);
      vi.advanceTimersByTime(10);
      pushMachines(sock, []); // home-mac 已下线：会话确已失效，但机器不在线，无从恢复
      expect(sock.sentOfType("enc")).toHaveLength(1); // 未重探
      vi.advanceTimersByTime(PROBE_MS * 4);
      expect(sock.closedWith).toHaveLength(0);
      expect(FakeWebSocket.created).toHaveLength(1);
      // 该机器重新上线 → 快照含它 → 探活 → 失效即重连（此时才恢复）
      pushMachines(sock, ["home-mac"]);
      expect(sock.sentOfType("enc")).toHaveLength(2);
      vi.advanceTimersByTime(PROBE_MS);
      expect(sock.closedWith).toContain(1000);
      expect(FakeWebSocket.created).toHaveLength(2);
    });
  });

  // v1.41.9 心跳逐机器：网关半开清理只豁免「收到过可解密入站帧」的会话，而
  // 此前心跳只发当前机器——非当前机器的 active 会话从未向网关证明持钥，60s
  // 后被清，切过去 sync / history_query 全部解不开而界面无感。
  describe("v1.41.9 心跳逐机器（全部在线 active 会话）", () => {
    const PING = (m: string): string => JSON.stringify({ v: 1, type: "ping", machine: m });

    /** 本组 harness（afterEach 统一 close；前台探活用例会覆写
     *  visibilityState——jsdom 默认 prerender——用后还原）。current 传函数 =
     *  当前机器可在用例内切换（侧栏切机 / 掉线自动切换的形态）。 */
    const tracked: WsManager[] = [];
    const makeHbManager = (current: string | (() => string | undefined) = "home-mac"): E2eHarness => {
      const h = makeE2eManager(current);
      tracked.push(h.mgr);
      return h;
    };
    /** 建连 → 首连快照两台 → 两台均 active（当前机器 home-mac）。 */
    const connectTwoActive = (gw: ReturnType<typeof makeGateway>, h: E2eHarness) => {
      h.mgr.connect();
      const sock = readyFlow(h);
      pushMachines(sock, ["home-mac", "work-mac"]);
      const keys = toActiveAll(sock, gw, h, ["home-mac", "work-mac"]);
      expect(sock.sentOfType("enc")).toHaveLength(0); // 未到 tick：尚无心跳
      return { sock, keys };
    };

    afterEach(() => {
      delete (document as { visibilityState?: string }).visibilityState;
      for (const m of tracked.splice(0)) {
        m.close();
      }
    });

    it("两台均 active：每个 tick 各发一帧 enc ping，分别可被各自的网关密钥解开、内帧 machine 正确", () => {
      const gw = makeGateway();
      let current = "home-mac";
      const h = makeHbManager(() => current);
      const { sock, keys } = connectTwoActive(gw, h);

      vi.advanceTimersByTime(25_000);
      expect(sock.sentOfType("enc")).toHaveLength(2);
      expect(sock.sentOfType("ping")).toHaveLength(0); // 无多余的明文/重复帧
      expect(openEncByMachine(sock, keys)).toEqual({
        "home-mac": PING("home-mac"),
        "work-mac": PING("work-mac"),
      });

      // 切到 work-mac 后再过一个 tick：仍是两台各一帧（当前机器不重复发）
      current = "work-mac";
      vi.advanceTimersByTime(25_000);
      expect(sock.sentOfType("enc")).toHaveLength(4);
      expect(sock.sentOfType("ping")).toHaveLength(0);
    });

    it("当前机器非 active、另一台 active：当前机器一帧明文 ping + 另一台一帧 enc ping", () => {
      const gw = makeGateway();
      const h = makeHbManager();
      h.mgr.connect();
      const sock = readyFlow(h);
      pushMachines(sock, ["home-mac", "work-mac"]);
      const { "work-mac": workKey } = toActiveAll(sock, gw, h, ["work-mac"]);
      // 当前机器：用户显式选择明文（unsupported + 放行，非 active）
      h.mgr.allowPlaintextE2e("home-mac");

      vi.advanceTimersByTime(25_000);
      expect(sock.sentOfType("ping")).toEqual([PING("home-mac")]); // 明文：保命线照旧
      expect(sock.sentOfType("enc")).toHaveLength(1);
      expect(openEncByMachine(sock, { "work-mac": workKey })).toEqual({ "work-mac": PING("work-mac") });
    });

    it("当前机器 active 且是唯一 active：每个 tick 恰好一帧 enc ping（不重复，与 v1.41.8 一致）", () => {
      const gw = makeGateway();
      const h = makeHbManager();
      h.mgr.connect();
      const sock = readyFlow(h);
      pushMachines(sock, ["home-mac"]);
      const keys = toActiveAll(sock, gw, h, ["home-mac"]);

      for (let tick = 1; tick <= 2; tick++) {
        vi.advanceTimersByTime(25_000);
        expect(sock.sentOfType("enc")).toHaveLength(tick);
        expect(sock.sentOfType("ping")).toHaveLength(0);
      }
      expect(openEncByMachine(sock, keys)).toEqual({ "home-mac": PING("home-mac") });
    });

    it("切回前台探活路径（probeOnce(pingIntervalMs)）同样覆盖全部在线 active 机器", () => {
      const gw = makeGateway();
      const h = makeHbManager();
      const { sock, keys } = connectTwoActive(gw, h);
      vi.advanceTimersByTime(5_000); // 未到 25s tick：尚无周期心跳
      expect(sock.sentOfType("enc")).toHaveLength(0);

      Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
      document.dispatchEvent(new Event("visibilitychange"));
      expect(sock.sentOfType("enc")).toHaveLength(2);
      expect(openEncByMachine(sock, keys)).toEqual({
        "home-mac": PING("home-mac"),
        "work-mac": PING("work-mac"),
      });
    });

    it("掉线机器不发（e2e 条目仍是 active）：重新上线后恢复对两台各发一帧", () => {
      const gw = makeGateway();
      const h = makeHbManager();
      const { sock, keys } = connectTwoActive(gw, h);

      // work-mac 掉线：快照只剩 home-mac。中继对「带 machine 且该机器不在线」
      // 的帧一律回 machine_offline error（App 弹「该机器离线」toast）——若照发
      // 就是每 25s 弹一次
      pushMachines(sock, ["home-mac"]);
      sock.serverSend(gatewayPong("home-mac", keys["home-mac"])); // 落定 home-mac 的会话探活
      vi.advanceTimersByTime(25_000);
      expect(Object.keys(openEncByMachine(sock, keys))).toEqual(["home-mac"]);

      // work-mac 重新上线：条目未清（不重新握手）→ 心跳恢复对两台各发一帧
      pushMachines(sock, ["home-mac", "work-mac"]);
      answerEncPings(sock, keys);
      expect(sock.sentOfType("e2e_hello")).toHaveLength(4); // 2 台 ×（首连 + 确认后重新握手）
      const before = sock.sentOfType("enc").length;
      vi.advanceTimersByTime(25_000);
      expect(sock.sentOfType("enc")).toHaveLength(before + 2);
    });

    it("全部掉线：当前机器照旧发它那一帧（保命线不因掉线停发，与 v1.41.8 一致）", () => {
      const gw = makeGateway();
      const h = makeHbManager();
      const { sock, keys } = connectTwoActive(gw, h);
      pushMachines(sock, []);
      vi.advanceTimersByTime(25_000);
      expect(sock.sentOfType("enc")).toHaveLength(1);
      expect(openEncByMachine(sock, keys)).toEqual({ "home-mac": PING("home-mac") });
    });
  });
});

// ---------------------------------------------------------------------------
// v1.5 配对模式 + 管理帧
// ---------------------------------------------------------------------------

describe("WsManager 配对与管理（v1.5）", () => {
  it("authFailReason：pairing denied → pairing_denied", () => {
    expect(authFailReason("pairing denied")).toBe("pairing_denied");
  });

  it("配对模式：hello 无凭证带 pairing+device（字节级）；pairing_pending 上抛且不再按 hello 超时断开", () => {
    const codes: string[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/ws",
      pairing: "iPhone/Safari",
      onFrame: () => {},
      onStatus: () => {},
      onAuthFailed: () => {},
      onPairingPending: (c) => codes.push(c),
      wsFactory: (u) => new FakeWebSocket(u),
    });
    mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    expect(sock.sent[0]).toBe('{"v":1,"type":"hello","pairing":true,"device":"iPhone/Safari"}');

    // 等待码到达：上抛回调 + 清 hello 超时（推进 >10s 不重连不断开）
    sock.serverSend('{"v":1,"type":"pairing_pending","code":"483920"}');
    expect(codes).toEqual(["483920"]);
    vi.advanceTimersByTime(60_000);
    expect(FakeWebSocket.created).toHaveLength(1);
    expect(sock.sentOfType("sync")).toHaveLength(0); // 未 ready 不发 sync
    expect(sock.closedWith).toHaveLength(0);
    mgr.close();
  });

  it("配对批准：同连接 hello_ack.session → onSession + ready + sync（新设备进入）", () => {
    const sessions: SessionInfo[] = [];
    const statuses: string[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/ws",
      pairing: "iPhone/Safari",
      onFrame: () => {},
      onStatus: (s) => statuses.push(s),
      onAuthFailed: () => {},
      onSession: (s) => sessions.push(s),
      onPairingPending: () => {},
      wsFactory: (u) => new FakeWebSocket(u),
    });
    mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.serverSend('{"v":1,"type":"pairing_pending","code":"111111"}');
    sock.serverSend(
      '{"v":1,"type":"hello_ack","role":"user","session":{"id":"sid-p","token":"tok-p","expiresAt":1900000000}}',
    );
    expect(sessions).toEqual([{ id: "sid-p", token: "tok-p", expiresAt: 1900000000 }]);
    expect(statuses.at(-1)).toBe("ready");
    // 批准后就绪：立即发 sync（未收 machines → 不带 machine 字段）
    expect(sock.sentOfType("sync")).toEqual(['{"v":1,"type":"sync"}']);
    mgr.close();
  });

  it("配对被拒：close 1008 pairing denied → pairing_denied 且不重连", () => {
    let reason: AuthFailReason | null = null;
    const mgr = new WsManager({
      url: "wss://example.test/ws",
      pairing: "iPhone/Safari",
      onFrame: () => {},
      onStatus: () => {},
      onAuthFailed: (r) => {
        reason = r;
      },
      wsFactory: (u) => new FakeWebSocket(u),
    });
    mgr.connect();
    const sock = lastSock();
    sock.emit("open");
    sock.emit("close", { code: 1008, reason: "pairing denied" });
    expect(reason).toBe("pairing_denied");
    vi.advanceTimersByTime(60_000);
    expect(FakeWebSocket.created).toHaveLength(1);
  });

  it("管理帧六发送器：字节级形状（adminToken 携带；多机模式下也恒明文不带 machine）", () => {
    const h = makeManager("tok-1", () => "home-mac");
    h.mgr.connect();
    const sock = readyFlow(h);
    const at = "at".repeat(32);
    expect(h.mgr.sendMachineList(at)).toBe(true);
    expect(h.mgr.sendMachineAdd(at, "mac-mini", "ab".repeat(32))).toBe(true);
    expect(h.mgr.sendMachineRemove(at, "mac-mini")).toBe(true);
    expect(h.mgr.sendUserTokenRotate(at)).toBe(true);
    expect(h.mgr.sendPairingApprove(at, "483920")).toBe(true);
    expect(h.mgr.sendPairingDeny(at, "483920")).toBe(true);
    expect(sock.sent).toContain(`{"v":1,"type":"machine_list","adminToken":"${at}"}`);
    expect(sock.sent).toContain(
      `{"v":1,"type":"machine_add","adminToken":"${at}","name":"mac-mini","pubkey":"${"ab".repeat(32)}"}`,
    );
    expect(sock.sent).toContain(`{"v":1,"type":"machine_remove","adminToken":"${at}","name":"mac-mini"}`);
    expect(sock.sent).toContain(`{"v":1,"type":"user_token_rotate","adminToken":"${at}"}`);
    expect(sock.sent).toContain(`{"v":1,"type":"pairing_approve","adminToken":"${at}","code":"483920"}`);
    expect(sock.sent).toContain(`{"v":1,"type":"pairing_deny","adminToken":"${at}","code":"483920"}`);
    // 管理帧不经 e2e 包装（relay 自产自消恒明文）：全部为明文 type 直发
    expect(sock.sentOfType("machine_list")).toHaveLength(1);
    expect(sock.sentOfType("enc")).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// v1.6 工作空间发送器：字节级形状 / machine 路由键 / 入站帧转发
// ---------------------------------------------------------------------------

describe("WsManager 工作空间（v1.6）", () => {
  it("四发送器：ready 前返回 false，ready 后字节精确（machine 缺省不带字段）", () => {
    const h = makeManager();
    h.mgr.connect();
    expect(h.mgr.listWorkspaces()).toBe(false);
    expect(h.mgr.saveWorkspace("code", "/u/code")).toBe(false);
    expect(h.mgr.removeWorkspace("code")).toBe(false);
    expect(h.mgr.setConvWorkdir("web:default", "/u/code")).toBe(false);
    const sock = readyFlow(h);
    expect(h.mgr.listWorkspaces()).toBe(true);
    expect(h.mgr.saveWorkspace("code", "/u/code")).toBe(true);
    expect(h.mgr.removeWorkspace("code")).toBe(true);
    expect(h.mgr.setConvWorkdir("web:default", "/u/code")).toBe(true);
    expect(sock.sent).toContain('{"v":1,"type":"workspace_list"}');
    expect(sock.sent).toContain('{"v":1,"type":"workspace_save","name":"code","path":"/u/code"}');
    expect(sock.sent).toContain('{"v":1,"type":"workspace_remove","name":"code"}');
    expect(sock.sent).toContain('{"v":1,"type":"conv_workdir_set","convId":"web:default","path":"/u/code"}');
  });

  it("四发送器带 machine：帧尾携带（多机路由约定同 message 族）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    expect(h.mgr.listWorkspaces("home-mac")).toBe(true);
    expect(h.mgr.saveWorkspace("code", "/u/code", "home-mac")).toBe(true);
    expect(h.mgr.removeWorkspace("code", "home-mac")).toBe(true);
    expect(h.mgr.setConvWorkdir("web:default", "/u/code", "home-mac")).toBe(true);
    expect(sock.sent).toContain('{"v":1,"type":"workspace_list","machine":"home-mac"}');
    expect(sock.sent).toContain(
      '{"v":1,"type":"workspace_save","name":"code","path":"/u/code","machine":"home-mac"}',
    );
    expect(sock.sent).toContain('{"v":1,"type":"workspace_remove","name":"code","machine":"home-mac"}');
    expect(sock.sent).toContain(
      '{"v":1,"type":"conv_workdir_set","convId":"web:default","path":"/u/code","machine":"home-mac"}',
    );
  });

  it("入站 workspaces / conv_workdir 明文帧经 onFrame 转发（reducer 消费路径）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    sock.serverSend(
      '{"v":1,"type":"workspaces","default":"/d","workspaces":[{"name":"a","path":"/a","lastUsedAt":100}],"convs":{"web:default":"/a","local:work":"/t"},"machine":"home-mac"}',
    );
    sock.serverSend('{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/u/x","machine":"home-mac"}');
    expect(h.frames).toContainEqual(
      expect.objectContaining({ type: "workspaces", default: "/d", machine: "home-mac" }),
    );
    expect(h.frames).toContainEqual(
      expect.objectContaining({ type: "conv_workdir", workdir: "/u/x", machine: "home-mac" }),
    );
  });
});

// ---------------------------------------------------------------------------
// v1.8 会话历史重放：queryHistory 发送器 + 入站 history 帧转发
// ---------------------------------------------------------------------------

describe("WsManager 会话历史重放（v1.8）", () => {
  it("queryHistory：ready 前返回 false；ready 后字节精确（machine 缺省不带字段）", () => {
    const h = makeManager();
    h.mgr.connect();
    expect(h.mgr.queryHistory("web:default", 0, "home-mac")).toBe(false);
    const sock = readyFlow(h);
    expect(h.mgr.queryHistory("web:default", 5, "home-mac")).toBe(true);
    expect(sock.sent.at(-1)).toBe(
      '{"v":1,"type":"history_query","convId":"web:default","since":5,"machine":"home-mac"}',
    );
    expect(h.mgr.queryHistory("web:default", 0)).toBe(true);
    expect(sock.sent.at(-1)).toBe('{"v":1,"type":"history_query","convId":"web:default","since":0}');
  });

  it("入站 history 明文帧经 onFrame 转发（单机模式无 e2e 条目放行）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    sock.serverSend(
      '{"v":1,"type":"history","convId":"web:default","entries":[{"kind":"text","key":"h:1","text":"重放","at":1}],"lastSeq":1,"trimmed":false}',
    );
    expect(h.frames.at(-1)).toMatchObject({ type: "history", lastSeq: 1, trimmed: false });
  });
});

// ---------------------------------------------------------------------------
// v1.7.x 切回前台探活（visibilitychange）：iOS 冻结/掐断后台 WS 的半开问题
// ---------------------------------------------------------------------------

describe("WsManager 前台探活（visibilitychange）", () => {
  /** jsdom 默认 visibilityState 为 "prerender"——覆写为指定值（用后还原）。 */
  const setVisibility = (state: string) => {
    Object.defineProperty(document, "visibilityState", { value: state, configurable: true });
  };
  const resetVisibility = () => {
    delete (document as { visibilityState?: string }).visibilityState;
  };
  const goForeground = () => {
    setVisibility("visible");
    document.dispatchEvent(new Event("visibilitychange"));
  };

  /** 本组专属 harness：经自有 wsFactory 只统计**本 manager** 的 socket——
   *  本文件早期用例遗留的 WsManager（未 close）监听器仍在 document 上，会
   *  随本组的 visibilitychange 派发与 fake 时钟推进被唤醒产生额外 socket，
   *  绝对计数会跨用例耦合（生产代码 close() 恒移除监听，纯测试环境现象）。 */
  interface VisHarness {
    mgr: WsManager;
    mine: FakeWebSocket[];
    statuses: string[];
  }
  const visManagers: WsManager[] = [];

  function makeVisManager(extra?: { visibleProbeMs?: number }): VisHarness {
    const mine: FakeWebSocket[] = [];
    const statuses: string[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      token: "tok-1",
      onFrame: () => {},
      onStatus: (s) => statuses.push(s),
      onAuthFailed: () => {},
      visibleProbeMs: extra?.visibleProbeMs,
      wsFactory: (u) => {
        const s = new FakeWebSocket(u);
        mine.push(s);
        return s;
      },
    });
    visManagers.push(mgr);
    return { mgr, mine, statuses };
  }

  afterEach(() => {
    resetVisibility();
    for (const m of visManagers.splice(0)) {
      m.close();
    }
  });

  it("visible：socket 为 null 且退避已排 → 清 timer 立即重连（退避计数保留）", () => {
    const h = makeVisManager();
    h.mgr.connect();
    const s1 = h.mine[0];
    s1.emit("open");
    s1.serverSend(HELLO_ACK);
    expect(h.statuses.at(-1)).toBe("ready");
    s1.emit("close", { code: 1006 });
    expect(h.statuses.at(-1)).toBe("reconnecting");
    // 退避 1s 未到点时切回前台 → 立即新建连接（不等剩余退避）
    vi.advanceTimersByTime(500);
    goForeground();
    expect(h.mine).toHaveLength(2);
    expect(h.statuses.at(-1)).toBe("connecting");
    // 立即重试不抬退避上限：attempts 计数保留 → 新连接再失败，下一轮退避 2s
    const s2 = h.mine[1];
    s2.emit("open");
    s2.emit("close", { code: 1006 });
    vi.advanceTimersByTime(1_999);
    expect(h.mine).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(h.mine).toHaveLength(3);
  });

  it("visible：连接 OPEN 但 lastFrameAt 超过 pingIntervalMs → 半开判死：teardown + 立即排重连", () => {
    const h = makeVisManager();
    h.mgr.connect();
    const sock = h.mine[0];
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    // 26s 无入站（25s tick 只发出站 ping、无 pong）：lastFrameAt 陈旧超 25s
    vi.advanceTimersByTime(26_000);
    goForeground();
    expect(sock.closedWith).toContain(1000); // 主动断开半开连接
    expect(h.statuses.at(-1)).toBe("reconnecting");
    vi.advanceTimersByTime(1_000);
    expect(h.mine).toHaveLength(2); // 退避到点重连
  });

  it("visible：连接新鲜（< pingIntervalMs）→ 发一次 ping 探活、不断开不重连", () => {
    const h = makeVisManager();
    h.mgr.connect();
    const sock = h.mine[0];
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    vi.advanceTimersByTime(5_000); // 未到 25s tick：无周期 ping
    expect(sock.sentOfType("ping")).toHaveLength(0);
    goForeground();
    expect(sock.sentOfType("ping")).toHaveLength(1); // 探活 ping 一枚
    expect(sock.closedWith).toHaveLength(0);
    expect(h.mine).toHaveLength(1); // 无重连
  });

  it("hidden 不触发；手动 close 后监听移除（visible 亦不动作）", () => {
    const h = makeVisManager();
    h.mgr.connect();
    const sock = h.mine[0];
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    setVisibility("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(sock.sentOfType("ping")).toHaveLength(0);
    // 组件卸载/退出登录路径：close() 摘监听
    h.mgr.close();
    goForeground();
    expect(h.mine).toHaveLength(1);
    expect(sock.sentOfType("ping")).toHaveLength(0);
  });

  it("superseded / 认证失败终态：监听随终态移除，探活不重连", () => {
    const h1 = makeVisManager();
    h1.mgr.connect();
    h1.mine[0].emit("open");
    h1.mine[0].serverSend(HELLO_ACK);
    h1.mine[0].emit("close", { code: 4001 });
    goForeground();
    expect(h1.mine).toHaveLength(1); // 终态不自动重连

    const h2 = makeVisManager();
    h2.mgr.connect();
    h2.mine[0].emit("open");
    h2.mine[0].serverSend(HELLO_ACK);
    h2.mine[0].emit("close", { code: 1008 });
    goForeground();
    expect(h2.mine).toHaveLength(1); // 仅初始连接
    expect(h2.statuses.at(-1)).not.toBe("connecting");
  });

  // v1.40.1：探活 ping 发出后 visibleProbeMs 内无任何入站帧 → 判死立即重连
  //（不再等下个心跳 tick 的 25s 间隔 + 60s 僵死阈值——iOS 挂起恢复后窗口过长）。
  it("v1.40.1：visible 探活后 visibleProbeMs 内有回应（pong）→ 不判死不重连", () => {
    const h = makeVisManager({ visibleProbeMs: 50 });
    h.mgr.connect();
    const sock = h.mine[0];
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    vi.advanceTimersByTime(5_000); // 未到 25s tick：探活 ping 一枚（lastFrameAt 新鲜）
    goForeground();
    expect(sock.sentOfType("ping")).toHaveLength(1);
    vi.advanceTimersByTime(10);
    sock.serverSend('{"v":1,"type":"pong"}'); // 探活回应：lastFrameAt 刷到 sentAt 之后
    vi.advanceTimersByTime(40); // 探活定时器到点：lastFrameAt >= sentAt → 链路健康
    expect(sock.closedWith).toHaveLength(0);
    expect(h.mine).toHaveLength(1); // 无重连
    expect(h.statuses.at(-1)).toBe("ready");
  });

  it("v1.40.1：visible 探活后 visibleProbeMs 内无任何入站帧 → 判死：teardown + 立即排重连", () => {
    const h = makeVisManager({ visibleProbeMs: 50 });
    h.mgr.connect();
    const sock = h.mine[0];
    sock.emit("open");
    sock.serverSend(HELLO_ACK);
    vi.advanceTimersByTime(5_000);
    goForeground();
    expect(sock.sentOfType("ping")).toHaveLength(1);
    vi.advanceTimersByTime(50); // 探活窗口内无任何回应 → 判死链路（半开）
    expect(sock.closedWith).toContain(1000); // 主动断开半开连接
    expect(h.statuses.at(-1)).toBe("reconnecting");
    vi.advanceTimersByTime(1_000); // 退避到点重连
    expect(h.mine).toHaveLength(2);
  });
});

// ---------------------------------------------------------------------------
// v1.40 语音转写 whisper 主路径：sendAsrRequest 发送器 + asr_result 连接层
// 拦截路由（onAsrResult 上抛，不进 onFrame/消息流）+ 注入面同规
// ---------------------------------------------------------------------------

describe("WsManager 语音转写（v1.40）", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("sendAsrRequest：ready 前返回 false；ready 后字节精确（mime 恒 audio/wav，machine 缺省不带字段）", () => {
    const h = makeManager();
    h.mgr.connect();
    expect(h.mgr.sendAsrRequest("QUJD", 100)).toBe(false);
    const sock = readyFlow(h);
    expect(h.mgr.sendAsrRequest("QUJD", 100)).toBe(true);
    expect(sock.sent.at(-1)).toBe(
      '{"v":1,"type":"asr_request","audio":"QUJD","mime":"audio/wav","durationMs":100}',
    );
    expect(h.mgr.sendAsrRequest("QUJD", 100, "home-mac")).toBe(true);
    expect(sock.sent.at(-1)).toBe(
      '{"v":1,"type":"asr_request","audio":"QUJD","mime":"audio/wav","durationMs":100,"machine":"home-mac"}',
    );
  });

  it("入站 asr_result：连接层拦截经 onAsrResult 上抛，不进 onFrame（不进消息流）", () => {
    const results: Array<{ text: string; durationMs: number }> = [];
    const frames: InboundFrame[] = [];
    const statuses: string[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      token: "tok-1",
      onFrame: (f) => frames.push(f),
      onStatus: (s) => statuses.push(s),
      onAuthFailed: () => {},
      onAsrResult: (f) => results.push({ text: f.text, durationMs: f.durationMs }),
      wsFactory: (u) => new FakeWebSocket(u),
    });
    mgr.connect();
    const sock = readyFlow({ mgr, statuses });
    sock.serverSend(
      '{"v":1,"type":"asr_result","text":"识别文本","durationMs":1234,"machine":"home-mac"}',
    );
    expect(results).toEqual([{ text: "识别文本", durationMs: 1234 }]);
    expect(frames).toHaveLength(0); // 连接层已拦截，不进消息流
  });

  it("asr_result 畸形帧（parseFrame 拒）静默丢弃：onAsrResult 不触发", () => {
    const results: unknown[] = [];
    const statuses: string[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      token: "tok-1",
      onFrame: () => {},
      onStatus: (s) => statuses.push(s),
      onAuthFailed: () => {},
      onAsrResult: (f) => results.push(f),
      wsFactory: (u) => new FakeWebSocket(u),
    });
    mgr.connect();
    const sock = readyFlow({ mgr, statuses });
    sock.serverSend('{"v":1,"type":"asr_result","text":"x"}'); // 缺 durationMs
    sock.serverSend('{"v":1,"type":"asr_result","text":"x","durationMs":-1}');
    expect(results).toHaveLength(0);
  });

  it("active 机器的明文 asr_result 属注入面：丢弃（onAsrResult 不触发）+ 降级告警（与 text/card 族同规）", () => {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    const events: E2eEvent[] = [];
    const statuses: string[] = [];
    const asrResults: unknown[] = [];
    const mgr = new WsManager({
      url: "wss://example.test/imagent/ws",
      token: "tok-1",
      onFrame: () => {},
      onStatus: (s) => statuses.push(s),
      onAuthFailed: () => {},
      onE2e: (e) => events.push(e),
      onAsrResult: (f) => asrResults.push(f),
      wsFactory: (u) => new FakeWebSocket(u),
      getMachine: () => "home-mac",
    });
    mgr.connect();
    const sock = readyFlow({ mgr, statuses });
    sock.serverSend(JSON.stringify({ v: 1, type: "machines", online: ["home-mac"] }));
    /** 网关按浏览器最新一帧 e2e_hello 应答 ack。 */
    const ackLatest = (): void => {
      const hello = JSON.parse(sock.sentOfType("e2e_hello").at(-1) ?? "null") as { ek: string; n1: string };
      const n2 = bytesToHex(randomBytes(32));
      const sig = bytesToHex(ed25519.sign(signatureMessage(hello.ek, gk, hello.n1, n2), idPriv));
      sock.serverSend(
        JSON.stringify({ v: 1, type: "e2e_ack", machine: "home-mac", gk, n2, sig, ik }),
      );
    };
    // ack（TOFU）→ 确认 → 以 pin 身份重新握手（v1.41.9）→ 第二帧 ack → active
    ackLatest();
    mgr.resolveTofu("home-mac", true);
    ackLatest();
    expect(events).toContainEqual({ kind: "active", machine: "home-mac" });
    sock.serverSend(
      '{"v":1,"type":"asr_result","text":"注入","durationMs":1,"machine":"home-mac"}',
    );
    expect(asrResults).toHaveLength(0); // 明文回包丢弃（网关回包恒 E2E 加密）
    expect(events).toContainEqual({ kind: "downgrade", machine: "home-mac" });
  });
});

// ---------------------------------------------------------------------------
// v1.41.3 会话删除：sendConvDelete 发送器 + enc 包装（业务帧族，同 message
// 族路由；快照/回执为网关回包，连接层无需拦截）
// ---------------------------------------------------------------------------

describe("WsManager 会话删除（v1.41.3）", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("sendConvDelete：ready 前返回 false；ready 后字节精确（machine 缺省不带字段）", () => {
    const h = makeManager();
    h.mgr.connect();
    expect(h.mgr.sendConvDelete("web:s-aa94")).toBe(false);
    const sock = readyFlow(h);
    expect(h.mgr.sendConvDelete("web:s-aa94")).toBe(true);
    expect(sock.sent.at(-1)).toBe('{"v":1,"type":"conv_delete","convId":"web:s-aa94"}');
    expect(h.mgr.sendConvDelete("peer:win-a", "home-mac")).toBe(true);
    expect(sock.sent.at(-1)).toBe(
      '{"v":1,"type":"conv_delete","convId":"peer:win-a","machine":"home-mac"}',
    );
  });

  it("conv_delete 在 BUSINESS_TYPES 内：e2e active 机器出站包成 enc 帧（relay 盲化）", () => {
    const h = makeManager();
    h.mgr.connect();
    const sock = readyFlow(h);
    // 走 e2e 握手至 active（同语音转写测试的 ack 流程）。
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    sock.serverSend(JSON.stringify({ v: 1, type: "machines", online: ["home-mac"] }));
    /** 网关按浏览器最新一帧 e2e_hello 应答 ack。 */
    const ackLatest = (): void => {
      const hello = JSON.parse(sock.sentOfType("e2e_hello").at(-1) ?? "null") as { ek: string; n1: string };
      const n2 = bytesToHex(randomBytes(32));
      const sig = bytesToHex(ed25519.sign(signatureMessage(hello.ek, gk, hello.n1, n2), idPriv));
      sock.serverSend(
        JSON.stringify({ v: 1, type: "e2e_ack", machine: "home-mac", gk, n2, sig, ik }),
      );
    };
    // ack（TOFU）→ 确认 → 以 pin 身份重新握手（v1.41.9）→ 第二帧 ack → active
    ackLatest();
    h.mgr.resolveTofu("home-mac", true);
    ackLatest();
    expect(h.mgr.sendConvDelete("web:s-aa94", "home-mac")).toBe(true);
    // 出站最外层是 enc 帧（原帧已加密，relay 不可见）。
    const wire = JSON.parse(sock.sent.at(-1) ?? "{}") as { type: string; machine: string };
    expect(wire.type).toBe("enc");
    expect(wire.machine).toBe("home-mac");
  });
});
