// 组件层覆盖（M5 白屏回归）：挂载真实 App 组件 + FakeWebSocket（经
// stubGlobal 注入，WsManager 缺省 wsFactory 即 new WebSocket(url)），按真机
// relay 帧序喂帧：hello（出站）→ hello_ack → machine(online:false) →
// machines(["home-mac","work-mac"])。回归点：machines 帧先于该机器任何消息
// 帧到达时 spaces 尚无该机器键，App 的 allPending 派生不得因此炸掉整树渲染
// （React 18 渲染抛错 = 卸载整树 → #root 空白，真机曾复现）。
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import type { WsEvent } from "./lib/ws";

/** 可控 fake WebSocket：手动 emit 事件、记录出站帧（形状对齐 ws.test.ts）。 */
class FakeWebSocket {
  static latest: FakeWebSocket | null = null;
  readyState = 0;
  sent: string[] = [];
  private listeners = new Map<string, Set<(ev: WsEvent) => void>>();

  constructor(public url: string) {
    FakeWebSocket.latest = this;
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

  close(): void {
    this.readyState = 3;
  }

  emit(type: "open" | "close" | "error", ev: WsEvent = {}): void {
    if (type === "open") {
      this.readyState = 1;
    } else if (type === "close") {
      this.readyState = 3;
    }
    for (const l of this.listeners.get(type) ?? []) {
      l(ev);
    }
  }

  /** 收一条入站帧（JSON 序列化后走 message 事件，同真实 socket）。 */
  recv(frame: unknown): void {
    const data = JSON.stringify(frame);
    for (const l of this.listeners.get("message") ?? []) {
      l({ data });
    }
  }
}

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

/** 挂载 App（token 预置登录态）并完成连接握手，返回活跃 fake socket。 */
function mountApp(): FakeWebSocket {
  localStorage.setItem("imagent.userToken", "test-token");
  render(<App />);
  const fake = FakeWebSocket.latest;
  if (!fake) {
    throw new Error("App 未创建 WebSocket 连接");
  }
  return fake;
}

/** 真机 relay 帧序：hello_ack → machine → machines。 */
function handshake(fake: FakeWebSocket, machines: string[]): void {
  act(() => {
    fake.emit("open");
    fake.recv({ v: 1, type: "hello_ack", role: "user" });
    fake.recv({ v: 1, type: "machine", online: false });
    fake.recv({ v: 1, type: "machines", online: machines });
  });
}

beforeEach(() => {
  vi.stubGlobal("WebSocket", FakeWebSocket);
  FakeWebSocket.latest = null;
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  localStorage.clear();
});

describe("App 挂载 + machines 帧序列（M5 多机白屏回归）", () => {
  it("hello 出站帧带 token；hello_ack 后 ready 并发无 machine 的 sync", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
    });
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "hello", token: "test-token" }));
    act(() => {
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "sync" }));
    expect(screen.getByText("已连接")).toBeTruthy();
  });

  it("两台网关在线：首收 machines 帧不崩溃，机器切换器出现且自动选中首台", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);

    // 机器切换器（Sidebar role=listbox）与两台机器按钮均渲染
    expect(screen.getByRole("listbox", { name: "机器列表" })).toBeTruthy();
    expect(screen.getByRole("button", { name: /home-mac/ })).toBeTruthy();
    expect(screen.getByRole("button", { name: /work-mac/ })).toBeTruthy();
    expect(screen.getByText("2 台在线")).toBeTruthy();

    // 自动切换到首台在线机器（StatusBar + 切换通知 toast）
    expect(screen.getByText("home-mac 在线")).toBeTruthy();
    expect(screen.getByText("已连接机器 home-mac")).toBeTruthy();

    // 机器维度 effect：补发一次带当前机器的 sync
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "sync", machine: "home-mac" }));
  });

  it("machines 空列表（网关全下线）：不崩溃，渲染空态且保持连接页", () => {
    const fake = mountApp();
    handshake(fake, []);

    expect(screen.getByRole("listbox", { name: "机器列表" })).toBeTruthy();
    expect(screen.getByText("（无在线机器）")).toBeTruthy();
    expect(screen.getByText("0 台在线")).toBeTruthy();
  });

  it("machines 帧先到、该机器消息帧后到：card 落对应机器空间正常渲染", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv({
        v: 1,
        type: "card",
        convId: "web:default",
        messageId: "m1",
        machine: "home-mac",
        card: {
          text: "hello from home-mac",
          tool_calls: [],
          thoughts: [],
          todos: [],
          phase: "outputting",
          run_secs: 1,
          terminal: "done",
        },
        state: "done",
      });
    });
    // 正文（markdown p）与侧栏预览（span）两处均应渲染出来
    expect(screen.getAllByText(/hello from home-mac/).length).toBeGreaterThanOrEqual(2);
  });
});
