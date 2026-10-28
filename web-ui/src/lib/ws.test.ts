import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WsManager, type WsEvent, type WebSocketLike } from "./ws";
import type { InboundFrame } from "./protocol";

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
function readyFlow(h: Harness): FakeWebSocket {
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
