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

  function makeE2eManager(machine: string): E2eHarness {
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
      wsFactory: (u) => new FakeWebSocket(u),
      getMachine: () => machine,
    });
    return { mgr, frames, events, statuses };
  }

  /** ready 后推 machines 快照（触发 e2e 握手）。 */
  function pushMachines(sock: FakeWebSocket, online: string[]): void {
    sock.serverSend(JSON.stringify({ v: 1, type: "machines", online }));
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

    // ack → TOFU 事件；确认 → active
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
    h.mgr.resolveTofu("home-mac", true);
    expect(h.events).toContainEqual({ kind: "active", machine: "home-mac" });

    // 出站：enc 包装，网关密钥可解回原帧
    expect(h.mgr.sendMessage("web:default", "机密", "home-mac")).toBe(true);
    const encSent = sock.sentOfType("enc");
    expect(encSent).toHaveLength(1);
    const enc = JSON.parse(encSent[0]) as { machine: string; box: string };
    expect(enc.machine).toBe("home-mac");
    const gwKey = gw.gatewayKey(hello, ack.n2);
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
    const ack = gw.ackFor(hello, "home-mac");
    sock.serverSend(JSON.stringify(ack));
    h.mgr.resolveTofu("home-mac", true);
    expect(h.events).toContainEqual({ kind: "active", machine: "home-mac" });
    const gwKey = gw.gatewayKey(hello, ack.n2);

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
    const ack = gw.ackFor(hello, "home-mac");
    sock.serverSend(JSON.stringify(ack));
    h.mgr.resolveTofu("home-mac", true);

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
    h.mgr.resolveTofu("home-mac", true);
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
    const ack = gw.ackFor(hello, "home-mac");
    sock.serverSend(JSON.stringify(ack));
    h.mgr.resolveTofu("home-mac", true);
    vi.advanceTimersByTime(25_000);
    const ping = sock.sent.at(-1) ?? "{}";
    expect(JSON.parse(ping).type).toBe("enc");
    expect(openBox(gw.gatewayKey(hello, ack.n2), JSON.parse(ping).box)).toBe(
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
