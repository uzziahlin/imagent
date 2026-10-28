// 组件层覆盖（M5 白屏回归）：挂载真实 App 组件 + FakeWebSocket（经
// stubGlobal 注入，WsManager 缺省 wsFactory 即 new WebSocket(url)），按真机
// relay 帧序喂帧：hello（出站）→ hello_ack → machine(online:false) →
// machines(["home-mac","work-mac"])。回归点：machines 帧先于该机器任何消息
// 帧到达时 spaces 尚无该机器键，App 的 allPending 派生不得因此炸掉整树渲染
// （React 18 渲染抛错 = 卸载整树 → #root 空白，真机曾复现）。
// v1.2 会话层：换发登录（USER_TOKEN 一次性）/ session 重连 / close 1008 三
// reason 分流 / 设备管理 UI / 旧 relay 兼容回退的端到端覆盖。
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
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
  // 还原被单测覆写的 navigator.userAgent（实例级 shadow）
  delete (navigator as { userAgent?: string }).userAgent;
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

// ---------------------------------------------------------------------------
// v1.2 会话层：换发登录 / session 重连 / close 1008 分流 / 设备管理 / 旧 relay
// ---------------------------------------------------------------------------

describe("App 会话层（v1.2）", () => {
  const DAY = 86_400_000;
  /** 会话签发 ack（hello_ack.session）。 */
  const ackWithSession = (id: string, token: string, expiresAt: number) =>
    JSON.stringify({
      v: 1,
      type: "hello_ack",
      role: "user",
      session: { id, token, expiresAt },
    });

  /** 登录页 UI 流程：输入 token（+可选设备名）→ 连接 → open，返回活跃 socket。 */
  function loginViaUi(token: string, device?: string): FakeWebSocket {
    render(<App />);
    fireEvent.change(screen.getByLabelText("访问令牌（USER_TOKEN）"), {
      target: { value: token },
    });
    if (device !== undefined) {
      fireEvent.change(screen.getByLabelText("设备名（可选）"), {
        target: { value: device },
      });
    }
    fireEvent.click(screen.getByRole("button", { name: "连接" }));
    const fake = FakeWebSocket.latest;
    if (!fake) {
      throw new Error("登录未创建 WebSocket 连接");
    }
    act(() => fake.emit("open"));
    return fake;
  }

  /** 预置有效会话后挂载（刷新恢复路径），返回活跃 socket（已 open）。 */
  function mountWithSession(expiresAt = Date.now() + 20 * DAY): FakeWebSocket {
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt }),
    );
    render(<App />);
    const fake = FakeWebSocket.latest;
    if (!fake) {
      throw new Error("App 未创建 WebSocket 连接");
    }
    act(() => fake.emit("open"));
    return fake;
  }

  it("换发登录：hello 带 token+device；签发 session 后 userToken 即删、会话持久化", () => {
    const fake = loginViaUi("tok-9", "我的 iPad");
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "hello", token: "tok-9", device: "我的 iPad" }),
    );
    // 登录瞬间 userToken 仍可恢复（relay 暂不可达时的重试窗口）
    expect(localStorage.getItem("imagent.userToken")).toBe("tok-9");
    act(() => fake.recv(JSON.parse(ackWithSession("sid-1", "st-1", Date.now() + 30 * DAY))));
    expect(screen.getByText("已连接")).toBeTruthy();
    const stored = JSON.parse(localStorage.getItem("imagent.session") ?? "null");
    expect(stored).toMatchObject({ id: "sid-1", token: "st-1" });
    // USER_TOKEN 一次性：签发即从 localStorage 删除
    expect(localStorage.getItem("imagent.userToken")).toBeNull();
  });

  it("登录设备名缺省 → hello 带UA摘要（iPhone/Safari）", () => {
    Object.defineProperty(navigator, "userAgent", {
      value:
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
      configurable: true,
    });
    const fake = loginViaUi("tok-9");
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "hello", token: "tok-9", device: "iPhone/Safari" }),
    );
  });

  it("刷新恢复：hello 用 session 形态；遗留 userToken 被清理", () => {
    localStorage.setItem("imagent.userToken", "legacy-token");
    const fake = mountWithSession();
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "hello", session: "st-r" }));
    expect(localStorage.getItem("imagent.userToken")).toBeNull();
    act(() => fake.recv(JSON.parse(ackWithSession("sid-r", "st-r", Date.now() + 20 * DAY))));
    expect(screen.getByText("已连接")).toBeTruthy();
  });

  it("close 1008 reason=session expired：清会话回登录页，提示「会话已过期」", () => {
    const fake = mountWithSession();
    act(() => {
      fake.emit("close", { code: 1008, reason: "session expired" });
    });
    expect(screen.getByRole("alert").textContent).toBe("会话已过期，请重新登录");
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("close 1008 reason=session revoked：回登录页提示「此设备已被吊销」", () => {
    const fake = mountWithSession();
    act(() => {
      fake.emit("close", { code: 1008, reason: "session revoked" });
    });
    expect(screen.getByRole("alert").textContent).toBe("此设备已被吊销");
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("close 1008 其它/缺席 reason：全清凭证回登录页、无特殊提示（v1.1 行为）", () => {
    const fake = loginViaUi("tok-9", "Mac/Chrome");
    act(() => fake.recv(JSON.parse(ackWithSession("sid-1", "st-1", Date.now() + 30 * DAY))));
    act(() => {
      fake.emit("close", { code: 1008, reason: "auth failed" });
    });
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(localStorage.getItem("imagent.session")).toBeNull();
    expect(localStorage.getItem("imagent.userToken")).toBeNull();
  });

  it("旧 relay 兼容：hello_ack 无 session → userToken 保留、无设备入口（现行模式）", () => {
    const fake = loginViaUi("tok-9", "Mac/Chrome");
    act(() => fake.recv({ v: 1, type: "hello_ack", role: "user" }));
    expect(screen.getByText("已连接")).toBeTruthy();
    expect(localStorage.getItem("imagent.userToken")).toBe("tok-9");
    expect(localStorage.getItem("imagent.session")).toBeNull();
    // 旧 relay 无会话层：设备管理入口与过期横幅均不出现
    expect(screen.queryByRole("button", { name: /已登录设备/ })).toBeNull();
    expect(screen.queryByText(/天后过期/)).toBeNull();
  });

  it("设备管理：打开发 device_list；devices 帧渲染列表（本机标记/相对时间）；吊销其它设备", () => {
    const fake = mountWithSession();
    act(() => fake.recv(JSON.parse(ackWithSession("sid-r", "st-r", Date.now() + 20 * DAY))));
    // 打开设备面板 → 发 device_list
    fireEvent.click(screen.getByRole("button", { name: "已登录设备" }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "device_list" }));
    expect(screen.getByRole("dialog", { name: "已登录设备" })).toBeTruthy();
    expect(screen.getByText("加载中…")).toBeTruthy();
    act(() =>
      fake.recv({
        v: 1,
        type: "devices",
        devices: [
          { id: "sid-r", name: "Mac/Chrome", connected: true, lastSeen: Date.now() - 30_000 },
          { id: "sid-x", name: "iPhone/Safari", connected: false, lastSeen: Date.now() - 2 * 3_600_000 },
        ],
      }),
    );
    expect(screen.getByText("Mac/Chrome")).toBeTruthy();
    expect(screen.getByText("iPhone/Safari")).toBeTruthy();
    expect(screen.getByText("本机")).toBeTruthy();
    expect(screen.getByText(/刚刚/)).toBeTruthy();
    expect(screen.getByText(/2 小时前/)).toBeTruthy();
    // 吊销其它设备 → 发 device_revoke → relay 回更新列表 → 该设备消失
    fireEvent.click(screen.getByRole("button", { name: "吊销" }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "device_revoke", deviceId: "sid-x" }));
    act(() =>
      fake.recv({
        v: 1,
        type: "devices",
        devices: [{ id: "sid-r", name: "Mac/Chrome", connected: true, lastSeen: Date.now() }],
      }),
    );
    expect(screen.queryByText("iPhone/Safari")).toBeNull();
    expect(screen.getByText("本机")).toBeTruthy();
  });

  it("吊销本机 = 登出：发 device_revoke 后清凭证回登录页", () => {
    const fake = mountWithSession();
    act(() => fake.recv(JSON.parse(ackWithSession("sid-r", "st-r", Date.now() + 20 * DAY))));
    fireEvent.click(screen.getByRole("button", { name: "已登录设备" }));
    act(() =>
      fake.recv({
        v: 1,
        type: "devices",
        devices: [{ id: "sid-r", name: "Mac/Chrome", connected: true, lastSeen: Date.now() }],
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "吊销并退出" }));
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "device_revoke", deviceId: "sid-r" }),
    );
    expect(screen.getByRole("alert").textContent).toBe("本机设备已吊销，请重新登录");
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("会话剩余 <3 天：侧栏底部过期横幅（点击登出回登录页）；29 天不提示", () => {
    const fake = mountWithSession(Date.now() + 2 * DAY);
    act(() => fake.recv(JSON.parse(ackWithSession("sid-r", "st-r", Date.now() + 2 * DAY))));
    const banner = screen.getByRole("button", { name: /天后过期，重新登录可续期/ });
    expect(banner.textContent).toMatch(/会话 2 天后过期/);
    fireEvent.click(banner);
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("会话剩余 29 天：不出现过期横幅（设备入口仍在）", () => {
    const fake = mountWithSession(Date.now() + 29 * DAY);
    act(() => fake.recv(JSON.parse(ackWithSession("sid-r", "st-r", Date.now() + 29 * DAY))));
    expect(screen.queryByText(/天后过期/)).toBeNull();
    expect(screen.getByRole("button", { name: "已登录设备" })).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// 交互体验批：断线 composer 置灰 / 未读徽标 / 空态示例 prompt / 审批弹层
// ---------------------------------------------------------------------------

describe("断线 composer 置灰（交互细节）", () => {
  /** 挂载 + 握手到 ready（单机模式即可），返回活跃 socket。 */
  function mountReady(): FakeWebSocket {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    return fake;
  }

  it("ready 可输入；断线（reconnecting）后禁用 + 占位「连接中断，恢复后可发送」", () => {
    const fake = mountReady();
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    expect(ta.disabled).toBe(false);
    expect(ta.placeholder).toBe("发消息给 Mac…（Enter 发送）");

    act(() => {
      fake.emit("close", { code: 1006 });
    });
    const disabledTa = screen.getByPlaceholderText("连接中断，恢复后可发送") as HTMLTextAreaElement;
    expect(disabledTa.disabled).toBe(true);
    // 发送 / 停止按钮同步禁用
    expect((screen.getByRole("button", { name: "发送" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "停止" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("superseded（4001 顶替）：同样禁用并提示；点「重新连接」换新连接后恢复", () => {
    const fake = mountReady();
    act(() => {
      fake.emit("close", { code: 4001 });
    });
    expect(screen.getByText("已在其它窗口连接")).toBeTruthy();
    const ta = screen.getByPlaceholderText("已在其它窗口连接，重新连接后可发送") as HTMLTextAreaElement;
    expect(ta.disabled).toBe(true);

    // 用户显式重连：复用 WsManager 新建连接（新 FakeWebSocket 实例）
    fireEvent.click(screen.getByRole("button", { name: "重新连接" }));
    const next = FakeWebSocket.latest;
    expect(next).toBeTruthy();
    expect(next).not.toBe(fake);
    act(() => {
      next!.emit("open");
      next!.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    const restored = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    expect(restored.disabled).toBe(false);
  });
});

describe("未读徽标与空态示例（交互细节）", () => {
  it("非当前会话收新消息 → 侧栏徽标计数；点选该会话清零", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);

    // 当前会话（web:default）消息不产生徽标
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:default", text: "当前会话消息", machine: "home-mac" });
    });
    const defaultBtn = screen.getByRole("button", { name: /default/ });
    expect(within(defaultBtn).queryByText(/条未读/)).toBeNull();

    // 非当前会话（web:work）消息 → 徽标 1；再来一条 → 2
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台第一条", machine: "home-mac" });
    });
    let workBtn = screen.getByRole("button", { name: /work/ });
    expect(within(workBtn).getByText("1")).toBeTruthy();
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台第二条", machine: "home-mac" });
    });
    workBtn = screen.getByRole("button", { name: /work/ });
    expect(within(workBtn).getByText("2")).toBeTruthy();

    // 点选 → 清零并切到该会话（aria-current 标记选中）
    fireEvent.click(workBtn);
    workBtn = screen.getByRole("button", { name: /work/ });
    expect(within(workBtn).queryByText("1")).toBeNull();
    expect(within(workBtn).queryByText("2")).toBeNull();
    expect(workBtn.getAttribute("aria-current")).toBe("page");
  });

  it("空态示例 prompt：点击即发送 message 帧 + 本地回显", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const sample = screen.getByRole("button", { name: "看看这个项目结构" });
    fireEvent.click(sample);
    expect(fake.sent).toContain(
      JSON.stringify({
        v: 1,
        type: "message",
        convId: "web:default",
        text: "看看这个项目结构",
        machine: "home-mac",
      }),
    );
    // 本地用户回显出现：气泡 + 侧栏预览各一处
    expect(screen.getAllByText("看看这个项目结构").length).toBe(2);
    // 发送后空态消失（有条目）
    expect(screen.queryByRole("button", { name: "帮我写个脚本" })).toBeNull();
  });

  it("断线时空态示例 prompt 不出现（无法发送）", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
      fake.emit("close", { code: 1006 });
    });
    expect(screen.queryByRole("button", { name: "看看这个项目结构" })).toBeNull();
  });
});

describe("审批弹层（slide-in + 移动端触觉）", () => {
  it("approval_request → 弹层渲染（不炸 vibrate）；拒绝回传 approval_response", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv({
        v: 1,
        type: "approval_request",
        convId: "web:default",
        requestId: "r1",
        tool: "Bash",
        inputSummary: "rm -rf /tmp/x",
        machine: "home-mac",
      });
    });
    expect(screen.getByRole("alertdialog", { name: "审批请求 Bash" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "拒绝" }));
    expect(fake.sent).toContain(
      JSON.stringify({
        v: 1,
        type: "approval_response",
        convId: "web:default",
        requestId: "r1",
        decision: "deny",
        machine: "home-mac",
      }),
    );
  });
});
