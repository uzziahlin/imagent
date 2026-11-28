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
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, hexToBytes, randomBytes, utf8ToBytes } from "@noble/hashes/utils.js";
import App from "./App";
import { __resetAdminToken } from "./lib/admin";
import { historyStore, memoryHistoryBackend, setHistoryBackend } from "./lib/historyStore";
import { fingerprint, openBox, sealFrame } from "./lib/e2e";
import { ACK_TIMEOUT_MS, ACK_TIMEOUT_TICK_MS, type InboundFrame, type WindowInfo } from "./lib/protocol";
import { WsManager, type WsEvent } from "./lib/ws";

/** 可控 fake WebSocket：手动 emit 事件、记录出站帧（形状对齐 ws.test.ts）。 */
class FakeWebSocket {
  static latest: FakeWebSocket | null = null;
  readyState = 0;
  sent: string[] = [];
  closedWith: number[] = [];
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

  close(code?: number): void {
    this.readyState = 3;
    this.closedWith.push(code ?? 1000);
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

/** 明文 message 帧断言（v1.44 起每帧带本机新生成的 clientId，逐帧不同）：出站
 *  帧里有一帧去掉 clientId 后与 expected 逐字节相等，且 clientId 是非空字符串
 *  （clientId 的形状由 v1.44 用例单独钉）。 */
function sentMessage(fake: FakeWebSocket, expected: Record<string, unknown>): boolean {
  return fake.sent.some((raw) => {
    const { clientId, ...rest } = JSON.parse(raw) as Record<string, unknown>;
    return typeof clientId === "string" && clientId !== "" && JSON.stringify(rest) === JSON.stringify(expected);
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
    expect(screen.queryByRole("button", { name: "管理" })).toBeNull();
    expect(screen.queryByText(/天后过期/)).toBeNull();
  });

  it("设备管理：打开发 device_list；devices 帧渲染列表（本机标记/相对时间）；吊销其它设备", () => {
    const fake = mountWithSession();
    act(() => fake.recv(JSON.parse(ackWithSession("sid-r", "st-r", Date.now() + 20 * DAY))));
    // 打开设备面板 → 发 device_list
    fireEvent.click(screen.getByRole("button", { name: "管理" }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "device_list" }));
    expect(screen.getByRole("dialog", { name: "管理" })).toBeTruthy();
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
    fireEvent.click(screen.getByRole("button", { name: "管理" }));
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
    expect(screen.getByRole("button", { name: "管理" })).toBeTruthy();
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
    expect(ta.placeholder).toBe("发消息给 Mac…（Enter 发送，/ 唤起命令）");

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

    // 当前会话（web:default）消息不产生徽标。v1.7.x 起 default 会话发首条
    // 消息后显示名改为派生 title——按消息文本定位该会话按钮。
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:default", text: "当前会话消息", machine: "home-mac" });
    });
    const defaultBtn = screen.getByRole("button", { name: /当前会话消息/ });
    expect(within(defaultBtn).queryByText(/条未读/)).toBeNull();

    // 非当前会话（web:work）消息 → 徽标 1；再来一条 → 2
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台第一条", machine: "home-mac" });
    });
    let workBtn = screen.getByRole("button", { name: /^work/ });
    expect(within(workBtn).getByText("1")).toBeTruthy();
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台第二条", machine: "home-mac" });
    });
    workBtn = screen.getByRole("button", { name: /^work/ });
    expect(within(workBtn).getByText("2")).toBeTruthy();

    // 点选 → 清零并切到该会话（aria-current 标记选中）
    fireEvent.click(workBtn);
    workBtn = screen.getByRole("button", { name: /^work/ });
    expect(within(workBtn).queryByText("1")).toBeNull();
    expect(within(workBtn).queryByText("2")).toBeNull();
    expect(workBtn.getAttribute("aria-current")).toBe("page");
  });

  it("空态示例 prompt：点击即发送 message 帧 + 本地回显", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const sample = screen.getByRole("button", { name: "看看这个项目结构" });
    fireEvent.click(sample);
    expect(
      sentMessage(fake, {
        v: 1,
        type: "message",
        convId: "web:default",
        text: "看看这个项目结构",
        machine: "home-mac",
      }),
    ).toBe(true);
    // 本地用户回显出现：气泡 + 侧栏预览各一处；default 会话首条消息还会作为
    // 自动命名 title 出现在侧栏标题/状态条（v1.7.x 起 default 也自动命名）。
    expect(screen.getAllByText("看看这个项目结构").length).toBeGreaterThanOrEqual(2);
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

// ---------------------------------------------------------------------------
// v1.3 passkey 登录（mock WebAuthn API + fetch）：成功存 session 进入 /
// 失败回落提示 / USER_TOKEN 路径不回归
// ---------------------------------------------------------------------------

describe("App passkey 登录（v1.3）", () => {
  /** 开启 WebAuthn 能力（jsdom 默认无）：PublicKeyCredential 全局 +
   *  navigator.credentials（get/create 由用例给实现）。 */
  function enableWebauthn(get: () => Promise<unknown>): void {
    vi.stubGlobal("PublicKeyCredential", class PublicKeyCredential {});
    Object.defineProperty(navigator, "credentials", {
      value: {
        get,
        create: async () => {
          throw new Error("本用例不触发 create");
        },
      },
      configurable: true,
    });
  }

  /** 伪造浏览器 assertion（字段为 ArrayBuffer/Uint8Array，lib/webauthn 转换）。 */
  function fakeAssertion(): unknown {
    const enc = (s: string) => new TextEncoder().encode(s);
    return {
      id: "cred-1",
      rawId: enc("raw-cred"),
      type: "public-key",
      response: {
        authenticatorData: enc("auth-data"),
        clientDataJSON: enc("{}"),
        signature: enc("sig"),
        userHandle: enc("user"),
      },
    };
  }

  /** login begin/complete 的 fetch mock。 */
  function webauthnFetch(beginStatus = 200, completeStatus = 200): ReturnType<typeof vi.fn> {
    return vi.fn(async (url: string | URL | Request) => {
      const u = typeof url === "string" ? url : url.toString();
      if (u.includes("webauthn/login/begin")) {
        if (beginStatus !== 200) {
          return new Response(JSON.stringify({ error: "no_credentials" }), { status: beginStatus });
        }
        return new Response(
          JSON.stringify({
            beginId: "begin-1",
            publicKey: {
              rpId: "localhost",
              challenge: "Zm9vYmFy", // base64url("foobar")
              allowCredentials: [],
            },
          }),
          { status: 200 },
        );
      }
      if (u.includes("webauthn/login/complete")) {
        if (completeStatus !== 200) {
          return new Response(JSON.stringify({ error: "verification_failed" }), {
            status: completeStatus,
          });
        }
        return new Response(
          JSON.stringify({
            session: { id: "sid-pk", token: "tok-pk", expiresAt: Date.now() / 1000 + 30 * 86_400 },
          }),
          { status: 200 },
        );
      }
      return new Response("{}", { status: 404 });
    }) as unknown as ReturnType<typeof vi.fn>;
  }

  it("成功：begin → credentials.get → complete 存 session，hello 用 session 形态直连", async () => {
    const fetchMock = webauthnFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    render(<App />);

    // 登录页双入口：通行钥匙页签默认高亮，主按钮可用。
    expect(screen.getByRole("tab", { name: "使用通行钥匙" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "使用通行钥匙登录" }));
    });

    // begin/complete 各一次；session 持久化。
    expect(fetchMock.mock.calls.filter((c) => c[0].toString().includes("login/begin"))).toHaveLength(1);
    expect(fetchMock.mock.calls.filter((c) => c[0].toString().includes("login/complete"))).toHaveLength(1);
    const stored = JSON.parse(localStorage.getItem("imagent.session") ?? "null");
    expect(stored).toMatchObject({ id: "sid-pk", token: "tok-pk" });

    // WS hello 直接带 sessionToken（与 USER_TOKEN 登录换发后同构）。
    const fake = FakeWebSocket.latest;
    expect(fake).toBeTruthy();
    act(() => fake!.emit("open"));
    expect(fake!.sent).toContain(JSON.stringify({ v: 1, type: "hello", session: "tok-pk" }));
  });

  it("失败（no_credentials）：留在登录页并提示回落访问令牌；切换页签可继续 USER_TOKEN 登录", async () => {
    const fetchMock = webauthnFetch(400);
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    render(<App />);

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "使用通行钥匙登录" }));
    });
    // 失败提示（alert）出现，登录页仍在（token 入口经页签可达）。
    expect(screen.getByRole("alert").textContent).toContain("尚未注册通行钥匙");
    expect(screen.getByRole("tab", { name: "使用访问令牌" })).toBeTruthy();

    // 切到访问令牌页签 → 表单出现 → USER_TOKEN 路径照常。
    fireEvent.click(screen.getByRole("tab", { name: "使用访问令牌" }));
    fireEvent.change(screen.getByLabelText("访问令牌（USER_TOKEN）"), {
      target: { value: "tok-9" },
    });
    fireEvent.click(screen.getByRole("button", { name: "连接" }));
    const fake = FakeWebSocket.latest;
    expect(fake).toBeTruthy();
    act(() => fake!.emit("open"));
    // 未填设备名 → hello 带缺省 UA 摘要 device；token 字段才是断言重点。
    const hello = JSON.parse(fake!.sent[0] ?? "{}") as { type?: string; token?: string };
    expect(hello).toMatchObject({ v: 1, type: "hello", token: "tok-9" });
  });

  it("验签失败（401）：提示验证失败，不进入主界面", async () => {
    const fetchMock = webauthnFetch(200, 401);
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    render(<App />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "使用通行钥匙登录" }));
    });
    expect(screen.getByRole("alert").textContent).toContain("通行钥匙验证失败");
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("WebAuthn 不可用（jsdom 默认）：无页签、直接令牌表单（USER_TOKEN 路径不回归）", () => {
    render(<App />);
    expect(screen.queryByRole("tablist", { name: "登录方式" })).toBeNull();
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(screen.getByRole("button", { name: "连接" })).toBeTruthy();
  });

  it("设备管理通行钥匙区：打开即发 cred_list；creds 帧渲染列表；删除发 cred_revoke", () => {
    // 通行钥匙列表仅在 WebAuthn 可用时渲染（不支持时显示指引）——先开启能力。
    enableWebauthn(async () => fakeAssertion());
    // 本 describe 内的局部挂载/ack 辅助（v1.2 describe 的同名函数不可跨块用）。
    const ack = (id: string, token: string, expiresAt: number) =>
      JSON.stringify({ v: 1, type: "hello_ack", role: "user", session: { id, token, expiresAt } });
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 }),
    );
    render(<App />);
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.recv(JSON.parse(ack("sid-r", "st-r", Date.now() + 20 * 86_400_000)));
    });
    fireEvent.click(screen.getByRole("button", { name: "管理" }));
    // 打开面板：device_list + cred_list 都发出。
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "cred_list" }));
    expect(screen.getByRole("dialog", { name: "管理" })).toBeTruthy();
    // 通行钥匙区：待回包态。
    expect(screen.getByText("通行钥匙")).toBeTruthy();
    act(() => {
      fake.recv({
        v: 1,
        type: "creds",
        credentials: [{ id: "cred-1", label: "iPhone 17", created: Math.floor(Date.now() / 1000) - 120 }],
      });
    });
    expect(screen.getByText("iPhone 17")).toBeTruthy();
    expect(screen.getByText(/注册于/)).toBeTruthy();
    // 删除 → cred_revoke → relay 回空列表。
    fireEvent.click(screen.getByRole("button", { name: "删除" }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "cred_revoke", credId: "cred-1" }));
    act(() => fake.recv({ v: 1, type: "creds", credentials: [] }));
    expect(screen.getByText(/尚未注册通行钥匙/)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// v1.4 E2E：TOFU 指纹确认弹层 / 身份变化红条 / 明文降级开关 / 设备管理指纹区
// / 降级告警横幅（真实 @noble 密码学，FakeWebSocket 扮演 relay+网关）
// ---------------------------------------------------------------------------

describe("App E2E（v1.4）", () => {
  /** 测试网关：长期 X25519 + ed25519 身份密钥（与 lib/e2e.test 同构）。 */
  function makeGateway() {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    const ackFor = (hello: { ek: string; n1: string }, machine: string, opts?: { ik?: string }) => {
      const n2 = bytesToHex(randomBytes(32));
      const msg = new Uint8Array(128);
      msg.set(hexToBytes(hello.ek), 0);
      msg.set(hexToBytes(gk), 32);
      msg.set(hexToBytes(hello.n1), 64);
      msg.set(hexToBytes(n2), 96);
      return {
        v: 1 as const,
        type: "e2e_ack" as const,
        machine,
        gk,
        n2,
        sig: bytesToHex(ed25519.sign(msg, idPriv)),
        ik: opts?.ik ?? ik,
      };
    };
    const gatewayKey = (hello: { ek: string; n1: string }, n2: string) => {
      const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
      const ikm = new Uint8Array(96);
      ikm.set(shared, 0);
      ikm.set(hexToBytes(hello.n1), 32);
      ikm.set(hexToBytes(n2), 64);
      return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
    };
    return { gk, ik, ackFor, gatewayKey };
  }

  /** 从 fake.sent 里取最近一条 e2e_hello（无则抛）。 */
  function lastHello(fake: FakeWebSocket): { ek: string; n1: string } {
    const raw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").at(-1);
    if (!raw) {
      throw new Error("尚未发出 e2e_hello");
    }
    return JSON.parse(raw);
  }

  /** machines 快照 → 网关 ack（返回 ack 供后续派生网关密钥）。 */
  function ackHandshake(fake: FakeWebSocket, gw: ReturnType<typeof makeGateway>, machine: string) {
    act(() => {
      fake.recv({ v: 1, type: "machines", online: [machine] });
    });
    const hello = lastHello(fake);
    const ack = gw.ackFor(hello, machine);
    act(() => {
      fake.recv(ack);
    });
    return { hello, ack };
  }

  it("TOFU 首连：指纹弹层（核对文案 + 分组指纹）→ 信任 → pin 落库 + 锁标 + 出站 enc", () => {
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    const { hello, ack } = ackHandshake(fake, gw, "home-mac");

    // 指纹确认弹层：机器名、核对文案、两枚分组指纹
    const dialog = screen.getByRole("dialog", { name: "确认机器加密指纹" });
    expect(within(dialog).getByText("home-mac")).toBeTruthy();
    expect(within(dialog).getByText(/imagent start/)).toBeTruthy();
    expect(within(dialog).getByText(fingerprint(gw.gk))).toBeTruthy();
    expect(within(dialog).getByText(fingerprint(gw.ik))).toBeTruthy();

    fireEvent.click(within(dialog).getByRole("button", { name: "信任并启用加密" }));
    // v1.41.9：信任 = 落 pin + 以 pin 身份重新握手（弹层期间网关侧的会话可能
    // 已被半开清理，不沿用其密钥）——新握手的 ack 到达前不算激活：无锁标、
    // 弹层仍驻（App 逻辑未改，靠 active 事件关弹层）
    expect(JSON.parse(localStorage.getItem("imagent.e2ePins") ?? "{}")).toHaveProperty("home-mac");
    expect(screen.queryByText("已加密")).toBeNull();
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();

    // 网关应答重新握手的 e2e_hello → StatusBar 锁标 + 弹层关闭
    const redone = lastHello(fake);
    const reack = gw.ackFor(redone, "home-mac");
    act(() => {
      fake.recv(reack);
    });
    expect(screen.getByText("已加密")).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: "确认机器加密指纹" })).toBeNull();

    // 发送：出站为 enc 帧，网关用**重新握手**的密钥可解回原帧（弹层期间的
    // 旧密钥解不开——密钥已换代）
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "机密指令" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    const encRaw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "enc").at(-1);
    expect(encRaw).toBeTruthy();
    const enc = JSON.parse(encRaw!) as { machine: string; box: string };
    expect(enc.machine).toBe("home-mac");
    // v1.44：内帧带本机生成的 clientId（送达回执的关联 id，形状同 newClientId）
    const inner = JSON.parse(openBox(gw.gatewayKey(redone, reack.n2), enc.box) ?? "null") as Record<string, unknown>;
    expect(inner).toMatchObject({ v: 1, type: "message", convId: "web:default", text: "机密指令", machine: "home-mac" });
    expect(inner.clientId).toMatch(/^m_[0-9a-z]+_[0-9a-f]{8}$/);
    // 弹层期间的旧密钥解不开
    expect(openBox(gw.gatewayKey(hello, ack.n2), enc.box)).toBeNull();
  });

  it("TOFU 拒绝 → fail-closed 横幅 + composer 禁用；「仍以明文连接」恢复明文发送", () => {
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    ackHandshake(fake, gw, "home-mac");
    fireEvent.click(screen.getByRole("button", { name: "不信任" }));

    // 不自动明文降级：横幅 + 发送禁用
    expect(screen.getByText(/不支持加密连接/)).toBeTruthy();
    const ta = screen.getByPlaceholderText("加密连接未确认，暂不能发送（见上方提示）") as HTMLTextAreaElement;
    expect(ta.disabled).toBe(true);
    expect((screen.getByRole("button", { name: "发送" }) as HTMLButtonElement).disabled).toBe(true);

    // 显式按钮：放行并恢复明文
    fireEvent.click(screen.getByRole("button", { name: "仍以明文连接" }));
    const restored = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    expect(restored.disabled).toBe(false);
    fireEvent.change(restored, { target: { value: "明文" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(sentMessage(fake, { v: 1, type: "message", convId: "web:default", text: "明文", machine: "home-mac" })).toBe(true);
    // 选择被持久（下次连接不再握手）——检查 localStorage 记录
    expect(JSON.parse(localStorage.getItem("imagent.e2ePlaintext") ?? "{}")).toEqual({ "home-mac": true });
  });

  it("身份变化：已 pin 公钥改变 → 红色告警 + 发送禁用", () => {
    const pinned = makeGateway();
    localStorage.setItem(
      "imagent.e2ePins",
      JSON.stringify({ "home-mac": { ik: pinned.ik, gk: pinned.gk, pinnedAt: 1 } }),
    );
    const mitm = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    // 中间人自有密钥应答、冒充 pin 的身份公钥
    act(() => {
      fake.recv({ v: 1, type: "machines", online: ["home-mac"] });
    });
    const hello = lastHello(fake);
    act(() => {
      fake.recv(mitm.ackFor(hello, "home-mac", { ik: pinned.ik }));
    });
    expect(screen.getAllByRole("alert").some((el) => el.textContent?.includes("身份变化"))).toBe(true);
    const ta = screen.getByPlaceholderText("加密连接未确认，暂不能发送（见上方提示）") as HTMLTextAreaElement;
    expect(ta.disabled).toBe(true);
  });

  it("设备管理指纹区：pin 列表（双指纹分组）+ 重置 → 清 pin 并重新握手", () => {
    const gw = makeGateway();
    // 会话模式（设备管理入口需要 session）
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 }),
    );
    render(<App />);
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.recv({
        v: 1,
        type: "hello_ack",
        role: "user",
        session: { id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 },
      });
    });
    ackHandshake(fake, gw, "home-mac");
    fireEvent.click(screen.getByRole("button", { name: "信任并启用加密" }));
    // v1.41.9：信任后以 pin 身份重新握手，网关应答才激活（指纹区快照随
    // active 事件刷新）
    act(() => {
      fake.recv(gw.ackFor(lastHello(fake), "home-mac"));
    });

    fireEvent.click(screen.getByRole("button", { name: "管理" }));
    const dialog = screen.getByRole("dialog", { name: "管理" });
    expect(within(dialog).getByText("已信任机器指纹")).toBeTruthy();
    expect(within(dialog).getByText("home-mac")).toBeTruthy();
    expect(within(dialog).getByText(`X25519 ${fingerprint(gw.gk)}`)).toBeTruthy();
    expect(within(dialog).getByText(`Ed25519 ${fingerprint(gw.ik)}`)).toBeTruthy();

    // 重置：清 pin + 重新发起握手（新 e2e_hello）
    const hellosBefore = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").length;
    fireEvent.click(within(dialog).getByRole("button", { name: "重置" }));
    expect(JSON.parse(localStorage.getItem("imagent.e2ePins") ?? "{}")).toEqual({});
    const hellosAfter = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").length;
    expect(hellosAfter).toBe(hellosBefore + 1);
    // 网关再 ack → 重新走 TOFU 弹层
    const hello = lastHello(fake);
    act(() => {
      fake.recv(gw.ackFor(hello, "home-mac"));
    });
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();
  });

  it("降级告警：握手完成后收到明文业务帧 → 黄条告警且内容不落消息流", () => {
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    ackHandshake(fake, gw, "home-mac");
    fireEvent.click(screen.getByRole("button", { name: "信任并启用加密" }));
    // v1.41.9：信任后以 pin 身份重新握手，网关应答才 active（握手完成）
    act(() => {
      fake.recv(gw.ackFor(lastHello(fake), "home-mac"));
    });
    // 网关方向明文 text 帧（半开会话超时回退场景）
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:default", text: "降级内容", machine: "home-mac" });
    });
    expect(screen.getAllByRole("alert").some((el) => el.textContent?.includes("收到未加密业务帧"))).toBe(true);
    expect(screen.queryByText("降级内容")).toBeNull(); // 帧已丢弃
  });
});

// ---------------------------------------------------------------------------
// v1.5 准入与管理：管理面板（机器增删/令牌轮换）+ re-auth 通用流程 +
// adminToken 内存缓存/过期 + 配对两端流（新设备等待 + 已登录侧批准）
// ---------------------------------------------------------------------------

describe("App 管理面板与 re-auth（v1.5）", () => {
  const ADMIN_TOK = "at".repeat(32);

  /** 开启 WebAuthn 能力（get 可控——同 passkey describe 的局部实现）。 */
  function enableWebauthn(get: () => Promise<unknown>): void {
    vi.stubGlobal("PublicKeyCredential", class PublicKeyCredential {});
    Object.defineProperty(navigator, "credentials", {
      value: {
        get,
        create: async () => {
          throw new Error("本用例不触发 create");
        },
      },
      configurable: true,
    });
  }

  function fakeAssertion(): unknown {
    const enc = (s: string) => new TextEncoder().encode(s);
    return {
      id: "cred-1",
      rawId: enc("raw-cred"),
      type: "public-key",
      response: {
        authenticatorData: enc("auth-data"),
        clientDataJSON: enc("{}"),
        signature: enc("sig"),
        userHandle: enc("user"),
      },
    };
  }

  /** reauth begin/complete 的 fetch mock（begin 次数可计数）。 */
  function reauthFetch(): ReturnType<typeof vi.fn> {
    return vi.fn(async (url: string | URL | Request) => {
      const u = typeof url === "string" ? url : url.toString();
      if (u.includes("webauthn/reauth/begin")) {
        return new Response(
          JSON.stringify({
            beginId: "rb-1",
            publicKey: { rpId: "localhost", challenge: "Zm9vYmFy", allowCredentials: [] },
          }),
          { status: 200 },
        );
      }
      if (u.includes("webauthn/reauth/complete")) {
        return new Response(
          JSON.stringify({ adminToken: ADMIN_TOK, expiresAt: Math.floor(Date.now() / 1000) + 300 }),
          { status: 200 },
        );
      }
      return new Response("{}", { status: 404 });
    }) as unknown as ReturnType<typeof vi.fn>;
  }

  /** 会话模式挂载 + hello_ack(session)（管理入口/配对卡都需要 session）。 */
  function mountSessionApp(): FakeWebSocket {
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 }),
    );
    render(<App />);
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.recv({
        v: 1,
        type: "hello_ack",
        role: "user",
        session: { id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 },
      });
    });
    return fake;
  }

  const sentOfType = (fake: FakeWebSocket, t: string) =>
    fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === t);

  const beginCalls = (m: ReturnType<typeof vi.fn>) =>
    m.mock.calls.filter((c) => c[0].toString().includes("reauth/begin"));

  afterEach(() => {
    __resetAdminToken();
  });

  it("打开管理面板：device_list/cred_list 即发；自动 re-auth（Bearer sessionToken）后 machine_list 带新 adminToken", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "device_list" }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "cred_list" }));
    // re-auth 完成 → machine_list（adminToken = reauth/complete 返回值）
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "machine_list", adminToken: ADMIN_TOK }),
    );
    const begin = beginCalls(fetchMock)[0]!;
    expect((begin[1]?.headers as Record<string, string>).authorization).toBe("Bearer st-r");

    // machines_cfg 渲染（名称/指纹/添加时间）
    act(() => {
      fake.recv({
        v: 1,
        type: "machines_cfg",
        machines: [{ name: "mac-mini", pubkey_fp: "aaaa bbbb cccc dddd", created: Math.floor(Date.now() / 1000) - 30 }],
      });
    });
    expect(screen.getByText("mac-mini")).toBeTruthy();
    expect(screen.getByText("aaaa bbbb cccc dddd")).toBeTruthy();
    expect(screen.getByText(/刚刚/)).toBeTruthy();
    expect(beginCalls(fetchMock)).toHaveLength(1); // 整个过程一次验证
  });

  it("添加机器：客户端校验拒绝非法输入；合法提交发 machine_add 且 adminToken 复用（不再 re-auth）", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });

    // 非法名称 + 短公钥 → 行内提示，不发帧
    fireEvent.change(screen.getByLabelText("机器名"), { target: { value: "bad name" } });
    fireEvent.change(screen.getByLabelText("机器公钥"), { target: { value: "abcd" } });
    fireEvent.click(screen.getByRole("button", { name: "添加机器" }));
    expect(screen.getByText(/机器名须为/)).toBeTruthy();
    expect(sentOfType(fake, "machine_add")).toHaveLength(0);

    // 合法提交（公钥大写 → 归一小写；name 透传）
    fireEvent.change(screen.getByLabelText("机器名"), { target: { value: "mac-mini" } });
    fireEvent.change(screen.getByLabelText("机器公钥"), { target: { value: "AB".repeat(32) } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "添加机器" }));
    });
    expect(fake.sent).toContain(
      JSON.stringify({
        v: 1,
        type: "machine_add",
        adminToken: ADMIN_TOK,
        name: "mac-mini",
        pubkey: "ab".repeat(32),
      }),
    );
    // adminToken 5 分钟窗：两次操作共享同一次 re-auth
    expect(beginCalls(fetchMock)).toHaveLength(1);
  });

  it("移除机器：二次确认（断线提示）后才发 machine_remove", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });
    act(() => {
      fake.recv({ v: 1, type: "machines_cfg", machines: [{ name: "mac-mini", pubkey_fp: "fp" }] });
    });
    fireEvent.click(screen.getByRole("button", { name: "移除" }));
    expect(screen.getByText(/网关连接将被立即断开/)).toBeTruthy();
    expect(sentOfType(fake, "machine_remove")).toHaveLength(0);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "确认移除" }));
    });
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "machine_remove", adminToken: ADMIN_TOK, name: "mac-mini" }),
    );
  });

  it("令牌轮换：确认警告 → user_token_rotate → 结果弹层一次性显示（复制按钮 + env 落点提示）→ 关闭即失", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });

    fireEvent.click(screen.getByRole("button", { name: "轮换访问令牌" }));
    expect(screen.getByText(/旧访问令牌立即失效/)).toBeTruthy(); // 二次确认警告
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "确认轮换" }));
    });
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "user_token_rotate", adminToken: ADMIN_TOK }),
    );

    act(() => {
      fake.recv({ v: 1, type: "user_token_rotated", token: "ff".repeat(32) });
    });
    const modal = screen.getByRole("alertdialog", { name: "访问令牌已轮换" });
    expect(within(modal).getByText(/仅显示这一次/)).toBeTruthy();
    expect(within(modal).getByText("~/.imagent/web-relay.env")).toBeTruthy();
    expect(within(modal).getByText("ff".repeat(32))).toBeTruthy();
    expect(within(modal).getByRole("button", { name: "复制令牌" })).toBeTruthy();

    fireEvent.click(within(modal).getByRole("button", { name: "我已保存，关闭" }));
    expect(screen.queryByText("ff".repeat(32))).toBeNull(); // 一次性：关闭即失
  });

  it("admin_auth_required：作废本地 adminToken 引导重试；下一操作自动重新 re-auth", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });
    expect(beginCalls(fetchMock)).toHaveLength(1);

    act(() => {
      fake.recv({ v: 1, type: "error", code: "admin_auth_required" });
    });
    expect(screen.getByText(/管理验证已过期，请重试操作/)).toBeTruthy();

    // 重试（刷新机器列表）→ 第二次 re-auth
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "刷新" }));
    });
    expect(beginCalls(fetchMock)).toHaveLength(2);
    expect(sentOfType(fake, "machine_list")).toHaveLength(2);
  });

  it("re-auth 用户取消：adminError 提示、不发管理帧", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => {
      throw new DOMException("canceled", "NotAllowedError");
    });
    const fake = mountSessionApp();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });
    expect(screen.getByText(/已取消管理验证/)).toBeTruthy();
    expect(sentOfType(fake, "machine_list")).toHaveLength(0);
  });

  it("无 WebAuthn 能力：面板提示需通行钥匙，machine_list 不发（设备/钥匙列表照常拉取）", () => {
    const fake = mountSessionApp();
    fireEvent.click(screen.getByRole("button", { name: "管理" }));
    expect(screen.getAllByText(/本浏览器不支持通行钥匙/).length).toBeGreaterThan(0);
    expect(sentOfType(fake, "machine_list")).toHaveLength(0);
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "device_list" }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "cred_list" }));
  });
});

// ---------------------------------------------------------------------------
// v1.5 新设备配对：登录页入口（新设备侧）+ 批准侧
// ---------------------------------------------------------------------------

describe("App 新设备配对流（v1.5）", () => {
  /** 从登录页发起配对（open 后返回配对连接的 fake）。 */
  function startPairing(device: string): FakeWebSocket {
    render(<App />);
    fireEvent.click(screen.getByRole("button", { name: /我是新设备/ }));
    fireEvent.change(screen.getByLabelText("设备名（可选）"), { target: { value: device } });
    fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => fake.emit("open"));
    return fake;
  }

  it("hello{pairing} 字节级（无凭证）；pairing_pending → 等待码大字 + 倒计时", () => {
    const fake = startPairing("新 iPad");
    expect(fake.sent[0]).toBe(
      JSON.stringify({ v: 1, type: "hello", pairing: true, device: "新 iPad" }),
    );
    expect(screen.getByText("正在连接中继…")).toBeTruthy();
    act(() => {
      fake.recv({ v: 1, type: "pairing_pending", code: "483920" });
    });
    expect(screen.getByLabelText("配对等待码").textContent).toBe("483920");
    expect(screen.getByLabelText("配对等待倒计时")).toBeTruthy();
  });

  it("批准进入：配对连接收 hello_ack.session → 存 session 并以会话形态重连", () => {
    const fake = startPairing("新 iPad");
    act(() => {
      fake.recv({ v: 1, type: "pairing_pending", code: "111111" });
      fake.recv({
        v: 1,
        type: "hello_ack",
        role: "user",
        session: { id: "sid-p", token: "tok-p", expiresAt: Date.now() + 30 * 86_400_000 },
      });
    });
    // session 持久化；配对连接被替换为新连接（hello 用 session 形态）
    expect(JSON.parse(localStorage.getItem("imagent.session") ?? "null")).toMatchObject({
      id: "sid-p",
      token: "tok-p",
    });
    const next = FakeWebSocket.latest as FakeWebSocket;
    expect(next).not.toBe(fake);
    act(() => {
      next.emit("open");
      next.recv({
        v: 1,
        type: "hello_ack",
        role: "user",
        session: { id: "sid-p", token: "tok-p", expiresAt: Date.now() + 30 * 86_400_000 },
      });
    });
    expect(next.sent).toContain(JSON.stringify({ v: 1, type: "hello", session: "tok-p" }));
    expect(screen.getByText("已连接")).toBeTruthy();
  });

  it("被拒绝：close 1008 pairing denied → 回登录页并提示", () => {
    const fake = startPairing("新 iPad");
    act(() => {
      fake.emit("close", { code: 1008, reason: "pairing denied" });
    });
    expect(screen.getByRole("alert").textContent).toContain("配对已被拒绝");
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
  });

  it("取消配对：返回登录页并关闭配对连接", () => {
    const fake = startPairing("新 iPad");
    act(() => {
      fake.recv({ v: 1, type: "pairing_pending", code: "111111" });
    });
    fireEvent.click(screen.getByRole("button", { name: "取消配对" }));
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(fake.closedWith.length).toBeGreaterThan(0);
  });
});

describe("App 配对批准侧（v1.5）", () => {
  const ADMIN_TOK = "at".repeat(32);

  function enableWebauthn(get: () => Promise<unknown>): void {
    vi.stubGlobal("PublicKeyCredential", class PublicKeyCredential {});
    Object.defineProperty(navigator, "credentials", {
      value: {
        get,
        create: async () => {
          throw new Error("本用例不触发 create");
        },
      },
      configurable: true,
    });
  }

  function fakeAssertion(): unknown {
    const enc = (s: string) => new TextEncoder().encode(s);
    return {
      id: "cred-1",
      rawId: enc("raw-cred"),
      type: "public-key",
      response: {
        authenticatorData: enc("auth-data"),
        clientDataJSON: enc("{}"),
        signature: enc("sig"),
        userHandle: enc("user"),
      },
    };
  }

  function reauthFetch(): ReturnType<typeof vi.fn> {
    return vi.fn(async (url: string | URL | Request) => {
      const u = typeof url === "string" ? url : url.toString();
      if (u.includes("webauthn/reauth/begin")) {
        return new Response(
          JSON.stringify({
            beginId: "rb-1",
            publicKey: { rpId: "localhost", challenge: "Zm9vYmFy", allowCredentials: [] },
          }),
          { status: 200 },
        );
      }
      if (u.includes("webauthn/reauth/complete")) {
        return new Response(
          JSON.stringify({ adminToken: ADMIN_TOK, expiresAt: Math.floor(Date.now() / 1000) + 300 }),
          { status: 200 },
        );
      }
      return new Response("{}", { status: 404 });
    }) as unknown as ReturnType<typeof vi.fn>;
  }

  function mountSessionApp(): FakeWebSocket {
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 }),
    );
    render(<App />);
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.recv({
        v: 1,
        type: "hello_ack",
        role: "user",
        session: { id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 },
      });
    });
    return fake;
  }

  const sentOfType = (fake: FakeWebSocket, t: string) =>
    fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === t);

  afterEach(() => {
    __resetAdminToken();
  });

  it("pairing_request → 顶部卡（设备名+码）；允许走 re-auth → pairing_approve 帧后卡消失", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();

    act(() => {
      fake.recv({
        v: 1,
        type: "pairing_request",
        code: "483920",
        device: "iPhone/Safari",
        expiresAt: Math.floor(Date.now() / 1000) + 300,
      });
    });
    const card = screen.getByRole("alertdialog", { name: "新设备配对请求" });
    expect(within(card).getByText("iPhone/Safari")).toBeTruthy();
    expect(within(card).getByLabelText("配对请求码").textContent).toBe("483920");

    await act(async () => {
      fireEvent.click(within(card).getByRole("button", { name: "允许" }));
    });
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "pairing_approve", adminToken: ADMIN_TOK, code: "483920" }),
    );
    expect(screen.queryByRole("alertdialog", { name: "新设备配对请求" })).toBeNull();
  });

  it("拒绝（同样管理级 re-auth）→ pairing_deny 帧", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => fakeAssertion());
    const fake = mountSessionApp();
    act(() => {
      fake.recv({ v: 1, type: "pairing_request", code: "222222", device: "Mac/Chrome" });
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "拒绝" }));
    });
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "pairing_deny", adminToken: ADMIN_TOK, code: "222222" }),
    );
    expect(screen.queryByRole("alertdialog", { name: "新设备配对请求" })).toBeNull();
  });

  it("re-auth 取消：不发帧、卡片保留并给出提示", async () => {
    const fetchMock = reauthFetch();
    vi.stubGlobal("fetch", fetchMock);
    enableWebauthn(async () => {
      throw new DOMException("canceled", "NotAllowedError");
    });
    const fake = mountSessionApp();
    act(() => {
      fake.recv({ v: 1, type: "pairing_request", code: "333333", device: "Mac/Chrome" });
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "允许" }));
    });
    expect(sentOfType(fake, "pairing_approve")).toHaveLength(0);
    expect(screen.getByRole("alertdialog", { name: "新设备配对请求" })).toBeTruthy();
    expect(screen.getByText(/已取消管理验证/)).toBeTruthy();
  });

  it("同码广播重放幂等：只弹一张卡", () => {
    const fake = mountSessionApp();
    act(() => {
      fake.recv({ v: 1, type: "pairing_request", code: "444444", device: "A" });
      fake.recv({ v: 1, type: "pairing_request", code: "444444", device: "A" });
    });
    expect(screen.getAllByLabelText("配对请求码")).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// v1.6 工作空间：ready 后拉快照 / 目录条常驻与切换 / 新会话弹层选目录
// ---------------------------------------------------------------------------

describe("App 工作空间（v1.6）", () => {
  const WORKSPACES = {
    v: 1,
    type: "workspaces",
    default: "/Users/u/default",
    workspaces: [
      { name: "code", path: "/Users/u/code", lastUsedAt: 100 },
      { name: "docs", path: "/Users/u/docs", lastUsedAt: 200 },
      { name: "old", path: "/Users/u/old" },
    ],
    convs: { "local:work": "/Users/u/tmp" },
    machine: "home-mac",
  };

  /** 挂载 + 握手 + 推工作空间快照，返回活跃 socket。 */
  function mountWorkspaceApp(): FakeWebSocket {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => fake.recv(WORKSPACES));
    return fake;
  }

  it("ready 后发 workspace_list（带当前机器）；未收 machines（旧 relay）不发；切机对新机器再发", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    // 旧 relay：无 machines 帧 → 不发（网关不认识该帧）
    expect(fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "workspace_list")).toHaveLength(0);
    act(() => fake.recv({ v: 1, type: "machines", online: ["home-mac", "work-mac"] }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "workspace_list", machine: "home-mac" }));
    // 切机 → 对新机器再拉
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "workspace_list", machine: "work-mac" }));
  });

  it("目录条：快照后渲染 default，conv_workdir 增量更新显示；无快照（旧网关）整条不渲染", () => {
    const fake = mountWorkspaceApp();
    const bar = screen.getByRole("button", { name: "切换会话目录" });
    expect(bar.textContent).toContain("/Users/u/default");
    act(() =>
      fake.recv({ v: 1, type: "conv_workdir", convId: "web:default", workdir: "/Users/u/code", machine: "home-mac" }),
    );
    expect(screen.getByRole("button", { name: "切换会话目录" }).textContent).toContain("/Users/u/code");

    // 无快照：旧 relay 握手（无 machines 帧）→ 整条不渲染（先卸载上一个 App）
    cleanup();
    const fake2 = mountApp();
    act(() => {
      fake2.emit("open");
      fake2.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    expect(screen.queryByRole("button", { name: "切换会话目录" })).toBeNull();
  });

  it("目录条点击弹切换器：chips 分区渲染；点选发 conv_workdir_set（convId 完整形态 + machine）并记 lastdir", () => {
    const fake = mountWorkspaceApp();
    fireEvent.click(screen.getByRole("button", { name: "切换会话目录" }));
    const dialog = screen.getByRole("dialog", { name: "切换会话目录" });
    // 空间 chips 按 lastUsedAt 降序（docs > code > old 无时间殿后）
    const spaces = within(within(dialog).getByLabelText("工作空间")).getAllByRole("button");
    expect(spaces.map((b) => b.textContent)).toEqual(["docs", "code", "old"]);
    // 正在使用（convs 值去重、排除空间 path/default）+ 默认 chip
    expect(within(within(dialog).getByLabelText("正在使用")).getByRole("button", { name: "tmp" })).toBeTruthy();
    expect(within(dialog).getByRole("button", { name: "默认" })).toBeTruthy();

    // 点选空间 → setConvWorkdir 帧内容断言 + lastdir 写入 + 弹层关闭
    fireEvent.click(spaces[0]);
    expect(fake.sent).toContain(
      JSON.stringify({
        v: 1,
        type: "conv_workdir_set",
        convId: "web:default",
        path: "/Users/u/docs",
        machine: "home-mac",
      }),
    );
    expect(localStorage.getItem("imagent-lastdir:home-mac")).toBe("/Users/u/docs");
    expect(screen.queryByRole("dialog", { name: "切换会话目录" })).toBeNull();
  });

  it("切换器选默认：同样发 default 路径的 set（语义统一）", () => {
    const fake = mountWorkspaceApp();
    fireEvent.click(screen.getByRole("button", { name: "切换会话目录" }));
    fireEvent.click(screen.getByRole("button", { name: "默认" }));
    expect(fake.sent).toContain(
      JSON.stringify({
        v: 1,
        type: "conv_workdir_set",
        convId: "web:default",
        path: "/Users/u/default",
        machine: "home-mac",
      }),
    );
  });

  it("新会话弹层：选空间创建 → /switch + setConvWorkdir(新 convId) + lastdir；重开预选上次选择", () => {
    const fake = mountWorkspaceApp();
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    let dialog = screen.getByRole("dialog", { name: "新会话" });
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "proj" } });
    fireEvent.click(within(within(dialog).getByLabelText("工作空间")).getByRole("button", { name: "code" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    // /switch 走默认会话（convId 缺省）+ 创建后绑定目录（convId 完整形态）
    expect(sentMessage(fake, { v: 1, type: "message", text: "/switch proj", machine: "home-mac" })).toBe(true);
    expect(fake.sent).toContain(
      JSON.stringify({
        v: 1,
        type: "conv_workdir_set",
        convId: "web:proj",
        path: "/Users/u/code",
        machine: "home-mac",
      }),
    );
    expect(localStorage.getItem("imagent-lastdir:home-mac")).toBe("/Users/u/code");
    // 弹层关闭、当前会话切到 web:proj（StatusBar 会话徽标 + 侧栏项）
    expect(screen.queryByRole("dialog", { name: "新会话" })).toBeNull();
    expect(screen.getAllByText("proj").length).toBeGreaterThanOrEqual(1);

    // 重开弹层：预选 lastdir（code chip 高亮）
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    dialog = screen.getByRole("dialog", { name: "新会话" });
    const codeChip = within(within(dialog).getByLabelText("工作空间")).getByRole("button", { name: "code" });
    expect(codeChip.getAttribute("aria-pressed")).toBe("true");

    // 选默认 chip 创建：dir === default → 不发 set（新会话天然落默认目录）
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "plain" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "默认" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    expect(sentMessage(fake, { v: 1, type: "message", text: "/switch plain", machine: "home-mac" })).toBe(true);
    expect(fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "conv_workdir_set")).toHaveLength(1);
  });

  it("新会话弹层：无快照（旧网关）不渲染目录区，名称创建流程不回归", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    expect(within(dialog).queryByLabelText("工作空间")).toBeNull();
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "legacy" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    expect(sentMessage(fake, { v: 1, type: "message", text: "/switch legacy" })).toBe(true);
  });

  // v1.36 引擎选择 chips：快照可选 backends 段（网关默认引擎 + 全注册表引擎名）

  /** 快照含 backends 段的挂载（多机握手 + 推快照）。 */
  function mountBackendsApp(): FakeWebSocket {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() =>
      fake.recv({
        ...WORKSPACES,
        backends: { current: "claude-code", options: ["default", "claude-code", "codex-app"] },
      }),
    );
    return fake;
  }

  it("新会话弹层（v1.36）：快照含 backends → 引擎 chips 默认选中；选 codex-app 创建 → /backend 帧发新 convId + 回显落新会话", () => {
    const fake = mountBackendsApp();
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    const eng = within(within(dialog).getByLabelText("引擎"));
    // 「默认（current）」chip 默认选中 + title 提示默认引擎名；options 各 chip 未选
    const defChip = eng.getByRole("button", { name: "默认（claude-code）" });
    expect(defChip.getAttribute("aria-pressed")).toBe("true");
    expect(defChip.getAttribute("title")).toBe("网关默认引擎：claude-code");
    const codexChip = eng.getByRole("button", { name: "codex-app" });
    expect(codexChip.getAttribute("aria-pressed")).toBe("false");
    expect(eng.getByRole("button", { name: "default" })).toBeTruthy();
    // 选 codex-app → 高亮切换
    fireEvent.click(codexChip);
    expect(codexChip.getAttribute("aria-pressed")).toBe("true");
    expect(defChip.getAttribute("aria-pressed")).toBe("false");
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "proj" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    // /switch 走默认会话（无 convId）；/backend 显式发给新 convId
    expect(sentMessage(fake, { v: 1, type: "message", text: "/switch proj", machine: "home-mac" })).toBe(true);
    expect(
      sentMessage(fake, {
        v: 1,
        type: "message",
        convId: "web:proj",
        text: "/backend codex-app",
        machine: "home-mac",
      }),
    ).toBe(true);
    // 本地回显与新会话首屏同落（当前会话已切 web:proj，user 气泡渲染命令文本）
    expect(screen.queryByRole("dialog", { name: "新会话" })).toBeNull();
    expect(screen.getAllByText("/backend codex-app").length).toBeGreaterThanOrEqual(1);
  });

  it("新会话弹层（v1.36）：选「默认」创建不发 /backend 帧（跟随网关默认引擎）；重开弹层重置回默认", () => {
    const fake = mountBackendsApp();
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    let dialog = screen.getByRole("dialog", { name: "新会话" });
    // 选了 codex-app 但不创建，关闭弹层
    fireEvent.click(within(within(dialog).getByLabelText("引擎")).getByRole("button", { name: "codex-app" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "取消" }));
    // 重开：引擎选择重置回默认（同清名/预选目录的模式）
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    dialog = screen.getByRole("dialog", { name: "新会话" });
    const eng = within(within(dialog).getByLabelText("引擎"));
    expect(eng.getByRole("button", { name: "默认（claude-code）" }).getAttribute("aria-pressed")).toBe("true");
    expect(eng.getByRole("button", { name: "codex-app" }).getAttribute("aria-pressed")).toBe("false");
    // 默认态创建：不发任何 /backend 帧
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "plain" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    expect(sentMessage(fake, { v: 1, type: "message", text: "/switch plain", machine: "home-mac" })).toBe(true);
    expect(fake.sent.filter((s) => (JSON.parse(s) as { text?: string }).text?.startsWith("/backend"))).toHaveLength(0);
  });

  it("新会话弹层（v1.36）：快照缺席 backends → 无引擎行（优雅降级），目录区照常", () => {
    mountWorkspaceApp(); // WORKSPACES 无 backends 段
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    expect(within(dialog).queryByLabelText("引擎")).toBeNull();
    expect(within(dialog).getByLabelText("工作空间")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// 会话自动命名：留空名字创建（s-xxxx 自动短 ID，协议帧零改动）+ 显示名由
// 首条消息派生（convTitle）
// ---------------------------------------------------------------------------

describe("App 会话自动命名", () => {
  /** 挂载 + 握手到 ready（单机模式即可），返回活跃 socket。 */
  function mountReady(): FakeWebSocket {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    return fake;
  }

  /** 出站 message 帧里的全部 /switch 命令文本。 */
  const switchCmds = (fake: FakeWebSocket): string[] =>
    fake.sent
      .map((s) => JSON.parse(s) as { type?: string; text?: string })
      .filter((f) => f.type === "message" && f.text?.startsWith("/switch "))
      .map((f) => f.text as string);

  it("留空名字创建：/switch s-xxxx 短 ID（协议零改动）+ 当前会话切到 web:s-xxxx", () => {
    const fake = mountReady();
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    // 名字留空直接创建（空值合法，按钮不再禁用）
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    const cmds = switchCmds(fake);
    expect(cmds).toHaveLength(1);
    expect(cmds[0]).toMatch(/^\/switch s-[0-9a-f]{4}$/);
    const shortId = cmds[0].slice("/switch ".length);
    // 弹层关闭；select_conv 落 web:s-xxxx：侧栏项 + 状态条徽标显示短 ID（尚无消息，无 title）
    expect(screen.queryByRole("dialog", { name: "新会话" })).toBeNull();
    expect(screen.getAllByText(shortId).length).toBeGreaterThanOrEqual(2);
  });

  it("自动会话发首条消息：message 帧带完整 convId；侧栏/状态条改显首条消息摘要", () => {
    const fake = mountReady();
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    const shortId = switchCmds(fake)[0].slice("/switch ".length);
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "帮我写个部署脚本" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    // 发到自动会话（完整 conv 形态）
    expect(
      sentMessage(fake, { v: 1, type: "message", convId: `web:${shortId}`, text: "帮我写个部署脚本" }),
    ).toBe(true);
    // 消息气泡 + 侧栏标题 + 侧栏预览 + 状态条徽标均显示首条消息文本（title 派生）
    expect(screen.getAllByText("帮我写个部署脚本").length).toBeGreaterThanOrEqual(3);
    // 短 ID 从显示位消失（显示名已换 title；convId 仅存于协议层）
    expect(screen.queryByText(shortId)).toBeNull();
  });

  it("手动名会话：发首条消息后显示名仍为手动名（title 不覆盖）", () => {
    const fake = mountReady();
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "proj" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    expect(switchCmds(fake)).toEqual(["/switch proj"]);
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "手动会话首条消息" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    // 侧栏标题 + 状态条徽标仍显示手动名 proj（title 已派生但显示名不采用）
    expect(screen.getAllByText("proj").length).toBeGreaterThanOrEqual(2);
    expect(screen.getAllByText("手动会话首条消息").length).toBeGreaterThanOrEqual(1);
  });
});

// ---------------------------------------------------------------------------
// v1.8 模型菜单动态化：/model 命令发出后延时 800ms 重拉 workspace_list
// （刷新快照 models.current 的「当前模型」高亮）
// ---------------------------------------------------------------------------

describe("/model 命令后延时重拉快照（v1.8）", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  /** 已出站的 workspace_list 帧数。 */
  const listCount = (fake: FakeWebSocket): number =>
    fake.sent.filter((s) => (JSON.parse(s) as { type?: string }).type === "workspace_list").length;

  /** 出站 message 帧的 text 列表。 */
  const messageTexts = (fake: FakeWebSocket): string[] =>
    fake.sent
      .map((s) => JSON.parse(s) as { type?: string; text?: string })
      .filter((f) => f.type === "message")
      .map((f) => f.text as string);

  /** 挂载 + 握手到 ready（单机 home-mac 在线）并返回 fake socket。 */
  function mountReady(): FakeWebSocket {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    return fake;
  }

  /** 输入框打字 + Enter 发送。 */
  const send = (text: string) => {
    const ta = screen.getByLabelText("消息输入框");
    fireEvent.change(ta, { target: { value: text } });
    fireEvent.keyDown(ta, { key: "Enter" });
  };

  it("发送 `/model opus`（带参）后 800ms 重发一次 workspace_list（带当前机器）", () => {
    vi.useFakeTimers();
    const fake = mountReady();
    const before = listCount(fake);
    expect(before).toBe(1); // ready effect 已发一条

    send("/model opus");
    expect(messageTexts(fake)).toContain("/model opus");
    // 未到 800ms 不重发；到点恰一条（带当前选中机器）。
    act(() => vi.advanceTimersByTime(799));
    expect(listCount(fake)).toBe(before);
    act(() => vi.advanceTimersByTime(1));
    expect(listCount(fake)).toBe(before + 1);
    const lists = fake.sent
      .map((s) => JSON.parse(s) as { type?: string; machine?: string })
      .filter((f) => f.type === "workspace_list");
    expect(lists[lists.length - 1].machine).toBe("home-mac");
  });

  it("`/model`（无参/仅尾随空格）与普通消息不触发重拉", () => {
    vi.useFakeTimers();
    const fake = mountReady();
    const before = listCount(fake);
    send("普通消息");
    send("/model"); // 无参（查看形态）
    send("/modelsomething"); // 非 /model 命令前缀
    act(() => vi.advanceTimersByTime(3000));
    expect(listCount(fake)).toBe(before);
  });
});

// ---------------------------------------------------------------------------
// v1.7.x 消息本地缓存（IndexedDB 门面 spy 注入）：挂载预填恢复 / 2s debounce
// 只写脏 conv / 退出登录清空
// ---------------------------------------------------------------------------

describe("App 本地消息缓存（historyStore 集成）", () => {
  /** 恢复条目夹具。 */
  const userEntry = (text: string, at: number) => ({ kind: "user" as const, key: `user:${at}`, text, at });
  const textEntry = (text: string, at: number) => ({ kind: "text" as const, key: `text:${at}`, text, at });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  it("挂载预填恢复：entries/预览/时间渲染、convOrder 按 lastAt 插 default 后、default 会话 title 生效", async () => {
    vi.spyOn(historyStore, "loadAll").mockResolvedValue({
      "home-mac": {
        "web:s-ab12": { entries: [userEntry("自动会话历史首条", 3000)] },
        "web:default": {
          entries: [userEntry("默认会话历史首条", 1000), textEntry("默认会话历史回复", 1100)],
        },
      },
    });
    const fake = mountApp();
    // 空态先渲染，loadAll 完成后一次 setState（微任务 flush）
    await act(async () => {});
    handshake(fake, ["home-mac"]);
    // default 会话：恢复首条派生 title → 侧栏标题 + 状态条徽标改显 title
    expect(screen.getAllByText("默认会话历史首条").length).toBeGreaterThanOrEqual(2);
    // 自动会话 web:s-a：title = 首条消息（侧栏标题 + 预览）
    expect(screen.getAllByText("自动会话历史首条").length).toBeGreaterThanOrEqual(2);
    // 末条预览：default 会话预览是最后一条 text
    expect(screen.getAllByText("默认会话历史回复").length).toBeGreaterThanOrEqual(1);
    // 点进恢复的会话：消息气泡渲染（entries 已预填）
    fireEvent.click(screen.getByRole("button", { name: /自动会话历史首条/ }));
    expect(screen.getAllByText("自动会话历史首条").length).toBeGreaterThanOrEqual(3);
  });

  it("保存 debounce：2s 后只写 entries 有变化的 conv（空会话/未变化不写）", async () => {
    vi.useFakeTimers();
    vi.spyOn(historyStore, "loadAll").mockResolvedValue({});
    const saveSpy = vi.spyOn(historyStore, "saveConv").mockResolvedValue();
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    await act(async () => {}); // loadAll resolve
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台消息", machine: "home-mac" });
    });
    act(() => vi.advanceTimersByTime(1_999));
    expect(saveSpy).not.toHaveBeenCalled(); // 未到 2s 不落盘
    act(() => vi.advanceTimersByTime(1));
    expect(saveSpy).toHaveBeenCalledTimes(1); // 只有脏 conv web:work（default 空会话不写）
    expect(saveSpy).toHaveBeenCalledWith(
      "home-mac",
      "web:work",
      [expect.objectContaining({ kind: "text", text: "后台消息" })],
      undefined, // v1.8：无 history 帧时水位缺省（不落 lastSeq 键）
    );
    // 引用不再变化：再推进不再重复写
    act(() => vi.advanceTimersByTime(5_000));
    expect(saveSpy).toHaveBeenCalledTimes(1);
  });

  it("退出登录：清空本地缓存，且清空后不再把内存旧会话回写", async () => {
    vi.useFakeTimers();
    vi.spyOn(historyStore, "loadAll").mockResolvedValue({});
    const saveSpy = vi.spyOn(historyStore, "saveConv").mockResolvedValue();
    const clearSpy = vi.spyOn(historyStore, "clear").mockResolvedValue();
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    await act(async () => {});
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:default", text: "将被清空的消息", machine: "home-mac" });
    });
    act(() => vi.advanceTimersByTime(2_000));
    expect(saveSpy).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "退出" }));
    expect(clearSpy).toHaveBeenCalledTimes(1); // 显式退出清本机痕迹
    // savedRefs 已对齐内存态：后续 flush 不把已清空的会话写回
    act(() => vi.advanceTimersByTime(5_000));
    expect(saveSpy).toHaveBeenCalledTimes(1);
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy(); // 回登录页
  });
});

// ---------------------------------------------------------------------------
// v1.7.x default 会话自动命名（convTitle 放宽）：固定入口也按首条消息命名
// ---------------------------------------------------------------------------

describe("App default 会话自动命名", () => {
  it("未发消息显示 default；发首条消息后侧栏/状态条改显 title", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    // 未发消息：侧栏标题 + 状态条会话徽标均显示 default
    expect(screen.getAllByText("default").length).toBeGreaterThanOrEqual(2);
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "默认会话的第一条" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    // 气泡 + 侧栏标题 + 侧栏预览 + 状态条徽标均显示首条消息（title 派生）
    expect(screen.getAllByText("默认会话的第一条").length).toBeGreaterThanOrEqual(3);
    // "default" 字样从显示位消失
    expect(screen.queryByText("default")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// v1.8 会话历史重放：ready/切会话触发 queryHistory（带本地水位）+ 水位随
// debounce 落盘 + 重放合并渲染（增量 / trimmed 重建）
// ---------------------------------------------------------------------------

describe("App 会话历史重放（v1.8）", () => {
  /** 出站的 history_query 帧（解析后对象，全字段断言用）。 */
  const historyQueries = (fake: FakeWebSocket) =>
    fake.sent
      .map((s) => JSON.parse(s) as Record<string, unknown>)
      .filter((f) => f.type === "history_query");

  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  it("ready 后对当前 conv 发 history_query（since=0、带当前机器）；旧 relay（无 machines 帧）不发", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    expect(historyQueries(fake)).toHaveLength(0); // 旧 relay 单机模式：网关不识帧
    act(() => fake.recv({ v: 1, type: "machines", online: ["home-mac"] }));
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "history_query", convId: "web:default", since: 0, machine: "home-mac" }),
    );
  });

  it("切会话每次都查；history 到达更新水位 → 切回带最新 lastSeq；空增量水位随 debounce 落盘", async () => {
    vi.useFakeTimers();
    vi.spyOn(historyStore, "loadAll").mockResolvedValue({});
    const saveSpy = vi.spyOn(historyStore, "saveConv").mockResolvedValue();
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    await act(async () => {}); // loadAll resolve
    // ready 已查当前 conv（since 0——本地无水位）
    expect(historyQueries(fake)).toEqual([
      { v: 1, type: "history_query", convId: "web:default", since: 0, machine: "home-mac" },
    ]);
    // 造出第二个会话并切过去：每次切都查（仍 since 0）
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台消息", machine: "home-mac" });
    });
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    expect(historyQueries(fake)).toEqual([
      { v: 1, type: "history_query", convId: "web:default", since: 0, machine: "home-mac" },
      { v: 1, type: "history_query", convId: "web:work", since: 0, machine: "home-mac" },
    ]);
    // history 到达（空增量，仅水位 7）
    act(() =>
      fake.recv({
        v: 1,
        type: "history",
        convId: "web:work",
        machine: "home-mac",
        entries: [],
        lastSeq: 7,
        trimmed: false,
      }),
    );
    // 切走再切回：since=7（本地水位）
    fireEvent.click(screen.getByRole("button", { name: /default/ }));
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    expect(historyQueries(fake).at(-1)).toEqual({
      v: 1,
      type: "history_query",
      convId: "web:work",
      since: 7,
      machine: "home-mac",
    });
    // 空增量也驱动落盘：2s 后 saveConv 带水位（entries 同写）
    act(() => vi.advanceTimersByTime(1_999));
    expect(saveSpy).not.toHaveBeenCalled();
    act(() => vi.advanceTimersByTime(1));
    expect(saveSpy).toHaveBeenCalledTimes(1);
    expect(saveSpy).toHaveBeenCalledWith(
      "home-mac",
      "web:work",
      [expect.objectContaining({ kind: "text", text: "后台消息" })],
      7,
    );
  });

  it("history 非空增量：重放条目合并渲染（气泡 + 侧栏概要）；trimmed 丢弃本地重建", async () => {
    vi.spyOn(historyStore, "loadAll").mockResolvedValue({});
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    await act(async () => {});
    act(() =>
      fake.recv({
        v: 1,
        type: "history",
        convId: "web:default",
        machine: "home-mac",
        entries: [
          { kind: "text", key: "h:1", text: "重放第一条", at: 1000 },
          {
            kind: "card",
            key: "h:2",
            messageId: "m1",
            card: {
              text: "重放终态卡",
              tool_calls: [],
              thoughts: [],
              todos: [],
              phase: "outputting",
              run_secs: 5,
              terminal: "done",
            },
            state: "done",
          },
        ],
        lastSeq: 2,
        trimmed: false,
      }),
    );
    // 本地空 conv 首次物化：气泡渲染 + 末条概要 + 首条 title（default 会话显示名）
    expect(screen.getAllByText(/重放第一条/).length).toBeGreaterThanOrEqual(2);
    expect(screen.getAllByText(/重放终态卡/).length).toBeGreaterThanOrEqual(2);
    // trimmed：本地条目丢弃，以重放集重建（概要换新；title 一旦写入不变）
    act(() =>
      fake.recv({
        v: 1,
        type: "history",
        convId: "web:default",
        machine: "home-mac",
        entries: [{ kind: "text", key: "h:9", text: "GC 后的完整历史", at: 9000 }],
        lastSeq: 9,
        trimmed: true,
      }),
    );
    expect(screen.getAllByText(/GC 后的完整历史/).length).toBeGreaterThanOrEqual(2);
    expect(screen.queryByText("重放终态卡")).toBeNull(); // 旧条目（气泡+概要）已不在
  });

  it("本地缓存恢复水位：ready 后首查携带恢复的 lastSeq；恢复条目照常渲染", async () => {
    vi.spyOn(historyStore, "loadAll").mockResolvedValue({
      "home-mac": {
        "web:default": {
          entries: [{ kind: "user", key: "user:1", text: "缓存消息", at: 1 }],
          lastSeq: 9,
        },
      },
    });
    const fake = mountApp();
    await act(async () => {}); // 恢复先于握手完成
    handshake(fake, ["home-mac"]);
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "history_query", convId: "web:default", since: 9, machine: "home-mac" }),
    );
    expect(screen.getAllByText("缓存消息").length).toBeGreaterThanOrEqual(2);
  });
});

// ---------------------------------------------------------------------------
// v1.10 ?conv= 深链会话定位（SW notificationclick 无窗口时 openWindow 到
// /imagent/?conv=...；挂载读参、完整 conv 形态校验后才切）
// ---------------------------------------------------------------------------

describe("v1.10 ?conv= 深链定位", () => {
  afterEach(() => {
    // 恢复 URL，避免污染同文件后续/其它用例
    window.history.replaceState(null, "", "/");
  });

  it("完整 conv 形态（web: 前缀）：挂载即切到该会话（侧栏当前项高亮）", () => {
    window.history.replaceState(null, "", "/?conv=web:abc");
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    const current = nav.querySelector('[aria-current="page"]');
    expect(current?.textContent).toContain("abc");
  });

  it("local: 前缀形态同样接受", () => {
    window.history.replaceState(null, "", "/?conv=local:term-1");
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("term-1");
  });

  it("非法形态（裸名字，缺前缀）不切换：保持默认会话", () => {
    window.history.replaceState(null, "", "/?conv=abc");
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    const current = nav.querySelector('[aria-current="page"]');
    expect(current?.textContent).toContain("default");
    expect(current?.textContent).not.toContain("abc");
  });

  it("无 ?conv= 参数：保持默认会话（回归）", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("default");
  });
});

// ---------------------------------------------------------------------------
// v1.36 上次视图记忆（localStorage "imagent.lastView"）：跨登录记住上次打开
// 的机器+会话。恢复在 useReducer lazy initializer（畸形回落默认）；持久化在
// 渲染后 effect（currentMachine/currentConv 变化即写）。恢复的机器在线 →
// 既有 machines 帧校正逻辑 no-op 保持；不在线 → 校正切 online[0]（不新增分支）。
// ---------------------------------------------------------------------------

describe("App 上次视图记忆（v1.36 lastView）", () => {
  it("切机 + 创建会话后：lastView 记录正确的 {machine, conv}", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);
    // 首帧 machines 校正：default 不在线 → 切 home-mac，视图随之落表
    expect(localStorage.getItem("imagent.lastView")).toBe(
      JSON.stringify({ machine: "home-mac", conv: "web:default" }),
    );
    // 切到 work-mac（reducer 回该机器默认会话）
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    expect(localStorage.getItem("imagent.lastView")).toBe(
      JSON.stringify({ machine: "work-mac", conv: "web:default" }),
    );
    // 创建会话 proj（无快照也可创建；select_conv 切当前 conv 后落表）
    fireEvent.click(screen.getByRole("button", { name: "+ 新会话" }));
    const dialog = screen.getByRole("dialog", { name: "新会话" });
    fireEvent.change(within(dialog).getByLabelText("新会话名称"), { target: { value: "proj" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建（/switch）" }));
    expect(localStorage.getItem("imagent.lastView")).toBe(
      JSON.stringify({ machine: "work-mac", conv: "web:proj" }),
    );
  });

  it("预置合法 lastView：挂载即恢复该机器+会话（机器在线 → 校正逻辑保持不切）", () => {
    localStorage.setItem(
      "imagent.lastView",
      JSON.stringify({ machine: "work-mac", conv: "web:proj" }),
    );
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);
    // 恢复的机器在 machines 列表 → 不自动切 home-mac（无「已连接机器」引导 toast）
    expect(screen.getByText("work-mac 在线")).toBeTruthy();
    expect(screen.queryByText("已连接机器 home-mac")).toBeNull();
    // 当前会话即恢复值（侧栏当前项高亮 web:proj）
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("proj");
  });

  it("恢复的机器不在线：machines 帧校正切 online[0]（既有逻辑，lastView 跟随落表）", () => {
    localStorage.setItem(
      "imagent.lastView",
      JSON.stringify({ machine: "gone-mac", conv: "web:old" }),
    );
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    // 校正通知措辞随恢复来源机器变化（from=gone-mac ≠ default → 离线切换文案）
    expect(screen.getByText("gone-mac 已离线，已切换到 home-mac")).toBeTruthy();
    expect(localStorage.getItem("imagent.lastView")).toBe(
      JSON.stringify({ machine: "home-mac", conv: "web:old" }),
    );
  });

  it("预置畸形 lastView（坏 JSON/字段缺失/非 string/空串）：回落默认不炸，且被合法默认覆写", () => {
    const bads = [
      "{not-json",
      JSON.stringify({ machine: "work-mac" }), // 缺 conv
      JSON.stringify({ machine: 7, conv: "web:proj" }), // 非 string
      JSON.stringify({ machine: "", conv: "web:proj" }), // 空串
    ];
    for (const bad of bads) {
      localStorage.setItem("imagent.lastView", bad);
      mountApp();
      // 渲染不炸、当前会话回落 default
      const nav = screen.getByRole("navigation", { name: "会话列表" });
      expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("default");
      // 挂载 effect 即把坏值覆写为合法默认（单机未收 machines 帧：default 形态）
      expect(localStorage.getItem("imagent.lastView")).toBe(
        JSON.stringify({ machine: "default", conv: "web:default" }),
      );
      cleanup();
    }
  });
});

// ---------------------------------------------------------------------------
// v1.37 深链 ?machine=：推送通知深链切机器（先 select_machine 再 select_conv
// ——select_conv 落在该机器会话空间才有效；machine 非法忽略仍按 conv 走；
// 深链参数优先于 lastView 恢复）
// ---------------------------------------------------------------------------

describe("v1.37 深链 ?machine= 切机器", () => {
  afterEach(() => {
    // 恢复 URL，避免污染同文件后续/其它用例
    window.history.replaceState(null, "", "/");
  });

  it("machine+conv 深链：切到该机器并定位会话（不被 machines 帧校正拉回 online[0]）", () => {
    window.history.replaceState(null, "", "/?conv=web:proj&machine=work-mac");
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);
    // 机器选中 work-mac（listbox 唯一 aria-selected）
    const listbox = screen.getByRole("listbox", { name: "机器列表" });
    expect(listbox.querySelector('[aria-selected="true"]')?.textContent).toContain("work-mac");
    // 会话定位 web:proj（侧栏当前项高亮）
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("proj");
  });

  it("machine 非法：忽略 machine 仍按 conv 走（机器走 machines 帧自动校正 online[0]）", () => {
    window.history.replaceState(null, "", "/?conv=web:abc&machine=bad%20mac");
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);
    const listbox = screen.getByRole("listbox", { name: "机器列表" });
    expect(listbox.querySelector('[aria-selected="true"]')?.textContent).toContain("home-mac");
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("abc");
  });

  it("只有 conv（既有行为回归）：机器保持自动校正的 online[0]", () => {
    window.history.replaceState(null, "", "/?conv=web:abc");
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);
    const listbox = screen.getByRole("listbox", { name: "机器列表" });
    expect(listbox.querySelector('[aria-selected="true"]')?.textContent).toContain("home-mac");
  });

  it("SW open-conv 消息带 machine：同样先切机器再切会话", () => {
    // jsdom 无 SW/PushManager/Notification：stub 出最小面使 pushSupported() 为真
    const listeners = new Map<string, (ev: Event) => void>();
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: {
        addEventListener: (t: string, l: (ev: Event) => void) => listeners.set(t, l),
        removeEventListener: (t: string) => listeners.delete(t),
        // register 恒败（本用例不测注册；initServiceWorker 内 catch 静默）
        register: () => Promise.reject(new Error("本用例不触发注册")),
      },
    });
    vi.stubGlobal("PushManager", class PushManager {});
    vi.stubGlobal("Notification", class Notification {});
    try {
      const fake = mountApp();
      handshake(fake, ["home-mac", "work-mac"]);
      act(() => {
        listeners.get("message")?.({
          data: { type: "open-conv", convId: "web:proj", machine: "work-mac" },
        } as MessageEvent);
      });
      const listbox = screen.getByRole("listbox", { name: "机器列表" });
      expect(listbox.querySelector('[aria-selected="true"]')?.textContent).toContain("work-mac");
      const nav = screen.getByRole("navigation", { name: "会话列表" });
      expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("proj");
    } finally {
      delete (navigator as { serviceWorker?: unknown }).serviceWorker;
    }
  });
});

// ---------------------------------------------------------------------------
// v1.39 SW 更新提示可点击：检测到新版本（installing→installed 且已有旧
// controller）→ 可点 toast「新版本已就绪，点击刷新」，点击立即 reload。
// （文案不带版本号：页面只知道自己正在跑的旧版本号，见 App.tsx 该处注释）
// （controllerchange 自动重载为双保险——容器监听器语义由 push.test 单测驱动）
// ---------------------------------------------------------------------------

describe("v1.39 SW 更新提示可点击", () => {
  it("更新检测弹出可点 toast，点击调用 location.reload 一次", async () => {
    // jsdom 无 SW：stub 最小面使 pushSupported() 为真；registration 可控驱动
    const registration: {
      addEventListener: ReturnType<typeof vi.fn>;
      installing: { state: string; addEventListener: ReturnType<typeof vi.fn> } | null;
    } = { addEventListener: vi.fn(), installing: null };
    const listeners = new Map<string, (ev: Event) => void>();
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: {
        addEventListener: (t: string, l: (ev: Event) => void) => listeners.set(t, l),
        removeEventListener: (t: string) => listeners.delete(t),
        // 页面已被旧 SW 接管：installed 时 controller 非空 = 更新而非首装
        controller: { scriptURL: "/imagent/sw.js" },
        register: async () => registration,
      },
    });
    vi.stubGlobal("PushManager", class PushManager {});
    vi.stubGlobal("Notification", class Notification {});
    // jsdom 的 location.reload 不可改写（own 非配置属性）——整体替换 location
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });
    try {
      const fake = mountApp();
      handshake(fake, ["home-mac"]);

      // register().then 微任务挂上 updatefound 监听后，驱动一次完整安装流
      await act(async () => {
        await new Promise((r) => setTimeout(r, 0));
        const updatefound = registration.addEventListener.mock.calls.find(
          ([type]) => type === "updatefound",
        )?.[1] as (ev: Event) => void;
        const installing = { state: "installing", addEventListener: vi.fn() };
        registration.installing = installing;
        updatefound(new Event("updatefound"));
        const statechange = installing.addEventListener.mock.calls.find(
          ([type]) => type === "statechange",
        )?.[1] as (ev: Event) => void;
        installing.state = "installed";
        statechange(new Event("statechange"));
      });

      // 可点形态 toast（button 可访问名 = 提示文案；点击立即重载）。
      const btn = screen.getByRole("button", { name: "新版本已就绪，点击刷新" });
      expect(btn).toBeTruthy();
      await act(async () => {
        btn.click();
      });
      expect(reload).toHaveBeenCalledTimes(1);
    } finally {
      delete (navigator as { serviceWorker?: unknown }).serviceWorker;
      vi.unstubAllGlobals();
    }
  });
});

// ---------------------------------------------------------------------------
// v1.41.7 SW 更新提示常驻（localToast.sticky）：自动消退 effect 对 sticky 形态
// 不起定时器——更新提示过 4s（及更久）仍在，点击照常 reload；普通本地 toast
// （通知开关失败反馈，非 sticky）仍 4s 消退。驱动方式同 v1.39 用例（register
// stub + 微任务冲刷 + updatefound/statechange 手工触发），定时用 fake timers
// （须在挂载前开启——冲刷用的 advanceTimersByTimeAsync 依赖假时钟）。
// ---------------------------------------------------------------------------

describe("v1.41.7 SW 更新提示常驻", () => {
  const UPDATE_TEXT = "新版本已就绪，点击刷新";

  afterEach(() => {
    vi.useRealTimers();
  });

  /** stub 最小 SW 容器（页面已被旧 SW 接管）并驱动一次完整安装流，使更新提示
   *  弹出；返回 location.reload 桩。调用方负责在 finally 里还原全局。 */
  async function mountWithUpdateToast(): Promise<ReturnType<typeof vi.fn>> {
    const registration: {
      addEventListener: ReturnType<typeof vi.fn>;
      installing: { state: string; addEventListener: ReturnType<typeof vi.fn> } | null;
    } = { addEventListener: vi.fn(), installing: null };
    const listeners = new Map<string, (ev: Event) => void>();
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: {
        addEventListener: (t: string, l: (ev: Event) => void) => listeners.set(t, l),
        removeEventListener: (t: string) => listeners.delete(t),
        controller: { scriptURL: "/imagent/sw.js" },
        register: async () => registration,
      },
    });
    vi.stubGlobal("PushManager", class PushManager {});
    vi.stubGlobal("Notification", class Notification {});
    // jsdom 的 location.reload 不可改写（own 非配置属性）——整体替换 location
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });

    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    // register().then 微任务挂上 updatefound 监听后，驱动一次完整安装流
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
      const updatefound = registration.addEventListener.mock.calls.find(
        ([type]) => type === "updatefound",
      )?.[1] as (ev: Event) => void;
      const installing = { state: "installing", addEventListener: vi.fn() };
      registration.installing = installing;
      updatefound(new Event("updatefound"));
      const statechange = installing.addEventListener.mock.calls.find(
        ([type]) => type === "statechange",
      )?.[1] as (ev: Event) => void;
      installing.state = "installed";
      statechange(new Event("statechange"));
    });
    return reload;
  }

  it("更新提示过 4s 仍存在（再过 1 分钟也还在），点击仍 reload 一次", async () => {
    vi.useFakeTimers();
    try {
      const reload = await mountWithUpdateToast();
      const btn = screen.getByRole("button", { name: UPDATE_TEXT });
      expect(btn).toBeTruthy();

      // 4s 到点（非 sticky 本地 toast 的消退时刻）：提示仍在
      act(() => vi.advanceTimersByTime(4000));
      expect(screen.getByRole("button", { name: UPDATE_TEXT })).toBeTruthy();
      act(() => vi.advanceTimersByTime(60_000));
      expect(screen.getByRole("button", { name: UPDATE_TEXT })).toBeTruthy();

      // 点击行为不变：立即重载一次
      await act(async () => {
        btn.click();
      });
      expect(reload).toHaveBeenCalledTimes(1);
    } finally {
      delete (navigator as { serviceWorker?: unknown }).serviceWorker;
      vi.unstubAllGlobals();
    }
  });

  it("普通本地 toast 仍 4s 消退（通知开关失败提示，非 sticky）", async () => {
    vi.useFakeTimers();
    // 预置会话（刷新恢复路径）：侧栏「通知」开关仅会话模式渲染
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * 86_400_000 }),
    );
    mountApp();
    handshake(FakeWebSocket.latest as FakeWebSocket, ["home-mac"]);

    // jsdom 无 PushManager → enablePush 返回 not_supported → 本地 error toast
    // （与更新提示共用 localToast 管道，但不带 sticky）
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "通知 不可用" }));
      await vi.advanceTimersByTimeAsync(0);
    });
    const TEXT = "此环境不支持通知（iOS 请先将本应用添加到主屏幕）";
    expect(screen.getByText(TEXT)).toBeTruthy();
    act(() => vi.advanceTimersByTime(3999));
    expect(screen.getByText(TEXT)).toBeTruthy();
    act(() => vi.advanceTimersByTime(1));
    expect(screen.queryByText(TEXT)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// v1.37 系统未读徽标（navigator.setAppBadge/clearAppBadge）+ 标签页标题未读数：
// 总数 = 全部机器全部 conv 的 unread 求和；feature-detect 缺席静默跳过
// ---------------------------------------------------------------------------

const BASE_TITLE = "imagent · Web 驾驶舱";

describe("v1.37 系统未读徽标 + 标题", () => {
  /** 注入 badge API spy（jsdom 默认无；configurable 便于 afterEach 清理）。 */
  function injectBadge() {
    const setAppBadge = vi.fn().mockResolvedValue(undefined);
    const clearAppBadge = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "setAppBadge", { value: setAppBadge, configurable: true });
    Object.defineProperty(navigator, "clearAppBadge", { value: clearAppBadge, configurable: true });
    return { setAppBadge, clearAppBadge };
  }

  afterEach(() => {
    delete (navigator as { setAppBadge?: unknown }).setAppBadge;
    delete (navigator as { clearAppBadge?: unknown }).clearAppBadge;
    document.title = BASE_TITLE;
  });

  it("无 badge API（jsdom）：静默跳过不炸；unread 变化驱动 title 计数/恢复", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    expect(document.title).toBe(BASE_TITLE);
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台消息", machine: "home-mac" });
    });
    expect(document.title).toBe("(1) imagent · Web 驾驶舱");
    // 切到该会话（select_conv 清未读）→ 标题恢复基准（手动名会话显示名恒剥前缀 work）
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    expect(document.title).toBe(BASE_TITLE);
  });

  it("badge API 存在：unread>0 setAppBadge(n) 递增、清零 clearAppBadge；登出再清一次", async () => {
    const spy = injectBadge();
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台消息", machine: "home-mac" });
    });
    expect(spy.setAppBadge).toHaveBeenCalledWith(1);
    expect(document.title).toBe("(1) imagent · Web 驾驶舱");
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "再来一条", machine: "home-mac" });
    });
    expect(spy.setAppBadge).toHaveBeenLastCalledWith(2);
    // 切会话清未读 → clearAppBadge + 标题基准（挂载时 unread=0 已调过一次，
    // 用增量断言）
    const clearsBefore = spy.clearAppBadge.mock.calls.length;
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    expect(spy.clearAppBadge.mock.calls.length).toBe(clearsBefore + 1);
    expect(document.title).toBe(BASE_TITLE);
    // 登出：登出流程再挂一次 clearAppBadge（幂等）+ 标题复位
    fireEvent.click(screen.getByRole("button", { name: "退出" }));
    expect(spy.clearAppBadge.mock.calls.length).toBe(clearsBefore + 2);
    expect(document.title).toBe(BASE_TITLE);
  });
});

// ---------------------------------------------------------------------------
// v1.37 侧栏「运行中」状态：card streaming 条目 → 会话项呼吸点；终态后消失；
// pending 审批不算运行态
// ---------------------------------------------------------------------------

describe("v1.37 侧栏运行中状态", () => {
  const CARD = (opts: { messageId: string; terminal: string; convId?: string }) => ({
    v: 1,
    type: "card",
    convId: opts.convId ?? "web:default",
    messageId: opts.messageId,
    machine: "home-mac",
    state: opts.terminal === "running" ? "streaming" : "done",
    card: {
      text: `任务 ${opts.messageId}`,
      tool_calls: [],
      thoughts: [],
      todos: [],
      phase: "outputting",
      run_secs: 1,
      terminal: opts.terminal,
    },
  });

  it("card streaming → 该会话项渲染运行点；同卡终态补丁后消失", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv(CARD({ messageId: "m1", terminal: "running" }));
    });
    expect(screen.getAllByTitle("运行中").length).toBe(1);
    act(() => {
      fake.recv(CARD({ messageId: "m1", terminal: "done" }));
    });
    expect(screen.queryByTitle("运行中")).toBeNull();
  });

  it("非当前会话 streaming 点亮；pending 审批不算运行态", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv(CARD({ messageId: "m2", terminal: "running", convId: "web:work" }));
      fake.recv({
        v: 1,
        type: "approval_request",
        convId: "web:default",
        requestId: "r1",
        tool: "Bash",
        inputSummary: "ls",
        machine: "home-mac",
      });
    });
    // 仅 web:work 一处运行点；审批弹层渲染但不产生运行点
    expect(screen.getAllByTitle("运行中").length).toBe(1);
    expect(screen.getByRole("alertdialog", { name: "审批请求 Bash" })).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// v1.37 审批卡倒计时（timeoutSecs，本地 1s 倒数、0 封底）+ 「前往会话」跳转
// （先 select_machine 再 select_conv，弹层不关闭）
// ---------------------------------------------------------------------------

describe("v1.37 审批倒计时 + 前往会话", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  const APPROVAL = (extra: Record<string, unknown> = {}) => ({
    v: 1,
    type: "approval_request",
    convId: "web:default",
    requestId: "r1",
    tool: "Bash",
    inputSummary: "rm -rf /tmp/x",
    machine: "home-mac",
    ...extra,
  });

  it("带 timeoutSecs：渲染剩余秒数随时间递减；到 0 显「即将超时」不再负数", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv(APPROVAL({ timeoutSecs: 10 }));
    });
    expect(screen.getByText("10s")).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(screen.getByText("9s")).toBeTruthy();
    act(() => {
      vi.advanceTimersByTime(9_000);
    });
    expect(screen.getByText("即将超时")).toBeTruthy();
    // 继续推进封底不变（不出现负数/其它形态）
    act(() => {
      vi.advanceTimersByTime(5_000);
    });
    expect(screen.getByText("即将超时")).toBeTruthy();
    expect(screen.queryByText(/^-/)).toBeNull();
  });

  it("timeoutSecs 缺席（旧网关）：不渲染倒计时（降级为既有形态）", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv(APPROVAL());
    });
    expect(screen.getByRole("alertdialog", { name: "审批请求 Bash" })).toBeTruthy();
    expect(screen.queryByText(/^\d+s$/)).toBeNull();
    expect(screen.queryByText("即将超时")).toBeNull();
  });

  it("「前往会话」：切到审批来源机器+会话；审批条保留（仅视口切换）", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac", "work-mac"]);
    act(() => {
      fake.recv({ ...APPROVAL(), convId: "web:proj", machine: "work-mac", requestId: "r9" });
    });
    // 当前在 home-mac（machines 帧校正），审批来自 work-mac
    expect(screen.getByText("已连接机器 home-mac")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "前往会话" }));
    // 视口切到 work-mac + web:proj（侧栏高亮）
    const listbox = screen.getByRole("listbox", { name: "机器列表" });
    expect(listbox.querySelector('[aria-selected="true"]')?.textContent).toContain("work-mac");
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("proj");
    // 审批条仍在（不关闭——回来还能处理）
    expect(screen.getByRole("alertdialog", { name: "审批请求 Bash" })).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// v1.40.1 前后台切换消息恢复（App 层）：① visible 兜底补拉——当前会话补一对
// sync + history_query（与 ws 层探活独立双保险）；③ queryHistory 在 E2E
// confirming（fail-closed）窗口拒发 → 指数退避重试，信任激活后重试成功。
// ---------------------------------------------------------------------------

describe("App 切回前台兜底补拉 + queryHistory 拒发重试（v1.40.1）", () => {
  afterEach(() => {
    vi.useRealTimers();
    // 还原被用例覆写的 visibilityState（实例级 shadow，同 ws.test.ts 模式）
    delete (document as { visibilityState?: string }).visibilityState;
  });

  /** 测试网关（与「App E2E（v1.4）」describe 内同名助手同构——该助手为
   *  describe 局部，此处复制一份；长期 X25519 + ed25519 身份密钥）。 */
  function makeGateway() {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    const ackFor = (hello: { ek: string; n1: string }, machine: string) => {
      const n2 = bytesToHex(randomBytes(32));
      const msg = new Uint8Array(128);
      msg.set(hexToBytes(hello.ek), 0);
      msg.set(hexToBytes(gk), 32);
      msg.set(hexToBytes(hello.n1), 64);
      msg.set(hexToBytes(n2), 96);
      return {
        v: 1 as const,
        type: "e2e_ack" as const,
        machine,
        gk,
        n2,
        sig: bytesToHex(ed25519.sign(msg, idPriv)),
        ik,
      };
    };
    const gatewayKey = (hello: { ek: string; n1: string }, n2: string) => {
      const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
      const ikm = new Uint8Array(96);
      ikm.set(shared, 0);
      ikm.set(hexToBytes(hello.n1), 32);
      ikm.set(hexToBytes(n2), 64);
      return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
    };
    return { gk, ik, ackFor, gatewayKey };
  }

  const setVisibility = (state: string): void => {
    Object.defineProperty(document, "visibilityState", { value: state, configurable: true });
  };
  const goVisible = (): void => {
    setVisibility("visible");
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
  };
  const sentCount = (fake: FakeWebSocket, frame: unknown): number =>
    fake.sent.filter((s) => s === JSON.stringify(frame)).length;

  it("visible 且 ready：当前会话补发一对 sync + history_query（幂等兜底）", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    const sync = { v: 1, type: "sync", machine: "home-mac" };
    const query = { v: 1, type: "history_query", convId: "web:default", since: 0, machine: "home-mac" };
    // 锚点：ready 时机既有 effect 已各发一次（e2e pending 窗口明文直发）
    expect(sentCount(fake, sync)).toBe(1);
    expect(sentCount(fake, query)).toBe(1);
    goVisible();
    // visible 兜底：再补一对（空增量便宜，网关按 seq 幂等）
    expect(sentCount(fake, sync)).toBe(2);
    expect(sentCount(fake, query)).toBe(2);
  });

  it("未 ready 或 hidden：visible 派发不动作", () => {
    const fake = mountApp();
    // 未 ready（连接中）：visible 派发不补拉
    act(() => {
      fake.emit("open");
    });
    goVisible();
    expect(fake.sent.some((s) => s.includes('"history_query"'))).toBe(false);
    // hidden：ready 后派发也不动作
    handshake(fake, ["home-mac"]);
    const sync = { v: 1, type: "sync", machine: "home-mac" };
    expect(sentCount(fake, sync)).toBe(1); // 仅既有 effect 的那次
    setVisibility("hidden");
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(sentCount(fake, sync)).toBe(1); // 不变
  });

  it("queryHistory fail-closed 拒发 → 退避重试；E2E 激活后重试成功（enc 直发）", () => {
    vi.useFakeTimers();
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    // 两台机器同帧上线 → 各自握手（pending 窗口明文直发）
    act(() => {
      fake.recv({ v: 1, type: "machines", online: ["home-mac", "work-mac"] });
    });
    const hellos = fake.sent
      .filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello")
      .map((s) => JSON.parse(s) as { ek: string; n1: string });
    expect(hellos).toHaveLength(2);
    const ackHome = gw.ackFor(hellos[0], "home-mac");
    const ackWork = gw.ackFor(hellos[1], "work-mac");
    act(() => {
      fake.recv(ackHome);
      fake.recv(ackWork);
    });
    // 两台均 confirming（TOFU 弹层，首台展示）→ fail-closed：业务帧拒发
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();

    // 切到 work-mac：history effect 触发 queryHistory → 拒发（静默 false）
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    const queryWork = {
      v: 1,
      type: "history_query",
      convId: "web:default",
      since: 0,
      machine: "work-mac",
    };
    expect(sentCount(fake, queryWork)).toBe(0); // fail-closed 未发任何明文
    act(() => {
      vi.advanceTimersByTime(800); // 重试 1：仍 confirming → 拒发
    });
    act(() => {
      vi.advanceTimersByTime(1600); // 重试 2：仍 confirming → 拒发
    });
    // 逐台信任 → 两台 active。v1.41.9：每次信任后该机器以 pin 身份重新握手，
    // 网关应答新握手才激活、弹层才切到下一台（在途期间 history_query 仍被拒）
    const trust = (machine: string) => {
      fireEvent.click(
        within(screen.getByRole("dialog", { name: "确认机器加密指纹" })).getByRole("button", {
          name: "信任并启用加密",
        }),
      );
      const raw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").at(-1);
      const hello = JSON.parse(raw ?? "null") as { ek: string; n1: string };
      const ack = gw.ackFor(hello, machine);
      act(() => {
        fake.recv(ack);
      });
      return { hello, ack };
    };
    trust("home-mac");
    const redoneWork = trust("work-mac");
    act(() => {
      vi.advanceTimersByTime(2400); // 重试 3：已 active → enc 直发成功，重试停止
    });
    // fail-closed 全程未破：从无明文 history_query；激活后恰好一枚 enc 内帧
    expect(sentCount(fake, queryWork)).toBe(0);
    const encs = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "enc");
    const inner = encs
      .map((s) => JSON.parse(s) as { machine: string; box: string })
      .map((enc) =>
        JSON.parse(openBox(gw.gatewayKey(redoneWork.hello, redoneWork.ack.n2), enc.box) ?? "null"),
      )
      .find((f) => f?.type === "history_query") as Record<string, unknown> | undefined;
    expect(inner).toEqual({ v: 1, type: "history_query", convId: "web:default", since: 0, machine: "work-mac" });
  });
});

// ---------------------------------------------------------------------------
// v1.41.8 peer 会话用户自发文本：真机两种表现的端到端回归——网关把用户发给
// 本地窗口的文本镜像回 `peer:<窗口名>` 会话时带 `self`（此前是普通 text 帧，
// 被渲染成 imagent 的发言）。
// ---------------------------------------------------------------------------

describe("peer 会话用户自发文本（v1.41.8）", () => {
  const M = "home-mac";

  /** 当前会话消息流（<main>）里某正文所在气泡的归属（按出现顺序）：user =
   *  右侧用户气泡（emerald 底），agent = 左侧 agent 气泡。侧栏预览与状态条
   *  标题在 main 之外，不计。 */
  function bubbleKinds(text: string): ("user" | "agent")[] {
    return within(screen.getByRole("main"))
      .getAllByText(text)
      .map((el) => (el.closest(".bg-emerald-600") ? "user" : "agent"));
  }

  /** 网关出站 text 帧归当前机器空间（多机模式帧恒带 machine）。 */
  const gwText = (fake: FakeWebSocket, convId: string, text: string, extra: Record<string, unknown> = {}) => {
    act(() => {
      fake.recv({ v: 1, type: "text", convId, text, at: 1795528250098, machine: M, ...extra });
    });
  };

  /** 输入框打字 + 点「发送」（命令文本会唤起命令面板，Enter 被面板接管，故点按钮）。 */
  const send = (text: string) => {
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: text } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
  };

  it("peer 会话直接打字：本机回显 + 网关镜像（self）只出一个用户气泡，不再双显；窗口回复仍是 agent 气泡", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    // 建立会话的欢迎文本是系统提示（普通 text → agent 气泡），让会话出现在侧栏。
    gwText(fake, "peer:win-a", "已建立与窗口「win-a」的会话。");
    fireEvent.click(screen.getByRole("button", { name: /^win-a/ }));

    send("帮我跑下构建");
    expect(
      sentMessage(fake, { v: 1, type: "message", convId: "peer:win-a", text: "帮我跑下构建", machine: M }),
    ).toBe(true);
    expect(bubbleKinds("帮我跑下构建")).toEqual(["user"]); // 本机回显
    expect(bubbleKinds("已建立与窗口「win-a」的会话。")).toEqual(["agent"]);

    // 网关把同一句镜像回来（v1.41.8 带 self）→ 与回显去重，不冒出第二个气泡。
    gwText(fake, "peer:win-a", "帮我跑下构建", { self: true });
    expect(bubbleKinds("帮我跑下构建")).toEqual(["user"]);

    // 窗口的回复（普通 text，无 self）照旧是 agent 气泡。
    gwText(fake, "peer:win-a", "构建通过，无告警。");
    expect(bubbleKinds("构建通过，无告警。")).toEqual(["agent"]);
    expect(bubbleKinds("帮我跑下构建")).toEqual(["user"]);
  });

  it("/to 派活：进入 peer 会话看到的是自己的用户气泡（不是 imagent 的发言）；回执仍落原会话为 agent 文本", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    const cmd = "/to win-a 你目前的待办项有哪些？";
    send(cmd);
    expect(sentMessage(fake, { v: 1, type: "message", convId: "web:default", text: cmd, machine: M })).toBe(true);

    // 网关：派活正文镜像进 peer:win-a（self）+ 当前会话回执（普通 text）。
    gwText(fake, "peer:win-a", "你目前的待办项有哪些？", { self: true });
    gwText(fake, "web:default", "✅ 已投递至 win-a。");
    // 原会话：命令回显（user）+ 回执（agent）。
    expect(bubbleKinds(cmd)).toEqual(["user"]);
    expect(bubbleKinds("✅ 已投递至 win-a。")).toEqual(["agent"]);

    // 进入 peer 会话（侧栏按窗口名显示）。
    fireEvent.click(screen.getByRole("button", { name: /^win-a/ }));
    expect(bubbleKinds("你目前的待办项有哪些？")).toEqual(["user"]);
  });

  it("旧网关兼容：镜像帧不带 self 仍按 agent 文本渲染（既有行为不变）", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    gwText(fake, "peer:win-a", "已建立与窗口「win-a」的会话。");
    fireEvent.click(screen.getByRole("button", { name: /^win-a/ }));
    gwText(fake, "peer:win-a", "帮我跑下构建");
    expect(bubbleKinds("帮我跑下构建")).toEqual(["agent"]);
  });
});

// ---------------------------------------------------------------------------
// v1.42 驾驶舱核心交互修复批（App 层）：停止按钮落到当前会话 / 待审批快照 /
// 中继瞬态关闭不再登出 / 草稿按「机器 + 会话」隔离。
// ---------------------------------------------------------------------------

describe("v1.42 停止按钮落到当前会话（control 帧 convId）", () => {
  const stopFrame = (convId: string) =>
    JSON.stringify({ v: 1, type: "control", cmd: "stop", convId, machine: "home-mac" });

  /** 挂载并让 web:work 出现在侧栏（非默认会话），返回活跃 socket。 */
  function mountWithWorkConv(): FakeWebSocket {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台会话消息", machine: "home-mac" });
    });
    return fake;
  }

  it("非默认会话点「停止」→ control 帧 convId 是当前会话（不是默认会话）", () => {
    const fake = mountWithWorkConv();
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    expect(fake.sent).toContain(stopFrame("web:work"));
    expect(fake.sent).not.toContain(stopFrame("web:default"));
  });

  it("默认会话点「停止」→ convId 为默认会话 web:default", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    expect(fake.sent).toContain(stopFrame("web:default"));
    // 只发了这一条 control 帧
    expect(fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "control")).toEqual([
      stopFrame("web:default"),
    ]);
  });

  it("切会话后 convId 跟随当前会话（work → default → work）", () => {
    const fake = mountWithWorkConv();
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    // default 会话发过首条消息后按消息文本定位（既有约定）；此处 default 尚无消息
    fireEvent.click(screen.getByRole("button", { name: /^default/ }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    fireEvent.click(screen.getByRole("button", { name: /^work/ }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    const stops = fake.sent
      .filter((s) => (JSON.parse(s) as { type: string }).type === "control")
      .map((s) => (JSON.parse(s) as { convId?: string }).convId);
    expect(stops).toEqual(["web:work", "web:default", "web:work"]);
  });

  it("自动会话（web:s-xxxx）点「停止」→ convId 为该会话完整形态", () => {
    const fake = mountApp();
    handshake(fake, ["home-mac"]);
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:s-ab12", text: "自动会话历史首条", machine: "home-mac" });
    });
    fireEvent.click(screen.getByRole("button", { name: /^自动会话历史首条/ }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    expect(fake.sent).toContain(stopFrame("web:s-ab12"));
  });
});

// ---------------------------------------------------------------------------
// v1.42 中继瞬态关闭不再登出：close 1008 "rate limited" 走自动重连（凭证仍在、
// 留在主界面）；新 reason（too many sessions / pairing expired / too many
// pairings）在登录页给出可操作的提示，会话数超限不清本地凭证。
// ---------------------------------------------------------------------------

describe("v1.42 中继瞬态关闭不再登出", () => {
  const DAY = 86_400_000;
  /** 会话凭证形态（刷新恢复路径）。 */
  const sessionJson = () =>
    JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * DAY });
  const sessionAck = () => ({
    v: 1,
    type: "hello_ack",
    role: "user",
    session: { id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * DAY },
  });
  /** 登录页是否在显示（USER_TOKEN 输入框在场 = 已回到登录页）。 */
  const onLoginPage = (): boolean => screen.queryByLabelText("访问令牌（USER_TOKEN）") !== null;
  /** 本地会话凭证是否仍在（按 id + token 判定——expiresAt 会被每次 hello_ack
   *  携带的 session 刷新，是既有行为，不能拿整串字节比「未被改动」）。 */
  const sessionKept = (): boolean =>
    JSON.parse(localStorage.getItem("imagent.session") ?? "null")?.token === "st-r";

  /** 会话形态挂载并握手到 ready，返回活跃 socket。 */
  function mountSessionReady(): FakeWebSocket {
    localStorage.setItem("imagent.session", sessionJson());
    render(<App />);
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.recv(sessionAck());
    });
    return fake;
  }

  afterEach(() => {
    vi.useRealTimers();
  });

  it("close 1008 rate limited → 自动重连、仍在主界面、会话凭证未被清除", () => {
    // 重连退避是 1s 定时器：挂载前切假时钟（WsManager 的定时器须落在假时钟上）
    vi.useFakeTimers();
    const fake = mountSessionReady();
    expect(screen.getByText("已连接")).toBeTruthy();
    expect(sessionKept()).toBe(true);

    // 真机帧序：限流前中继先发 error{rate_limited}，随后 close 1008
    act(() => {
      fake.recv({ v: 1, type: "error", code: "rate_limited" });
      fake.emit("close", { code: 1008, reason: "rate limited" });
    });
    // error 帧仍走既有 toast 文案
    expect(screen.getByText("发送过快，已被限流断开")).toBeTruthy();
    expect(screen.getByText("重连中")).toBeTruthy();
    expect(onLoginPage()).toBe(false);
    expect(sessionKept()).toBe(true);

    // 退避等待期间：冲刷定时器后仍是「未登出」（负向断言不早于被测路径）
    act(() => {
      vi.advanceTimersByTime(500);
    });
    expect(onLoginPage()).toBe(false);
    expect(FakeWebSocket.latest).toBe(fake);

    // 退避到点 → 新连接（hello 用 session 形态）→ hello_ack → 恢复已连接
    act(() => {
      vi.advanceTimersByTime(500);
    });
    const next = FakeWebSocket.latest as FakeWebSocket;
    expect(next).not.toBe(fake);
    act(() => {
      next.emit("open"); // hello 在连接 open 后才发出
    });
    // 重连 hello 用 session 形态（凭证仍在 → 不需要用户重新登录）
    expect(next.sent[0]).toBe(JSON.stringify({ v: 1, type: "hello", session: "st-r" }));
    act(() => {
      next.recv(sessionAck());
    });
    expect(screen.getByText("已连接")).toBeTruthy();
    expect(screen.queryByText("重连中")).toBeNull();
    // 恢复后再冲刷 5s（toast 消退 / 缓存落盘等定时器）：仍无登录页、凭证原样
    act(() => {
      vi.advanceTimersByTime(5_000);
    });
    expect(onLoginPage()).toBe(false);
    expect(sessionKept()).toBe(true);
    expect(screen.queryByRole("alert")).toBeNull(); // 没有任何认证失败提示
  });

  it("对照：同一挂载方式下 close 1008 auth failed → 仍回登录页并清凭证（既有行为不变）", () => {
    const fake = mountSessionReady();
    act(() => {
      fake.emit("close", { code: 1008, reason: "auth failed" });
    });
    expect(onLoginPage()).toBe(true);
    expect(localStorage.getItem("imagent.session")).toBeNull();
  });

  it("close 1008 too many sessions → 登录页出现引导提示；本地会话凭证未被清", () => {
    const fake = mountSessionReady();
    act(() => {
      fake.emit("close", { code: 1008, reason: "too many sessions" });
    });
    expect(onLoginPage()).toBe(true);
    expect(screen.getByRole("alert").textContent).toBe(
      "登录会话数已达上限，请先在已登录设备的「管理 → 已登录设备」里吊销不用的会话，再重新登录",
    );
    expect(sessionKept()).toBe(true);
  });

  it("USER_TOKEN 首登被拒（too many sessions）→ 令牌保留，腾出名额后可直接重试", () => {
    render(<App />);
    fireEvent.change(screen.getByLabelText("访问令牌（USER_TOKEN）"), { target: { value: "tok-9" } });
    fireEvent.click(screen.getByRole("button", { name: "连接" }));
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.emit("close", { code: 1008, reason: "too many sessions" });
    });
    expect(onLoginPage()).toBe(true);
    expect(screen.getByRole("alert").textContent).toContain("登录会话数已达上限");
    expect(localStorage.getItem("imagent.userToken")).toBe("tok-9");
  });

  /** 给当前会话留一条已落盘的文字草稿（防抖 500ms 用假时钟冲掉），返回
   *  localStorage 里的原文。调用方须已切假时钟。 */
  const seedDraft = (text: string): string | null => {
    const input = () => screen.getByLabelText("消息输入框");
    fireEvent.change(input(), { target: { value: text } });
    act(() => {
      vi.advanceTimersByTime(500);
    });
    const raw = localStorage.getItem("imagent.drafts");
    // 正向锚点：草稿确实落了盘（否则下面的「仍在 / 已清」没有判别力）
    expect(raw).toContain(text);
    return raw;
  };

  it("too many sessions 不清本机数据：已落盘的草稿原样保留、historyStore.clear 未被调用、凭证仍在；没来得及落盘的最后几句也保住", () => {
    vi.useFakeTimers();
    const clearSpy = vi.spyOn(historyStore, "clear").mockResolvedValue();
    try {
      const fake = mountApp();
      handshake(fake, ["home-mac"]);
      const draft = seedDraft("吊销会话期间不能丢的草稿");
      // 再输入一句、不冲防抖：此刻它只在内存 store 里，尚未落盘
      fireEvent.change(screen.getByLabelText("消息输入框"), {
        target: { value: "吊销会话期间不能丢的草稿 最后几句" },
      });
      act(() => {
        fake.emit("close", { code: 1008, reason: "too many sessions" });
      });
      expect(onLoginPage()).toBe(true);
      expect(localStorage.getItem("imagent.drafts")).toBe(draft); // 落了盘的那份原样
      expect(clearSpy).not.toHaveBeenCalled();
      expect(localStorage.getItem("imagent.userToken")).toBe("test-token");
      // 冲刷 5s：内存里的草稿没有被清，排着的防抖照常把它落盘（最后几句也在）
      act(() => {
        vi.advanceTimersByTime(5_000);
      });
      expect(Object.values(JSON.parse(localStorage.getItem("imagent.drafts") ?? "null"))).toEqual([
        "吊销会话期间不能丢的草稿 最后几句",
      ]);
      expect(clearSpy).not.toHaveBeenCalled();
    } finally {
      clearSpy.mockRestore();
    }
  });

  it.each(["auth failed", "session expired", "session revoked"])(
    "回归·close 1008 %s → 草稿与本地消息缓存照旧被清（凭证不可用 = 退出路径）",
    (reason) => {
      vi.useFakeTimers();
      const clearSpy = vi.spyOn(historyStore, "clear").mockResolvedValue();
      try {
        const fake = mountSessionReady();
        handshake(fake, ["home-mac"]);
        seedDraft("凭证失效后应随之清掉的草稿");
        // 再输入一句、不冲防抖：排着的落盘定时器也应随清空一并取消
        fireEvent.change(screen.getByLabelText("消息输入框"), {
          target: { value: "凭证失效后应随之清掉的草稿 最后几句" },
        });
        act(() => {
          fake.emit("close", { code: 1008, reason });
        });
        expect(onLoginPage()).toBe(true);
        expect(localStorage.getItem("imagent.drafts")).toBeNull(); // 内存与 storage 一并清
        expect(clearSpy).toHaveBeenCalledTimes(1);
        expect(sessionKept()).toBe(false);
        // 冲刷：迟到的防抖不会把草稿写回
        act(() => {
          vi.advanceTimersByTime(5_000);
        });
        expect(localStorage.getItem("imagent.drafts")).toBeNull();
      } finally {
        clearSpy.mockRestore();
      }
    },
  );

  it.each([
    ["pairing expired", "配对码已过期，请重新发起配对"],
    ["too many pairings", "同时进行的配对请求过多，请稍后再试"],
  ])("配对中 close 1008 %s → 登录页提示「%s」（不再落入泛化的「配对未成功」）", (reason, notice) => {
    render(<App />);
    fireEvent.click(screen.getByRole("button", { name: /我是新设备/ }));
    fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
    const fake = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      fake.emit("open");
      fake.emit("close", { code: 1008, reason });
    });
    expect(onLoginPage()).toBe(true);
    expect(screen.getByRole("alert").textContent).toBe(notice);
  });

  it.each([
    ["pairing expired", "配对码已过期，请重新发起配对"],
    ["too many pairings", "同时进行的配对请求过多，请稍后再试"],
  ])("非配对连接 close 1008 %s → 同样给出「%s」", (reason, notice) => {
    const fake = mountSessionReady();
    act(() => {
      fake.emit("close", { code: 1008, reason });
    });
    expect(screen.getByRole("alert").textContent).toBe(notice);
  });
});

// ---------------------------------------------------------------------------
// v1.42 待审批快照（App 层）：ready 后对全部在线机器各查一次（E1）+ 该机器加密
// 会话建立后再查一次（E2）；approvals 帧在 onFrame 拦截转 approvals_snapshot。
// 修复：离线期间到达的审批重连后无按钮；离开期间已超时 / 已解决的审批挂到刷新。
// ---------------------------------------------------------------------------

describe("v1.42 待审批快照", () => {
  const M = "home-mac";
  /** 契约示例里的快照条目 / approvals 帧。 */
  const item = (extra: Record<string, unknown> = {}) => ({
    convId: "web:s-192d",
    requestId: "req-1",
    tool: "Bash",
    inputSummary: "npm test",
    timeoutSecs: 731,
    ...extra,
  });
  const APPROVALS = (machine: string, items: unknown[]) => ({ v: 1, type: "approvals", machine, items });
  const APPROVAL_REQ = (requestId: string, convId = "web:default") => ({
    v: 1,
    type: "approval_request",
    convId,
    requestId,
    tool: "Bash",
    inputSummary: "rm -rf /tmp/x",
    machine: M,
  });
  /** 出站 approvals_query（明文形态）的目标机器序列。 */
  const approvalQueries = (fake: FakeWebSocket): string[] =>
    fake.sent
      .filter((s) => (JSON.parse(s) as { type: string }).type === "approvals_query")
      .map((s) => (JSON.parse(s) as { machine?: string }).machine ?? "");
  const dialog = (tool = "Bash") => screen.queryByRole("alertdialog", { name: `审批请求 ${tool}` });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("① ready + 收到 machines 帧 → 对**全部**在线机器各发一条 approvals_query（不只是当前机器）", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    // hello_ack 后尚未收到 machines 帧：不发（单机模式旧网关不认识该帧）
    expect(approvalQueries(fake)).toEqual([]);
    act(() => {
      fake.recv({ v: 1, type: "machine", online: false });
      fake.recv({ v: 1, type: "machines", online: [M, "work-mac", "office-mac"] });
    });
    // 当前机器是 home-mac（自动选中首台）；三台各一条
    expect(approvalQueries(fake)).toEqual([M, "work-mac", "office-mac"]);
    // 帧形状即契约原文
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "approvals_query", machine: "work-mac" }));
  });

  it("①b 切换当前机器 / machines 帧重排不重发：待审批集合不随「当前看哪台机器」变化", () => {
    const fake = mountApp();
    handshake(fake, [M, "work-mac"]);
    expect(approvalQueries(fake)).toEqual([M, "work-mac"]);
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    expect(approvalQueries(fake)).toEqual([M, "work-mac"]); // 切机不触发
    // machines 帧重排（任一网关上下线都会再推）：同样不触发
    act(() => {
      fake.recv({ v: 1, type: "machines", online: ["work-mac", M] });
    });
    expect(approvalQueries(fake)).toEqual([M, "work-mac"]);
  });

  it("①c 单机模式（旧 relay，无 machines 帧）不发 approvals_query", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
      fake.recv({ v: 1, type: "machine", online: true });
    });
    expect(screen.getByText("已连接")).toBeTruthy();
    expect(approvalQueries(fake)).toEqual([]);
  });

  it("② 收到 approvals 帧 → 审批面板出现对应条目（按钮 / 机器徽标 / 会话 / 倒计时取剩余秒数）；拒绝原样回传", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    act(() => {
      fake.recv(APPROVALS(M, [item()]));
    });
    const box = dialog();
    expect(box).toBeTruthy();
    expect(within(box!).getByText(M)).toBeTruthy();
    expect(within(box!).getByText("s-192d")).toBeTruthy(); // 会话徽标（自动会话显示短名）
    expect(within(box!).getByText("npm test")).toBeTruthy();
    expect(within(box!).getByText("731s")).toBeTruthy(); // 剩余秒数，不是 900s 预算
    fireEvent.click(within(box!).getByRole("button", { name: "拒绝" }));
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "approval_response", convId: "web:s-192d", requestId: "req-1", decision: "deny", machine: M }),
    );
    expect(dialog()).toBeNull(); // 本地乐观移除
  });

  it("②b 快照引用的会话此前不存在 → 出现在侧栏（物化），可点「前往会话」切过去", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    act(() => {
      fake.recv(APPROVALS(M, [item({ convId: "web:proj", requestId: "r9" })]));
    });
    const nav = screen.getByRole("navigation", { name: "会话列表" });
    expect(within(nav).getByRole("button", { name: /^proj/ })).toBeTruthy();
    fireEvent.click(within(dialog()!).getByRole("button", { name: "前往会话" }));
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("proj");
  });

  it("③ 离线期间错过 approval_request：断线重连后由 approvals 快照恢复审批面板（无需手打 y/n）", () => {
    // 重连退避 1s 是定时器：挂载前切假时钟
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    expect(approvalQueries(fake)).toEqual([M]);
    expect(dialog()).toBeNull();
    // 断线（iOS 挂起 / 网络切换）；离线期间网关发起了审批——approval_request 本连接收不到
    act(() => {
      fake.emit("close", { code: 1006 });
    });
    expect(screen.getByText("重连中")).toBeTruthy();
    expect(dialog()).toBeNull();
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    const next = FakeWebSocket.latest as FakeWebSocket;
    expect(next).not.toBe(fake);
    handshake(next, [M]);
    expect(screen.getByText("已连接")).toBeTruthy();
    // 重连 ready → 再查一次（此刻时间晚于断线前）
    expect(approvalQueries(next)).toEqual([M]);
    // 网关回快照：离线期间到达的那条审批在网关侧仍待处理
    act(() => {
      next.recv(APPROVALS(M, [item({ requestId: "missed-1", convId: "web:default" })]));
    });
    expect(dialog()).toBeTruthy();
    expect(within(dialog()!).getByText("npm test")).toBeTruthy();
    fireEvent.click(within(dialog()!).getByRole("button", { name: "允许" }));
    expect(next.sent).toContain(
      JSON.stringify({ v: 1, type: "approval_response", convId: "web:default", requestId: "missed-1", decision: "allow", machine: M }),
    );
  });

  it("④ 离开期间已超时 / 已解决的审批：重连后空快照把陈旧 pending 清出面板", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    // 离线前：面板上挂着两条审批（两条同名，按 alertdialog 全量计数）
    expect(screen.queryAllByRole("alertdialog")).toHaveLength(0);
    act(() => {
      fake.recv(APPROVAL_REQ("stale-1"));
      fake.recv(APPROVAL_REQ("stale-2", "web:other"));
    });
    expect(screen.queryAllByRole("alertdialog")).toHaveLength(2);
    // 断开 → 退避 → 新连接 ready（此刻的查询晚于两条旧审批的到达）
    act(() => {
      fake.emit("close", { code: 1006 });
      vi.advanceTimersByTime(1000);
    });
    const next = FakeWebSocket.latest as FakeWebSocket;
    handshake(next, [M]);
    expect(approvalQueries(next)).toEqual([M]);
    // 离开期间两条都已超时：网关回空集合 → 面板清空
    act(() => {
      next.recv(APPROVALS(M, []));
    });
    expect(screen.queryAllByRole("alertdialog")).toHaveLength(0);
    // 不编造终态记录：消息流里没有「已批准 / 已拒绝 / 已取消」之类的审批记录
    expect(screen.queryByText(/已(批准|拒绝|取消)/)).toBeNull();
    expect(screen.getByLabelText("消息输入框")).toBeTruthy();
  });

  it("④b 查询之后才到达的实时审批不被（更早那次查询的）快照误删", () => {
    // 「查询之后到达」的判定是 at > queriedAt（毫秒）：测试里查询发出与实时帧
    // 到达若在同一同步段会落进同一毫秒——真实链路上二者之间隔着一次完整
    // 往返，这里用假时钟把这段间隔显式造出来。
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    expect(approvalQueries(fake)).toEqual([M]); // E1 发出
    act(() => {
      vi.advanceTimersByTime(5); // 网络往返
    });
    // 查询发出之后网关才发起的审批：实时帧到达（此刻晚于查询）
    act(() => {
      fake.recv(APPROVAL_REQ("fresh-1"));
    });
    expect(dialog()).toBeTruthy();
    // 对应那次（更早）查询的应答到达：快照生成时 fresh-1 还不存在 → 空集合。
    // 它是查询之后到达的实时条目，必须保留（否则面板上的按钮凭空消失）。
    act(() => {
      vi.advanceTimersByTime(5);
      fake.recv(APPROVALS(M, []));
    });
    expect(dialog()).toBeTruthy();
    expect(within(dialog()!).getByText("rm -rf /tmp/x")).toBeTruthy();
    // 用户仍可正常处理它
    fireEvent.click(within(dialog()!).getByRole("button", { name: "拒绝" }));
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "approval_response", convId: "web:default", requestId: "fresh-1", decision: "deny", machine: M }),
    );
    expect(dialog()).toBeNull();
  });

  /** 测试网关（与「App E2E（v1.4）」describe 内同名助手同构——该助手为
   *  describe 局部，此处复制一份；长期 X25519 + ed25519 身份密钥）。 */
  function makeGateway() {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    const ackFor = (hello: { ek: string; n1: string }, machine: string) => {
      const n2 = bytesToHex(randomBytes(32));
      const msg = new Uint8Array(128);
      msg.set(hexToBytes(hello.ek), 0);
      msg.set(hexToBytes(gk), 32);
      msg.set(hexToBytes(hello.n1), 64);
      msg.set(hexToBytes(n2), 96);
      return {
        v: 1 as const,
        type: "e2e_ack" as const,
        machine,
        gk,
        n2,
        sig: bytesToHex(ed25519.sign(msg, idPriv)),
        ik,
      };
    };
    const gatewayKey = (hello: { ek: string; n1: string }, n2: string) => {
      const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
      const ikm = new Uint8Array(96);
      ikm.set(shared, 0);
      ikm.set(hexToBytes(hello.n1), 32);
      ikm.set(hexToBytes(n2), 64);
      return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
    };
    return { gk, ik, ackFor, gatewayKey };
  }

  /** 从 fake.sent 里取最近一条 e2e_hello（无则抛）。 */
  function lastHelloFor(fake: FakeWebSocket): { ek: string; n1: string } {
    const raw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").at(-1);
    if (!raw) {
      throw new Error("尚未发出 e2e_hello");
    }
    return JSON.parse(raw);
  }

  it("⑤ 该机器加密会话建立（E2E active）后再查一次：握手完成后的查询走密文（enc 内 approvals_query）", () => {
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
      fake.recv({ v: 1, type: "machines", online: [M] });
    });
    // E1：ready + machines 后已发一条明文查询（握手窗口）
    expect(approvalQueries(fake)).toEqual([M]);
    // 网关 ack → confirming（TOFU 弹层）：fail-closed 期间不发第二条（明文 / 密文都没有）
    const firstHello = lastHelloFor(fake);
    const firstAck = gw.ackFor(firstHello, M);
    act(() => {
      fake.recv(firstAck);
    });
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();
    expect(approvalQueries(fake)).toEqual([M]);
    expect(fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "enc")).toHaveLength(0);
    // 信任 → 以 pin 身份重新握手 → 新 ack → active
    fireEvent.click(screen.getByRole("button", { name: "信任并启用加密" }));
    const redone = lastHelloFor(fake);
    const secondAck = gw.ackFor(redone, M);
    act(() => {
      fake.recv(secondAck);
    });
    expect(screen.getByText("已加密")).toBeTruthy();
    // E2：active 后的第二次查询走密文——明文仍只有 E1 那一条。enc 帧里取内帧为
    // approvals_query 的那些（v1.43 起同一触发点还会再发一条 windows_query 的密文，
    // enc 帧不再恰一条）：恰一条、机器对、用**重新握手**的网关密钥可解回原帧
    // （握手窗口那把密钥解不开）
    expect(approvalQueries(fake)).toEqual([M]);
    const encs = fake.sent
      .filter((s) => (JSON.parse(s) as { type: string }).type === "enc")
      .map((s) => JSON.parse(s) as { machine: string; box: string });
    const APPROVAL_QUERY = JSON.stringify({ v: 1, type: "approvals_query", machine: M });
    const redoneKey = gw.gatewayKey(redone, secondAck.n2);
    const approvalEncs = encs.filter((_enc, i) => openBox(redoneKey, encs[i].box) === APPROVAL_QUERY);
    expect(approvalEncs).toHaveLength(1);
    expect(approvalEncs[0].machine).toBe(M);
    expect(openBox(gw.gatewayKey(firstHello, firstAck.n2), approvalEncs[0].box)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// v1.42 输入草稿按「机器 + 会话」保存：草稿键 = draftKey(当前机器, 当前会话)；
// 文字草稿经 DraftStore 防抖落盘 localStorage（键 imagent.drafts）；登出 /
// 认证失败 / 删除会话时清理；页面隐藏与 pagehide 时立即落盘。
// Composer 与 store 的协作细节（图片隔离、压缩中切换会话等）在
// Composer.test.tsx，存储本身在 lib/drafts.test.ts。
// ---------------------------------------------------------------------------

describe("v1.42 输入草稿按「机器 + 会话」保存", () => {
  const DRAFTS = "imagent.drafts";
  const M = "home-mac";
  /** 让 web:work 出现在侧栏。 */
  const workConv = (fake: FakeWebSocket): void => {
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "web:work", text: "后台会话消息", machine: M });
    });
  };
  const box = (): HTMLTextAreaElement => screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
  const type = (value: string): void => {
    fireEvent.change(box(), { target: { value } });
  };
  /** 落盘的草稿集合（null = 存储里没有草稿键）。 */
  const saved = (): Record<string, string> | null => {
    const raw = localStorage.getItem(DRAFTS);
    return raw === null ? null : (JSON.parse(raw) as Record<string, string>);
  };
  /** 冲刷防抖（500ms）及其后的定时器。 */
  const settle = (ms = 500): void => {
    act(() => {
      vi.advanceTimersByTime(ms);
    });
  };

  afterEach(() => {
    vi.useRealTimers();
    delete (document as { visibilityState?: string }).visibilityState;
  });

  it("① 草稿键跟随当前机器 + 会话：`<machine>||<convId>`；换会话 / 换机器即另一个键", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M, "work-mac"]);
    type("默认会话的草稿");
    settle();
    expect(saved()).toEqual({ "home-mac||web:default": "默认会话的草稿" });

    // 切到 work 会话：另一个键，前一个保留
    workConv(fake);
    fireEvent.click(screen.getByRole("button", { name: /^work(?!-mac)/ }));
    expect(box().value).toBe("");
    type("work 会话的草稿");
    settle();
    expect(saved()).toEqual({
      "home-mac||web:default": "默认会话的草稿",
      "home-mac||web:work": "work 会话的草稿",
    });

    // 切机器：另一台机器上的同名默认会话是第三个键（与 home-mac 的 default 互不相干）
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    expect(box().value).toBe("");
    type("另一台机器的草稿");
    settle();
    expect(saved()).toEqual({
      "home-mac||web:default": "默认会话的草稿",
      "home-mac||web:work": "work 会话的草稿",
      "work-mac||web:default": "另一台机器的草稿",
    });

    // 切回 home-mac：回到它自己的默认会话草稿
    fireEvent.click(screen.getByRole("button", { name: new RegExp(`^${M}`) }));
    expect(box().value).toBe("默认会话的草稿");
  });

  it("② 两个会话间切换，输入框内容各自独立（不再把草稿带到别的会话）", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    type("写给 default 的");
    workConv(fake);
    fireEvent.click(screen.getByRole("button", { name: /^work(?!-mac)/ }));
    expect(box().value).toBe(""); // 没有跟过来
    type("写给 work 的");
    fireEvent.click(screen.getByRole("button", { name: /^default/ }));
    expect(box().value).toBe("写给 default 的");
    fireEvent.click(screen.getByRole("button", { name: /^work(?!-mac)/ }));
    expect(box().value).toBe("写给 work 的");
  });

  it("③ 登出清空草稿：localStorage 的 imagent.drafts 被删，且不会有迟到的防抖写入把它写回", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    type("登出前的草稿");
    settle();
    // 正向锚点：草稿确实落了盘（否则下面的「为 null」没有判别力）
    expect(saved()).toEqual({ "home-mac||web:default": "登出前的草稿" });
    type("还没落盘的最后几句"); // 防抖重新计时，尚未写盘
    fireEvent.click(screen.getByRole("button", { name: "退出" }));
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(localStorage.getItem(DRAFTS)).toBeNull();
    // 冲刷：登出时排着的防抖（500ms 后写）已随 clearAll 取消，不会把草稿写回
    settle(5_000);
    expect(localStorage.getItem(DRAFTS)).toBeNull();
  });

  it("④ 预置 localStorage 草稿 → 首次进入对应会话时输入框恢复（刷新 / 杀进程后不丢）", () => {
    localStorage.setItem(DRAFTS, JSON.stringify({ "home-mac||web:work": "刷新前没发完的" }));
    const fake = mountApp();
    handshake(fake, [M]);
    expect(box().value).toBe(""); // 当前在 default，没有它的草稿
    workConv(fake);
    fireEvent.click(screen.getByRole("button", { name: /^work(?!-mac)/ }));
    expect(box().value).toBe("刷新前没发完的");
    // 恢复出来的草稿可以正常继续编辑并发送
    type("刷新前没发完的，补一句");
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(
      sentMessage(fake, { v: 1, type: "message", convId: "web:work", text: "刷新前没发完的，补一句", machine: M }),
    ).toBe(true);
    expect(box().value).toBe("");
  });

  it("⑤ 认证失败回登录页同样清空草稿（与消息缓存同为本机痕迹）", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    type("认证失败前的草稿");
    settle();
    expect(saved()).not.toBeNull();
    act(() => {
      fake.emit("close", { code: 1008, reason: "auth failed" });
    });
    expect(screen.getByLabelText("访问令牌（USER_TOKEN）")).toBeTruthy();
    expect(localStorage.getItem(DRAFTS)).toBeNull();
    settle(5_000);
    expect(localStorage.getItem(DRAFTS)).toBeNull();
  });

  it("⑤b 被限流（瞬态关闭，不登出）时草稿不受影响：留在主界面，草稿原样", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    type("限流时正在写的");
    act(() => {
      fake.emit("close", { code: 1008, reason: "rate limited" });
    });
    settle(1_000); // 退避到点，重连
    const next = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      next.emit("open");
      next.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    expect(screen.queryByLabelText("访问令牌（USER_TOKEN）")).toBeNull(); // 没回登录页
    expect(box().value).toBe("限流时正在写的");
  });

  it("⑥ 页面隐藏 / 卸载前落盘：防抖还没到点时转入后台或触发 pagehide，草稿立即写盘", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    type("刚打的字");
    expect(localStorage.getItem(DRAFTS)).toBeNull(); // 防抖 500ms 未到
    // 转入不可见（iOS 把后台应用挂起前的最后一个时机）
    Object.defineProperty(document, "visibilityState", { value: "hidden", configurable: true });
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(saved()).toEqual({ "home-mac||web:default": "刚打的字" });
    // 回到前台继续输入 → pagehide（页面卸载）同步落盘
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    type("又打了几个字");
    expect(saved()).toEqual({ "home-mac||web:default": "刚打的字" }); // 仍在防抖里
    act(() => {
      window.dispatchEvent(new Event("pagehide"));
    });
    expect(saved()).toEqual({ "home-mac||web:default": "又打了几个字" });
  });

  it("⑥b 页面可见时的 visibilitychange 不落盘（只有不可见 / pagehide 才立即写）", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    type("可见时的输入");
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(localStorage.getItem(DRAFTS)).toBeNull(); // 仍等防抖
    settle();
    expect(saved()).toEqual({ "home-mac||web:default": "可见时的输入" });
  });

  it("⑦ 删除会话 → 该会话的草稿一并清掉（同名会话重建时旧草稿不再冒出）；别的会话草稿保留", () => {
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    type("default 的草稿留着");
    workConv(fake);
    const workItem = (): HTMLElement => screen.getByRole("button", { name: /^work(?!-mac)/ });
    fireEvent.click(workItem());
    type("要随会话一起删掉的草稿");
    settle();
    expect(saved()).toEqual({
      "home-mac||web:default": "default 的草稿留着",
      "home-mac||web:work": "要随会话一起删掉的草稿",
    });
    // 侧栏左滑露出删除 → 点删除 → 确认（pointer 事件须手搓：jsdom 无 PointerEvent 构造器）
    const pointer = (kind: "pointerdown" | "pointermove" | "pointerup", x: number) => {
      const ev = new Event(kind, { bubbles: true, cancelable: true, composed: true });
      Object.assign(ev, { pointerType: "touch", button: 0, clientX: x, clientY: 20 });
      return ev;
    };
    const item = workItem();
    fireEvent(item, pointer("pointerdown", 200));
    fireEvent(item, pointer("pointermove", 170));
    fireEvent(item, pointer("pointermove", 140));
    fireEvent(item, pointer("pointerup", 140));
    fireEvent.click(screen.getByRole("button", { name: "删除" }));
    fireEvent.click(
      within(screen.getByRole("dialog", { name: "确认删除会话" })).getByRole("button", { name: "删除" }),
    );
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "conv_delete", convId: "web:work", machine: M }));
    settle();
    expect(saved()).toEqual({ "home-mac||web:default": "default 的草稿留着" });
    // 当前会话已切回 default；输入框不是被删会话的草稿
    expect(box().value).toBe("default 的草稿留着");
  });
});

// ---------------------------------------------------------------------------
// v1.42 重新登录后的连接就绪：App 的连接态（status）只由 WsManager 的 onStatus
// 驱动，而登出 / 认证失败不会把它从 ready 拉回来；新 WsManager 的内部初值就是
// connecting，connect() 里那次同值 setStatus 不通知。结果是重新登录后 status
// 一直停在 ready，新连接 hello_ack 带来的 ready 被当作「没有变化」——所有依赖
// status 的 effect（sync + workspace_list、history_query、待审批查询）在新连接
// 上一次都不触发。修复：WS effect 每建立一个新的连接实例，先显式拉回 connecting。
// ---------------------------------------------------------------------------

describe("v1.42 重新登录后连接就绪 effect 照常触发", () => {
  const M = "home-mac";
  const DAY = 86_400_000;
  const frameTypes = (fake: FakeWebSocket): string[] =>
    fake.sent.map((s) => (JSON.parse(s) as { type: string }).type);
  const TOKEN_INPUT = "访问令牌（USER_TOKEN）";
  /** 登录页输入令牌并连接，返回新 socket（已 open）。 */
  function loginAgain(token: string): FakeWebSocket {
    fireEvent.change(screen.getByLabelText(TOKEN_INPUT), { target: { value: token } });
    fireEvent.click(screen.getByRole("button", { name: "连接" }));
    const next = FakeWebSocket.latest as FakeWebSocket;
    act(() => next.emit("open"));
    return next;
  }
  /** 新连接就绪后「与首连一致」的出站：ws 内置 sync + App 的 sync、workspace_list、
   *  history_query 各一条 + 每台在线机器一条 approvals_query。 */
  const expectReadyBurst = (fake: FakeWebSocket): void => {
    const types = frameTypes(fake);
    expect(types.filter((t) => t === "sync"), "sync").toHaveLength(2);
    for (const t of ["workspace_list", "history_query", "approvals_query"]) {
      expect(types.filter((x) => x === t), t).toHaveLength(1);
    }
  };

  it("对照·首连（页面加载）ready 后的出站：sync×2 + workspace_list + history_query + approvals_query", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    expectReadyBurst(fake);
  });

  it("退出后重新登录（不经刷新）：新连接 ready 时同样补发，不再因 status 停在 ready 而全部缺席", () => {
    const first = mountApp();
    handshake(first, [M]);
    expectReadyBurst(first);
    // 退出：state（machinesKnown / machines）保留，连接态此前也一直保留在 ready
    fireEvent.click(screen.getByRole("button", { name: "退出" }));
    expect(screen.getByLabelText(TOKEN_INPUT)).toBeTruthy();
    const second = loginAgain("tok-2");
    expect(second).not.toBe(first);
    expect(frameTypes(second)).toEqual(["hello"]); // 尚未 ready：只有 hello
    handshake(second, [M]);
    expect(screen.getByText("已连接")).toBeTruthy();
    expectReadyBurst(second);
    expect(second.sent).toContain(JSON.stringify({ v: 1, type: "approvals_query", machine: M }));
  });

  it("认证失败（凭证失效回登录页）后重新登录：新连接 ready 时同样补发", () => {
    const first = mountApp();
    handshake(first, [M]);
    act(() => {
      first.emit("close", { code: 1008, reason: "auth failed" });
    });
    expect(screen.getByLabelText(TOKEN_INPUT)).toBeTruthy();
    const second = loginAgain("tok-2");
    handshake(second, [M]);
    expect(screen.getByText("已连接")).toBeTruthy();
    expectReadyBurst(second);
  });

  it("曾登录 → 退出 → 新设备配对获批：交接瞬间不对未就绪的新连接发出查询（也不落在配对连接上）；新连接 ready 后发出", () => {
    const first = mountApp();
    handshake(first, [M]);
    expectReadyBurst(first);
    fireEvent.click(screen.getByRole("button", { name: "退出" }));
    // 新设备配对
    fireEvent.click(screen.getByRole("button", { name: /我是新设备/ }));
    fireEvent.click(screen.getByRole("button", { name: "开始配对" }));
    const pair = FakeWebSocket.latest as FakeWebSocket;
    // 交接瞬间的发送发生在 WsManager 上：记录每次 sendApprovalsQuery 的返回值
    const sendSpy = vi.spyOn(WsManager.prototype, "sendApprovalsQuery");
    try {
      act(() => pair.emit("open"));
      act(() => pair.recv({ v: 1, type: "pairing_pending", code: "123456" }));
      // 已登录侧批准：配对连接上直接换发 hello_ack.session → App 以会话形态重连
      act(() => {
        pair.recv({
          v: 1,
          type: "hello_ack",
          role: "user",
          session: { id: "sid-p", token: "st-p", expiresAt: Date.now() + 20 * DAY },
        });
      });
      const next = FakeWebSocket.latest as FakeWebSocket;
      expect(next).not.toBe(pair);
      // 交接瞬间：连接态的这次变化让待审批 effect 触发了一次，但新连接尚未就绪
      // ——查询被拒发（返回 false，不发帧），配对连接上也没有任何业务帧
      expect(sendSpy.mock.results.map((r) => r.value)).toEqual([false]);
      // 配对连接上只有 ws 层内置的 hello 与 hello_ack 后的 sync（既有行为），
      // 没有任何 App 层 effect 发的帧（待审批查询 / workspace_list / history_query）
      expect(frameTypes(pair)).toEqual(["hello", "sync"]);
      expect(pair.sent.filter((s) => s.includes("approvals_query"))).toHaveLength(0);
      // 新连接就绪 → 查询真正发出
      act(() => next.emit("open"));
      act(() => {
        next.recv({
          v: 1,
          type: "hello_ack",
          role: "user",
          session: { id: "sid-p", token: "st-p", expiresAt: Date.now() + 20 * DAY },
        });
      });
      expect(sendSpy.mock.results.map((r) => r.value)).toEqual([false, true]);
      act(() => {
        next.recv({ v: 1, type: "machine", online: false });
        next.recv({ v: 1, type: "machines", online: [M] });
      });
      expectReadyBurst(next);
    } finally {
      sendSpy.mockRestore();
    }
  });

  it("首次挂载不因显式拉回连接态而多出状态往返：首连就绪前后出站帧序不变", () => {
    const fake = mountApp();
    // 挂载后尚未连接：无业务帧
    expect(frameTypes(fake)).toEqual([]);
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    expect(frameTypes(fake)).toEqual(["hello", "sync"]);
  });
});

// ---------------------------------------------------------------------------
// v1.43 本地窗口看板（App 层）：ready 后对全部在线机器各查一次（E1）+ 该机器加密
// 会话建立后再查一次（E2）；windows 帧进 reducer 驱动侧栏区块与状态条提示；
// 星标 = 发 window_watch 帧，帧被接受后才乐观翻转。关注归网关所有（由网关持久化），
// 前端不保存名单、不对账。
// ---------------------------------------------------------------------------

describe("v1.43 本地窗口看板", () => {
  const M = "home-mac";
  /** 契约 §2 示例形态的窗口条目（可选时间戳省略）。 */
  const win = (extra: Partial<WindowInfo> = {}): WindowInfo => ({
    name: "jormungandr-f8",
    status: "idle",
    cwd: "/Users/demo/work/api",
    watched: false,
    ...extra,
  });
  const IDLE = win();
  const WAITING = win({ name: "imagent-a7", status: "waiting", waitingFor: "dialog open" });
  const BUSY = win({ name: "runner", status: "busy" });
  const WINDOWS = (items: WindowInfo[], machine = M, hookInstalled = true) => ({
    v: 1 as const,
    type: "windows" as const,
    machine,
    hookInstalled,
    items,
  });
  const parsed = (fake: FakeWebSocket): Record<string, unknown>[] =>
    fake.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
  /** 出站 windows_query（明文形态）的目标机器序列。 */
  const windowsQueries = (fake: FakeWebSocket): string[] =>
    parsed(fake)
      .filter((f) => f.type === "windows_query")
      .map((f) => (f.machine as string | undefined) ?? "");
  /** 出站 window_watch（明文形态）帧。 */
  const watchFrames = (fake: FakeWebSocket): Record<string, unknown>[] =>
    parsed(fake).filter((f) => f.type === "window_watch");
  const recvWindows = (fake: FakeWebSocket, items: WindowInfo[], machine = M, hookInstalled = true): void => {
    act(() => fake.recv(WINDOWS(items, machine, hookInstalled)));
  };
  /** 侧栏「本地窗口」区块（DOM 序：桌面侧栏在前、移动抽屉在后）。 */
  const sections = (): HTMLElement[] => screen.queryAllByRole("region", { name: "本地窗口" });
  const section = (i = 0): HTMLElement => sections()[i];
  const row = (label: string, scope: HTMLElement = section()): HTMLElement =>
    within(scope).getByRole("button", { name: label }) as HTMLElement;
  const waitingChip = (): HTMLElement | null =>
    screen.queryByRole("button", { name: /个本地窗口在等你确认/ });
  /** 状态条里当前会话名 pill 的文本（convTitle：peer 会话显示窗口名）。 */
  const convLabel = (): string | null =>
    screen.getByRole("banner").querySelector("span.font-mono")?.textContent ?? null;
  /** 移动端抽屉壳层的 aria-hidden 值：jsdom 无布局（md:hidden 不生效），壳层是否
   *  「打开」以它为准——关闭态 "true"，打开后 "false"。 */
  const drawerHidden = (): string | null =>
    document.querySelector(".z-50.md\\:hidden")?.getAttribute("aria-hidden") ?? null;

  afterEach(() => {
    vi.useRealTimers();
    // 恢复 URL，避免污染同文件后续/其它用例
    window.history.replaceState(null, "", "/");
  });

  it("① ready + 收到 machines 帧 → 对**全部**在线机器各发一条 windows_query（帧形状即契约原文）", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    // hello_ack 后尚未收到 machines 帧：不发（单机模式旧网关不认识该帧）
    expect(windowsQueries(fake)).toEqual([]);
    act(() => {
      fake.recv({ v: 1, type: "machine", online: false });
      fake.recv({ v: 1, type: "machines", online: [M, "work-mac", "office-mac"] });
    });
    // 当前机器是 home-mac（自动选中首台）；三台各一条
    expect(windowsQueries(fake)).toEqual([M, "work-mac", "office-mac"]);
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "windows_query", machine: "work-mac" }));
  });

  it("①b 切换当前机器 / machines 帧重排不重发；单机模式（旧 relay，无 machines 帧）不发", () => {
    const fake = mountApp();
    handshake(fake, [M, "work-mac"]);
    expect(windowsQueries(fake)).toEqual([M, "work-mac"]);
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    expect(windowsQueries(fake)).toEqual([M, "work-mac"]); // 切机不触发
    // machines 帧重排（任一网关上下线都会再推）：同样不触发
    act(() => {
      fake.recv({ v: 1, type: "machines", online: ["work-mac", M] });
    });
    expect(windowsQueries(fake)).toEqual([M, "work-mac"]);
    cleanup();
    const single = mountApp();
    act(() => {
      single.emit("open");
      single.recv({ v: 1, type: "hello_ack", role: "user" });
      single.recv({ v: 1, type: "machine", online: true });
    });
    expect(screen.getByText("已连接")).toBeTruthy();
    expect(windowsQueries(single)).toEqual([]);
  });

  it("② 收到 windows 帧 → 侧栏出现「本地窗口」区块与窗口行（状态文字 / 状态点 / 相对时间 / 汇总 / 钩子状态行）", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    // 还没有快照：区块不出现（旧网关 / 未启用 peer 通道的形态）
    expect(sections()).toHaveLength(0);
    recvWindows(fake, [win({ statusUpdatedAt: Date.now() - 3 * 60_000 }), WAITING, BUSY]);
    const sec = section();
    expect(sec).toBeTruthy();
    // 窗口名与第二行状态文字（相对时间取自 statusUpdatedAt）
    const idleRow = row("窗口 jormungandr-f8：空闲");
    expect(within(idleRow).getByText("jormungandr-f8")).toBeTruthy();
    expect(within(idleRow).getByText("空闲 · 3 分钟前")).toBeTruthy();
    expect(idleRow.querySelector(".rounded-full")?.className).toContain("bg-slate-500");
    const waitRow = row("窗口 imagent-a7：等待确认，等你确认");
    expect(within(waitRow).getByText("等待确认（等你确认）")).toBeTruthy();
    expect(waitRow.querySelector(".rounded-full")?.className).toContain("bg-amber-400");
    const busyRow = row("窗口 runner：运行中");
    expect(within(busyRow).getByText("运行中")).toBeTruthy();
    expect(busyRow.querySelector(".rounded-full")?.className).toContain("animate-pulse");
    // 呼吸动画只给运行中（待确认 / 空闲的点静止）
    expect(sec!.querySelectorAll(".animate-pulse")).toHaveLength(1);
    // 区块头汇总：待确认优先于运行中（同时有 busy 也不显示「N 运行中」）
    expect(within(sec!).getByText("1 待确认")).toBeTruthy();
    expect(within(sec!).queryByText(/\d+ 运行中/)).toBeNull();
    expect(within(sec!).getByText("结论自动上报：已开启")).toBeTruthy();
    // 还没关注任何窗口：三个星标都是空心
    expect(within(sec!).getAllByRole("button", { name: /^关注窗口 / })).toHaveLength(3);
  });

  it("③ 点星标 → 发出 window_watch{watch:true}、星标即变实心（帧被接受后的乐观更新）；再点 → {watch:false}；关注不落 localStorage", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    recvWindows(fake, [IDLE]);
    // 关注归网关所有（由网关持久化）：前端不保存名单——点星标不得产生任何本地存储写入
    const keysBefore = Object.keys(localStorage);
    const star = (): HTMLElement =>
      within(section()).getByRole("button", { name: /^(?:取消)?关注窗口 jormungandr-f8$/ }) as HTMLElement;
    expect(star().getAttribute("aria-label")).toBe("关注窗口 jormungandr-f8");
    expect(star().getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(star());
    // 乐观更新：帧一发出（不必等网关回包）星标即变实心
    expect(star().getAttribute("aria-label")).toBe("取消关注窗口 jormungandr-f8");
    expect(star().getAttribute("aria-pressed")).toBe("true");
    expect(watchFrames(fake)).toEqual([
      { v: 1, type: "window_watch", machine: M, name: "jormungandr-f8", watch: true },
    ]);
    // 再点：取消关注
    fireEvent.click(star());
    expect(star().getAttribute("aria-pressed")).toBe("false");
    expect(watchFrames(fake)).toEqual([
      { v: 1, type: "window_watch", machine: M, name: "jormungandr-f8", watch: true },
      { v: 1, type: "window_watch", machine: M, name: "jormungandr-f8", watch: false },
    ]);
    expect(Object.keys(localStorage)).toEqual(keysBefore);
  });

  it("③b 关注另一个窗口不动第一个；网关回报已关注的快照到达后保持实心，且不重复发帧", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    recvWindows(fake, [IDLE, WAITING]);
    fireEvent.click(within(section()).getByRole("button", { name: "关注窗口 jormungandr-f8" }));
    expect(
      within(section()).getByRole("button", { name: "关注窗口 imagent-a7" }).getAttribute("aria-pressed"),
    ).toBe("false");
    // 网关处理完 window_watch 后重发快照（watched:true；内容与乐观更新后的一致）
    recvWindows(fake, [{ ...IDLE, watched: true }, WAITING]);
    expect(
      within(section()).getByRole("button", { name: "取消关注窗口 jormungandr-f8" }).getAttribute("aria-pressed"),
    ).toBe("true");
    expect(watchFrames(fake)).toHaveLength(1);
  });

  it("③c 网关报已关注的窗口（可以是别的设备关注的）：本机点实心星即取消关注——发 window_watch{watch:false}、星标变空心；网关回报后保持", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    // 关注是跨设备的全局状态：别的设备关注过，本机快照里它已是实心
    recvWindows(fake, [{ ...WAITING, watched: true }]);
    expect(watchFrames(fake)).toEqual([]);
    fireEvent.click(within(section()).getByRole("button", { name: "取消关注窗口 imagent-a7" }));
    expect(watchFrames(fake)).toEqual([
      { v: 1, type: "window_watch", machine: M, name: "imagent-a7", watch: false },
    ]);
    expect(
      within(section()).getByRole("button", { name: "关注窗口 imagent-a7" }).getAttribute("aria-pressed"),
    ).toBe("false");
    // 网关处理完重发快照（watched:false，与乐观更新后一致）：保持空心，不重复发帧
    recvWindows(fake, [WAITING]);
    expect(within(section()).getByRole("button", { name: "关注窗口 imagent-a7" })).toBeTruthy();
    expect(watchFrames(fake)).toHaveLength(1);
  });

  it("④ 发送被拒（未 ready）时点星标：不出帧、状态不变、不留任何痕迹；连接恢复后再点才生效", () => {
    // 重连退避 1s 是定时器：挂载前切假时钟
    vi.useFakeTimers();
    const fake = mountApp();
    handshake(fake, [M]);
    recvWindows(fake, [IDLE]);
    act(() => {
      fake.emit("close", { code: 1006 });
    });
    expect(screen.getByText("重连中")).toBeTruthy();
    const before = section()!.innerHTML;
    const keysBefore = Object.keys(localStorage);
    fireEvent.click(within(section()).getByRole("button", { name: "关注窗口 jormungandr-f8" }));
    // 帧发不出去（未 ready）→ 不做乐观更新：星标仍是空心，界面不抢先说「已关注」
    expect(section()!.innerHTML).toBe(before);
    expect(watchFrames(fake)).toEqual([]);
    expect(Object.keys(localStorage)).toEqual(keysBefore);
    // 连接恢复 → 再查一次 → 网关快照照旧报未关注（它从没收到过那帧）→ 星标仍是空心
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    const next = FakeWebSocket.latest as FakeWebSocket;
    expect(next).not.toBe(fake);
    handshake(next, [M]);
    expect(windowsQueries(next)).toEqual([M]);
    recvWindows(next, [IDLE]);
    expect(
      within(section()).getByRole("button", { name: "关注窗口 jormungandr-f8" }).getAttribute("aria-pressed"),
    ).toBe("false");
    expect(watchFrames(next)).toEqual([]);
    // 恢复后再点：帧发出，星标随即变实心
    fireEvent.click(within(section()).getByRole("button", { name: "关注窗口 jormungandr-f8" }));
    expect(watchFrames(next)).toEqual([
      { v: 1, type: "window_watch", machine: M, name: "jormungandr-f8", watch: true },
    ]);
    expect(
      within(section()).getByRole("button", { name: "取消关注窗口 jormungandr-f8" }).getAttribute("aria-pressed"),
    ).toBe("true");
  });

  it("④b 发送被拒（握手待确认指纹，E2E fail-closed）时点星标：不出帧、状态不变；确认加密后再点才生效（走密文）", () => {
    const gw = makeGateway();
    const fake = mountApp();
    handshake(fake, [M]);
    // 网关 ack → confirming（TOFU 弹层）：出站业务帧 fail-closed 拒发；入站明文仍放行，看板照常显示
    act(() => {
      fake.recv(gw.ackFor(lastHelloFor(fake), M));
    });
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();
    recvWindows(fake, [IDLE]);
    const before = section()!.innerHTML;
    fireEvent.click(within(section()).getByRole("button", { name: "关注窗口 jormungandr-f8" }));
    expect(section()!.innerHTML).toBe(before);
    expect(watchFrames(fake)).toEqual([]);
    // 明文 / 密文都没有（此时连 enc 帧都还没发过）
    expect(parsed(fake).filter((f) => f.type === "enc")).toHaveLength(0);
    // 信任 → 以 pin 身份重新握手 → active
    fireEvent.click(screen.getByRole("button", { name: "信任并启用加密" }));
    const redone = lastHelloFor(fake);
    const secondAck = gw.ackFor(redone, M);
    act(() => {
      fake.recv(secondAck);
    });
    fireEvent.click(within(section()).getByRole("button", { name: "关注窗口 jormungandr-f8" }));
    // active 后出站走密文：用**重新握手**的网关密钥可解回 window_watch
    const key = gw.gatewayKey(redone, secondAck.n2);
    const inner = parsed(fake)
      .filter((f) => f.type === "enc")
      .map((enc) => openBox(key, enc.box as string))
      .filter((plain): plain is string => plain !== null)
      .map((plain) => JSON.parse(plain) as Record<string, unknown>);
    expect(inner.filter((f) => f.type === "window_watch")).toEqual([
      { v: 1, type: "window_watch", machine: M, name: "jormungandr-f8", watch: true },
    ]);
    expect(within(section()).getByRole("button", { name: "取消关注窗口 jormungandr-f8" })).toBeTruthy();
  });

  it("⑤ 点窗口行 → 当前会话变为 peer:<窗口名>（状态条标题 / 会话列表当前项），之后发消息落该会话；在移动端抽屉里点则同时关抽屉", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    recvWindows(fake, [IDLE]);
    expect(drawerHidden()).toBe("true");
    // 打开移动端抽屉（壳层 aria-hidden 变 false，抽屉内出现第二份侧栏）
    fireEvent.click(screen.getByRole("button", { name: "打开会话列表" }));
    expect(drawerHidden()).toBe("false");
    expect(sections()).toHaveLength(2);
    fireEvent.click(row("窗口 jormungandr-f8：空闲", section(1)));
    // 抽屉关闭（内容延迟 150ms 卸载，但对可访问性树已隐藏）；后续断言落在桌面侧栏那一份
    expect(drawerHidden()).toBe("true");
    expect(sections()).toHaveLength(1);
    expect(convLabel()).toBe("jormungandr-f8");
    const nav = screen.getAllByRole("navigation", { name: "会话列表" })[0];
    expect(nav.querySelector('[aria-current="page"]')?.textContent).toContain("jormungandr-f8");
    // 在该会话发消息 → message 帧 convId 是 peer:<窗口名>
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "帮我跑下构建" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(
      sentMessage(fake, { v: 1, type: "message", convId: "peer:jormungandr-f8", text: "帮我跑下构建", machine: M }),
    ).toBe(true);
  });

  it("⑥ 旧网关（收不到任何 windows 帧）→ 侧栏没有「本地窗口」区块、状态条没有待确认提示，其余功能不变", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    expect(sections()).toHaveLength(0);
    expect(screen.queryByText(/结论自动上报/)).toBeNull();
    expect(waitingChip()).toBeNull();
    // 会话列表 / 机器切换器 / 空态示例 prompt 照常
    expect(screen.getByRole("navigation", { name: "会话列表" })).toBeTruthy();
    expect(screen.getByRole("listbox", { name: "机器列表" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "看看这个项目结构" }));
    expect(
      sentMessage(fake, { v: 1, type: "message", convId: "web:default", text: "看看这个项目结构", machine: M }),
    ).toBe(true);
    expect(sections()).toHaveLength(0);
  });

  it("⑥b 畸形 windows 帧被拒收：不出现区块（不拿半份可疑数据渲染整块侧栏界面）", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    for (const bad of [
      // 条目缺 name
      { v: 1, type: "windows", machine: M, hookInstalled: true, items: [{ status: "idle", cwd: "/x", watched: false }] },
      // status 越界
      { v: 1, type: "windows", machine: M, hookInstalled: true, items: [{ name: "w", status: "running", cwd: "/x", watched: false }] },
      // watched 非布尔
      { v: 1, type: "windows", machine: M, hookInstalled: true, items: [{ name: "w", status: "idle", cwd: "/x", watched: "false" }] },
      // 缺 hookInstalled
      { v: 1, type: "windows", machine: M, items: [] },
      // machine 空串
      { v: 1, type: "windows", machine: "", hookInstalled: true, items: [] },
      // items 非数组
      { v: 1, type: "windows", machine: M, hookInstalled: true, items: {} },
    ]) {
      act(() => fake.recv(bad));
    }
    expect(sections()).toHaveLength(0);
    expect(waitingChip()).toBeNull();
  });

  it("⑦ 状态条：快照里有 1 个 waiting → 出现「⏸ 1 个窗口待确认」，点击打开抽屉；没有 waiting → 不出现；waiting 清零后消失", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    expect(waitingChip()).toBeNull();
    // 只有运行中：「运行中」的数量在侧栏区块头，不占状态条
    recvWindows(fake, [IDLE, BUSY]);
    expect(waitingChip()).toBeNull();
    recvWindows(fake, [IDLE, WAITING]);
    const chip = waitingChip();
    expect(chip!.textContent).toBe("⏸ 1 个窗口待确认");
    expect(chip!.getAttribute("aria-label")).toBe("有 1 个本地窗口在等你确认，打开会话列表");
    expect(drawerHidden()).toBe("true");
    fireEvent.click(chip!);
    expect(drawerHidden()).toBe("false");
    // 窗口不再等待（对话框处理掉了）→ 提示消失
    recvWindows(fake, [IDLE, { ...WAITING, status: "idle" }]);
    expect(waitingChip()).toBeNull();
  });

  it("⑦b 状态条只统计**当前机器**的 waiting：另一台机器的快照里有 waiting 也不提示，切过去才提示", () => {
    const fake = mountApp();
    handshake(fake, [M, "work-mac"]);
    recvWindows(fake, [WAITING], "work-mac");
    expect(waitingChip()).toBeNull();
    // 当前机器 home-mac 的侧栏也不显示 work-mac 的窗口
    expect(sections()).toHaveLength(0);
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    expect(waitingChip()!.textContent).toBe("⏸ 1 个窗口待确认");
    expect(within(section()).getByRole("button", { name: "窗口 imagent-a7：等待确认，等你确认" })).toBeTruthy();
  });

  it("⑧ 窗口会话空态：进入 peer: 会话且无条目 → 显示说明文字、没有示例 prompt；默认会话仍有示例 prompt；发出第一条消息后说明消失", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    recvWindows(fake, [IDLE]);
    // 对照：默认会话是通用空态（示例 prompt 在，没有说明）
    expect(screen.getByRole("button", { name: "看看这个项目结构" })).toBeTruthy();
    expect(screen.queryByText(/这是与本地 Claude Code 窗口/)).toBeNull();
    fireEvent.click(row("窗口 jormungandr-f8：空闲"));
    const main = within(screen.getByRole("main"));
    expect(
      main.getByText(
        "这是与本地 Claude Code 窗口「jormungandr-f8」的对话。直接输入文字即可派活，窗口空闲时会自动开始处理；窗口的回信会出现在这里。",
      ),
    ).toBeTruthy();
    for (const sample of ["看看这个项目结构", "帮我写个脚本", "解释一下最近的改动"]) {
      expect(screen.queryByRole("button", { name: sample }), sample).toBeNull();
    }
    // 发出第一条消息后会话不再为空：说明消失（示例 prompt 也不回来——会话已有内容）
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "帮我跑下构建" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(screen.queryByText(/这是与本地 Claude Code 窗口/)).toBeNull();
    expect(main.getByText("帮我跑下构建")).toBeTruthy();
  });

  it("⑧b 深链进入 peer: 会话（?conv=peer:<窗口名>）同样显示说明（不依赖快照是否到达）", () => {
    window.history.replaceState(null, "", "/?conv=peer:win-a");
    handshake(mountApp(), [M]);
    expect(screen.queryByText(/这是与本地 Claude Code 窗口「win-a」的对话/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "看看这个项目结构" })).toBeNull();
  });

  it("⑧c 会话已有消息（窗口回信落进 peer 会话）→ 不显示说明", () => {
    const fake = mountApp();
    handshake(fake, [M]);
    recvWindows(fake, [IDLE]);
    act(() => {
      fake.recv({ v: 1, type: "text", convId: "peer:jormungandr-f8", text: "构建通过，无告警。", machine: M });
    });
    fireEvent.click(row("窗口 jormungandr-f8：空闲"));
    expect(screen.queryByText(/这是与本地 Claude Code 窗口/)).toBeNull();
    expect(within(screen.getByRole("main")).getByText("构建通过，无告警。")).toBeTruthy();
  });

  /** 测试网关（与「v1.42 待审批快照」describe 内同名助手同构——该助手为
   *  describe 局部，此处复制一份；长期 X25519 + ed25519 身份密钥）。 */
  function makeGateway() {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    const ackFor = (hello: { ek: string; n1: string }, machine: string) => {
      const n2 = bytesToHex(randomBytes(32));
      const msg = new Uint8Array(128);
      msg.set(hexToBytes(hello.ek), 0);
      msg.set(hexToBytes(gk), 32);
      msg.set(hexToBytes(hello.n1), 64);
      msg.set(hexToBytes(n2), 96);
      return {
        v: 1 as const,
        type: "e2e_ack" as const,
        machine,
        gk,
        n2,
        sig: bytesToHex(ed25519.sign(msg, idPriv)),
        ik,
      };
    };
    const gatewayKey = (hello: { ek: string; n1: string }, n2: string) => {
      const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
      const ikm = new Uint8Array(96);
      ikm.set(shared, 0);
      ikm.set(hexToBytes(hello.n1), 32);
      ikm.set(hexToBytes(n2), 64);
      return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
    };
    return { gk, ik, ackFor, gatewayKey };
  }

  /** 从 fake.sent 里取最近一条 e2e_hello（无则抛）。 */
  function lastHelloFor(fake: FakeWebSocket): { ek: string; n1: string } {
    const raw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").at(-1);
    if (!raw) {
      throw new Error("尚未发出 e2e_hello");
    }
    return JSON.parse(raw);
  }

  it("⑨ 该机器加密会话建立（E2E active）后再查一次：握手完成后的查询走密文（enc 内 windows_query）；此后快照必须是密文才被采纳", () => {
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
      fake.recv({ v: 1, type: "machines", online: [M] });
    });
    // E1：ready + machines 后已发一条明文查询（握手窗口）
    expect(windowsQueries(fake)).toEqual([M]);
    // 网关 ack → confirming（TOFU 弹层）：fail-closed 期间不发第二条（明文 / 密文都没有）
    const firstHello = lastHelloFor(fake);
    const firstAck = gw.ackFor(firstHello, M);
    act(() => {
      fake.recv(firstAck);
    });
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();
    expect(windowsQueries(fake)).toEqual([M]);
    expect(parsed(fake).filter((f) => f.type === "enc")).toHaveLength(0);
    // 信任 → 以 pin 身份重新握手 → 新 ack → active
    fireEvent.click(screen.getByRole("button", { name: "信任并启用加密" }));
    const redone = lastHelloFor(fake);
    const secondAck = gw.ackFor(redone, M);
    act(() => {
      fake.recv(secondAck);
    });
    expect(screen.getByText("已加密")).toBeTruthy();
    // E2：active 后的第二次查询走密文——明文仍只有 E1 那一条；用**重新握手**的网关
    // 密钥可解回 windows_query
    const key = gw.gatewayKey(redone, secondAck.n2);
    const inner = parsed(fake)
      .filter((f) => f.type === "enc")
      .map((enc) => openBox(key, enc.box as string))
      .filter((plain): plain is string => plain !== null)
      .map((plain) => JSON.parse(plain) as Record<string, unknown>);
    expect(inner.filter((f) => f.type === "windows_query")).toEqual([
      { v: 1, type: "windows_query", machine: M },
    ]);
    expect(windowsQueries(fake)).toEqual([M]);
    // active 之后：明文快照是注入面，被丢弃（看板不出现）
    recvWindows(fake, [IDLE]);
    expect(sections()).toHaveLength(0);
    // 密文快照（网关经 send_business 加密下发）照常渲染
    act(() => fake.recv(sealFrame(M, key, WINDOWS([IDLE]))));
    expect(within(section()).getByRole("button", { name: "窗口 jormungandr-f8：空闲" })).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// v1.44 送达回执 + 网关版本（App 层）：ready 后对全部在线机器各查一次网关能力
// （E1）+ 该机器加密会话建立后再查一次（E2，先作废上一代会话问到的能力）；
// gateway_info 决定该机器的本机回显带不带送达状态；ack 帧改写回显状态；超时
// 判定每 2 秒巡检；重发。回执路径按真实形态走密文（测试网关的会话密钥封 / 解）。
// ---------------------------------------------------------------------------

describe("v1.44 送达回执 + 网关版本", () => {
  const M = "home-mac";
  const DAY = 86_400_000;
  type Frame = Record<string, unknown>;
  /** 契约 §2 示例形态的 gateway_info 帧（默认：支持回执）。 */
  const INFO = (machine = M, version = "1.44.0", caps: string[] = ["acks"]): Frame => ({
    v: 1,
    type: "gateway_info",
    machine,
    version,
    caps,
  });
  const ACK_RECV = (clientId: string, convId = "web:default"): Frame => ({
    v: 1,
    type: "ack",
    machine: M,
    clientId,
    convId,
    status: "received",
  });
  const ACK_REJECT = (clientId: string, convId = "web:default"): Frame => ({
    v: 1,
    type: "ack",
    machine: M,
    clientId,
    convId,
    status: "rejected",
    reason: "not_allowed",
    message: "发送者未获授权，消息未被处理",
  });
  /** 出站帧解析：明文帧原样；enc 帧用网关侧密钥解出内帧（解不开 / 没给密钥 → null）。 */
  const outbound = (fake: FakeWebSocket, key?: Uint8Array): (Frame | null)[] =>
    fake.sent.map((raw) => {
      const f = JSON.parse(raw) as Frame;
      if (f.type !== "enc") {
        return f;
      }
      const plain = key ? openBox(key, f.box as string) : null;
      return plain === null ? null : (JSON.parse(plain) as Frame);
    });
  /** 出站 gateway_info_query 的目标机器序列（明文 + 密文内帧）。 */
  const infoQueries = (fake: FakeWebSocket, key?: Uint8Array): string[] =>
    outbound(fake, key)
      .filter((f): f is Frame => f !== null && f.type === "gateway_info_query")
      .map((f) => (f.machine as string | undefined) ?? "");
  type SentMessage = { frame: Frame; clientId: string };
  /** 出站 message 帧（明文 + 密文内帧）：去掉 clientId 的帧体 + clientId。 */
  const messages = (fake: FakeWebSocket, key?: Uint8Array): SentMessage[] =>
    outbound(fake, key).flatMap((f) => {
      if (f === null || f.type !== "message") {
        return [];
      }
      const { clientId, ...rest } = f;
      return [{ frame: rest, clientId: clientId as string }];
    });
  const box = (): HTMLTextAreaElement => screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
  /** 输入框打字 + 点「发送」（命令文本会唤起命令面板，Enter 被面板接管，故点按钮）。 */
  const send = (text: string): void => {
    fireEvent.change(box(), { target: { value: text } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
  };
  /** 当前会话消息区里，正文为 text 的全部用户气泡外层节点（按出现顺序）。 */
  const userWraps = (text: string): HTMLElement[] =>
    within(screen.getByRole("main"))
      .getAllByText(text)
      .map((el) => el.closest(".bg-emerald-600")!.parentElement as HTMLElement);
  const userWrap = (text: string): HTMLElement => userWraps(text)[0];
  /** 某条用户气泡的各级子节点类名（时间戳 / 气泡 / 状态行）。 */
  const childClasses = (wrap: HTMLElement): string[] => Array.from(wrap.children).map((c) => c.className);
  /** 「重发」按钮（全局恰一个时可这样取）。 */
  const resendButton = (): HTMLElement => screen.getByRole("button", { name: "重发这条消息" });

  /** 测试网关（与其它 describe 内同名助手同构——该助手为 describe 局部，此处复制
   *  一份；长期 X25519 + ed25519 身份密钥）。 */
  function makeGateway() {
    const gkPriv = x25519.utils.randomSecretKey();
    const gk = bytesToHex(x25519.getPublicKey(gkPriv));
    const idPriv = ed25519.utils.randomSecretKey();
    const ik = bytesToHex(ed25519.getPublicKey(idPriv));
    const ackFor = (hello: { ek: string; n1: string }, machine: string) => {
      const n2 = bytesToHex(randomBytes(32));
      const msg = new Uint8Array(128);
      msg.set(hexToBytes(hello.ek), 0);
      msg.set(hexToBytes(gk), 32);
      msg.set(hexToBytes(hello.n1), 64);
      msg.set(hexToBytes(n2), 96);
      return {
        v: 1 as const,
        type: "e2e_ack" as const,
        machine,
        gk,
        n2,
        sig: bytesToHex(ed25519.sign(msg, idPriv)),
        ik,
      };
    };
    const gatewayKey = (hello: { ek: string; n1: string }, n2: string) => {
      const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
      const ikm = new Uint8Array(96);
      ikm.set(shared, 0);
      ikm.set(hexToBytes(hello.n1), 32);
      ikm.set(hexToBytes(n2), 64);
      return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
    };
    return { gk, ik, ackFor, gatewayKey };
  }

  /** 从 fake.sent 里取最近一条 e2e_hello（无则抛）。 */
  function lastHelloFor(fake: FakeWebSocket): { ek: string; n1: string } {
    const raw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "e2e_hello").at(-1);
    if (!raw) {
      throw new Error("尚未发出 e2e_hello");
    }
    return JSON.parse(raw);
  }

  /** 挂载并把机器 M 的加密会话带到 active（预置 pin → 握手直接 active，无 TOFU）。
   *  返回活跃 socket、测试网关与网关侧会话密钥（出站 enc 用它解、入站业务帧用它封）。
   *  假时钟须在调用前装好（退避等定时器在挂载时创建）。 */
  function mountEncrypted(): { fake: FakeWebSocket; gw: ReturnType<typeof makeGateway>; key: Uint8Array } {
    const gw = makeGateway();
    localStorage.setItem("imagent.e2ePins", JSON.stringify({ [M]: { ik: gw.ik, gk: gw.gk, pinnedAt: 1 } }));
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
      fake.recv({ v: 1, type: "machine", online: false });
      fake.recv({ v: 1, type: "machines", online: [M] });
    });
    const hello = lastHelloFor(fake);
    const ack = gw.ackFor(hello, M);
    act(() => {
      fake.recv(ack);
    });
    expect(screen.getByText("已加密")).toBeTruthy();
    return { fake, gw, key: gw.gatewayKey(hello, ack.n2) };
  }

  /** 网关经 send_business 加密下发一帧业务帧。 */
  const gwSend = (fake: FakeWebSocket, key: Uint8Array, frame: Frame): void => {
    act(() => {
      fake.recv(sealFrame(M, key, frame as unknown as InboundFrame));
    });
  };

  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  it("① ready + 收到 machines 帧 → 对**全部**在线机器各发一条 gateway_info_query（帧形状即契约原文）；切机 / machines 重排不重发；单机模式不发", () => {
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
    });
    // hello_ack 后尚未收到 machines 帧：不发（单机模式旧网关不认识该帧）
    expect(infoQueries(fake)).toEqual([]);
    act(() => {
      fake.recv({ v: 1, type: "machine", online: false });
      fake.recv({ v: 1, type: "machines", online: [M, "work-mac", "office-mac"] });
    });
    // 当前机器是 home-mac（自动选中首台）；三台各一条
    expect(infoQueries(fake)).toEqual([M, "work-mac", "office-mac"]);
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "gateway_info_query", machine: "work-mac" }));
    fireEvent.click(screen.getByRole("button", { name: /work-mac/ }));
    act(() => {
      fake.recv({ v: 1, type: "machines", online: ["work-mac", M] });
    });
    expect(infoQueries(fake)).toEqual([M, "work-mac", "office-mac"]);
    cleanup();
    const single = mountApp();
    act(() => {
      single.emit("open");
      single.recv({ v: 1, type: "hello_ack", role: "user" });
      single.recv({ v: 1, type: "machine", online: true });
    });
    expect(screen.getByText("已连接")).toBeTruthy();
    expect(infoQueries(single)).toEqual([]);
  });

  it("①b 该机器加密会话建立（E2E active）后再查一次，走密文；同时作废上一代会话里问到的能力——新应答回来之前发出的消息不跟踪送达，应答到达后恢复", () => {
    const gw = makeGateway();
    const fake = mountApp();
    act(() => {
      fake.emit("open");
      fake.recv({ v: 1, type: "hello_ack", role: "user" });
      fake.recv({ v: 1, type: "machines", online: [M] });
    });
    // E1：ready + machines 后已发一条明文查询（握手窗口）
    expect(infoQueries(fake)).toEqual([M]);
    // 握手窗口里网关的应答到达（此时机器尚未加密，明文放行）：能力已记录
    act(() => {
      fake.recv(INFO());
    });
    send("握手窗口里的消息");
    expect(screen.getByText("发送中")).toBeTruthy();
    // 网关 ack → confirming（TOFU 弹层）：fail-closed 期间不发第二条（明文 / 密文都没有）
    const firstHello = lastHelloFor(fake);
    const firstAck = gw.ackFor(firstHello, M);
    act(() => {
      fake.recv(firstAck);
    });
    expect(screen.getByRole("dialog", { name: "确认机器加密指纹" })).toBeTruthy();
    expect(infoQueries(fake)).toEqual([M]);
    expect(fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "enc")).toHaveLength(0);
    // 信任 → 以 pin 身份重新握手 → 新 ack → active
    fireEvent.click(screen.getByRole("button", { name: "信任并启用加密" }));
    const redone = lastHelloFor(fake);
    const secondAck = gw.ackFor(redone, M);
    act(() => {
      fake.recv(secondAck);
    });
    const key = gw.gatewayKey(redone, secondAck.n2);
    // E2：active 后的第二次查询走密文——明文仍只有 E1 那一条；密文里恰一条、机器对
    expect(infoQueries(fake, key)).toEqual([M, M]);
    expect(infoQueries(fake)).toEqual([M]);
    // 加密会话换代：上一代会话里问到的能力作废——握手窗口那条「发送中」收敛为
    // 「未确认送达」，此刻发出的新消息不带送达状态（能力暂时未知，按旧网关处理）
    expect(screen.queryByText("发送中")).toBeNull();
    expect(screen.getByText("未确认送达")).toBeTruthy();
    send("能力待定窗口里的消息");
    expect(screen.queryByText("发送中")).toBeNull();
    expect(screen.getAllByText("未确认送达")).toHaveLength(1);
    // 新网关的密文应答到达 → 能力确认 → 之后的消息恢复跟踪（先前那条不回头补）
    gwSend(fake, key, INFO());
    send("能力确认后的消息");
    expect(screen.getByText("发送中")).toBeTruthy();
    expect(userWraps("能力待定窗口里的消息")[0].children).toHaveLength(2);
  });

  it("② 发送 → 回显带 clientId 与 pending（「发送中」）→ 收 ack{received} → sent：状态行消失，气泡各级类名与不支持回执时逐项一致；message 帧的 clientId 即回执关联的 id", () => {
    const { fake, key } = mountEncrypted();
    // 对照：能力未知时发出的回显（旧网关的形态）——时间戳 + 气泡，没有状态行
    send("能力到来之前");
    const plain = userWrap("能力到来之前");
    expect(plain.children).toHaveLength(2);
    expect(screen.queryByText("发送中")).toBeNull();
    gwSend(fake, key, INFO());
    send("帮我跑下构建");
    const frames = messages(fake, key);
    expect(frames.at(-1)!.frame).toEqual({
      v: 1,
      type: "message",
      convId: "web:default",
      text: "帮我跑下构建",
      machine: M,
    });
    expect(frames.at(-1)!.clientId).toMatch(/^m_[0-9a-z]+_[0-9a-f]{8}$/);
    expect(frames[0].clientId).not.toBe(frames.at(-1)!.clientId); // 每次发送都是新的
    const pending = userWrap("帮我跑下构建");
    expect(childClasses(pending)).toHaveLength(3); // 时间戳 + 气泡 + 「发送中」
    expect(pending.querySelector(".bg-emerald-600")!.className).toContain("opacity-70");
    expect(within(pending).getByText("发送中")).toBeTruthy();
    // 别的设备发的消息的回执（本机没有对应条目）：忽略，不影响这条
    gwSend(fake, key, ACK_RECV("m_other0_00000000"));
    expect(screen.getByText("发送中")).toBeTruthy();
    // 网关回执（密文）：按 clientId 关联 → sent
    gwSend(fake, key, ACK_RECV(frames.at(-1)!.clientId));
    const sent = userWrap("帮我跑下构建");
    expect(childClasses(sent)).toEqual(childClasses(plain));
    expect(sent.querySelector(".bg-emerald-600")!.className).not.toContain("opacity-70");
    expect(sent.textContent!.endsWith("帮我跑下构建")).toBe(true); // 气泡之后没有任何状态文字
    expect(screen.queryByText("发送中")).toBeNull();
    expect(screen.queryByRole("button", { name: "重发这条消息" })).toBeNull();
  });

  it("③ 收 ack{rejected} → 气泡下出现原因与「重发」；点重发 → 发出新帧（新 clientId、同一段文字）且原条目不变；输入框不被填回", () => {
    const { fake, key } = mountEncrypted();
    gwSend(fake, key, INFO());
    send("rm -rf /tmp/x");
    const id1 = messages(fake, key).at(-1)!.clientId;
    expect(screen.queryByText(/未送达/)).toBeNull();
    gwSend(fake, key, ACK_REJECT(id1));
    const wrap = userWrap("rm -rf /tmp/x");
    expect(within(wrap).getByText("未送达：发送者未获授权，消息未被处理")).toBeTruthy();
    expect(resendButton().textContent).toBe("重发");
    expect(wrap.querySelector(".bg-emerald-600")!.className).not.toContain("opacity-70");
    const originalHtml = wrap.innerHTML;
    const sentBefore = messages(fake, key).length;
    fireEvent.click(resendButton());
    // 新帧：同一段文字、新的 clientId；原条目原样，另起一条 pending 的回显
    const frames = messages(fake, key);
    expect(frames).toHaveLength(sentBefore + 1);
    expect(frames.at(-1)!.frame).toEqual({ v: 1, type: "message", convId: "web:default", text: "rm -rf /tmp/x", machine: M });
    expect(frames.at(-1)!.clientId).not.toBe(id1);
    expect(frames.at(-1)!.clientId).toMatch(/^m_[0-9a-z]+_[0-9a-f]{8}$/);
    const wraps = userWraps("rm -rf /tmp/x");
    expect(wraps).toHaveLength(2);
    expect(wraps[0].innerHTML).toBe(originalHtml);
    expect(childClasses(wraps[1])).toHaveLength(3);
    expect(within(wraps[1]).getByText("发送中")).toBeTruthy();
    expect(box().value).toBe("");
  });

  it("④ 机器不支持回执（没有 gateway_info）→ 15 秒后仍不出现「未确认送达」；支持回执 → 推进 15 秒出现（差一点不出现）", () => {
    vi.useFakeTimers();
    const { fake, key } = mountEncrypted();
    // 旧网关：对 gateway_info_query 不回任何帧
    send("旧网关上的消息");
    expect(screen.queryByText("发送中")).toBeNull();
    act(() => vi.advanceTimersByTime(ACK_TIMEOUT_MS + ACK_TIMEOUT_TICK_MS));
    expect(screen.queryByText("未确认送达")).toBeNull();
    expect(screen.queryByRole("button", { name: "重发这条消息" })).toBeNull();
    // 新网关的应答到达 → 支持回执
    gwSend(fake, key, INFO());
    send("新网关上的消息");
    expect(screen.getByText("发送中")).toBeTruthy();
    act(() => vi.advanceTimersByTime(ACK_TIMEOUT_MS - 1));
    expect(screen.queryByText("未确认送达")).toBeNull();
    act(() => vi.advanceTimersByTime(ACK_TIMEOUT_TICK_MS));
    expect(screen.getByText("未确认送达")).toBeTruthy();
    expect(screen.queryByText("发送中")).toBeNull();
    expect(within(userWrap("新网关上的消息")).getByRole("button", { name: "重发这条消息" })).toBeTruthy();
  });

  it("⑤ ws 掉线（非 ready）：发出满 2 秒的 pending 回显 → 未确认送达（不必等 15 秒）；未满 2 秒不动", () => {
    vi.useFakeTimers();
    const { fake, key } = mountEncrypted();
    gwSend(fake, key, INFO());
    send("断线前发出的");
    expect(screen.getByText("发送中")).toBeTruthy();
    act(() => {
      fake.emit("close", { code: 1006 });
    });
    expect(screen.getByText("重连中")).toBeTruthy();
    act(() => vi.advanceTimersByTime(1000)); // 退避到点：新连接已建但尚未就绪
    expect(screen.getByText("发送中")).toBeTruthy(); // 发出刚满 1 秒：还不算
    act(() => vi.advanceTimersByTime(1100));
    expect(screen.getByText("未确认送达")).toBeTruthy();
    expect(screen.queryByText("发送中")).toBeNull();
  });

  it("⑥ 迟到的 ack{received} 把「未确认送达」翻回已送达：状态行与「重发」按钮消失", () => {
    vi.useFakeTimers();
    const { fake, key } = mountEncrypted();
    gwSend(fake, key, INFO());
    send("迟到回执的那条");
    const id = messages(fake, key).at(-1)!.clientId;
    act(() => vi.advanceTimersByTime(ACK_TIMEOUT_MS + ACK_TIMEOUT_TICK_MS));
    expect(screen.getByText("未确认送达")).toBeTruthy();
    gwSend(fake, key, ACK_RECV(id));
    expect(screen.queryByText("未确认送达")).toBeNull();
    expect(screen.queryByRole("button", { name: "重发这条消息" })).toBeNull();
    expect(screen.queryByText("发送中")).toBeNull();
    expect(userWrap("迟到回执的那条").children).toHaveLength(2);
  });

  it("⑦ ws 未 ready 时点重发：不出帧，文字回到输入框（接在已有草稿之后）+ toast「连接未就绪，已放回输入框」；原条目不变", () => {
    vi.useFakeTimers();
    const { fake, key } = mountEncrypted();
    gwSend(fake, key, INFO());
    send("要重发的消息");
    gwSend(fake, key, ACK_REJECT(messages(fake, key).at(-1)!.clientId));
    expect(screen.getByText("未送达：发送者未获授权，消息未被处理")).toBeTruthy();
    fireEvent.change(box(), { target: { value: "正在写的" } });
    act(() => {
      fake.emit("close", { code: 1006 });
    });
    expect(screen.getByText("重连中")).toBeTruthy();
    const sentBefore = fake.sent.length;
    fireEvent.click(resendButton());
    expect(fake.sent).toHaveLength(sentBefore);
    expect(box().value).toBe("正在写的\n要重发的消息");
    expect(screen.getByText("连接未就绪，已放回输入框")).toBeTruthy();
    expect(screen.getByText("未送达：发送者未获授权，消息未被处理")).toBeTruthy();
    expect(messages(fake, key)).toHaveLength(1);
  });

  it("⑦b 连接态是 ready 但发送被拒（重新握手后网关 10 秒不应答 → 加密会话不可用、未放行明文）：点重发同样放回输入框 + toast，不出帧", () => {
    vi.useFakeTimers();
    const { fake, key } = mountEncrypted();
    gwSend(fake, key, INFO());
    send("握手超时前被拒的那条");
    gwSend(fake, key, ACK_REJECT(messages(fake, key).at(-1)!.clientId));
    expect(resendButton()).toBeTruthy();
    // 掉线 → 退避 → 新连接就绪、机器快照到达 → 重新发起握手，但网关这次不应答
    act(() => {
      fake.emit("close", { code: 1006 });
    });
    act(() => vi.advanceTimersByTime(1000));
    const next = FakeWebSocket.latest as FakeWebSocket;
    act(() => {
      next.emit("open");
      next.recv({ v: 1, type: "hello_ack", role: "user" });
      next.recv({ v: 1, type: "machine", online: false });
      next.recv({ v: 1, type: "machines", online: [M] });
    });
    act(() => vi.advanceTimersByTime(10_000));
    // 连接态 ready，但该机器发送被阻（横幅 + 输入框禁用）
    expect(screen.getByText(/不支持加密连接/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "发送" }) as HTMLButtonElement).disabled).toBe(true);
    const sentBefore = next.sent.length;
    fireEvent.click(resendButton());
    expect(next.sent).toHaveLength(sentBefore);
    expect(box().value).toBe("握手超时前被拒的那条");
    expect(screen.getByText("连接未就绪，已放回输入框")).toBeTruthy();
    expect(screen.getByText("未送达：发送者未获授权，消息未被处理")).toBeTruthy();
  });

  it("⑧ 管理页：有 gateway_info 的机器行显示「网关 v1.44.0」，没有的不显示（旧网关 / 本次没在线过）；版本随网关应答更新", async () => {
    // 机器列表只在本浏览器支持 WebAuthn 时渲染（同其它管理面板用例的最小桩）；
    // re-auth 的 fetch 一律 404——机器列表由下面直接喂 machines_cfg 帧
    vi.stubGlobal("PublicKeyCredential", class PublicKeyCredential {});
    Object.defineProperty(navigator, "credentials", {
      value: {
        get: async () => {
          throw new Error("本用例不触发 re-auth");
        },
        create: async () => {
          throw new Error("本用例不触发 create");
        },
      },
      configurable: true,
    });
    vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", { status: 404 })));
    localStorage.setItem(
      "imagent.session",
      JSON.stringify({ id: "sid-r", token: "st-r", expiresAt: Date.now() + 20 * DAY }),
    );
    const fake = mountApp();
    handshake(fake, [M, "work-mac"]);
    // 只有 home-mac 应答了能力查询
    act(() => {
      fake.recv(INFO());
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "管理" }));
    });
    const dialog = screen.getByRole("dialog", { name: "管理" });
    expect(within(dialog).queryByText(/网关 v/)).toBeNull(); // 机器列表还没回来
    act(() => {
      fake.recv({
        v: 1,
        type: "machines_cfg",
        machines: [
          { name: M, pubkey_fp: "aaaa bbbb cccc dddd", created: Math.floor(Date.now() / 1000) - 30 },
          { name: "work-mac", pubkey_fp: "aaaa bbbb cccc dddd", created: Math.floor(Date.now() / 1000) - 60 },
          { name: "office-mac", pubkey_fp: "aaaa bbbb cccc dddd", created: Math.floor(Date.now() / 1000) - 90 },
        ],
      });
    });
    expect(within(dialog).getByText("网关 v1.44.0")).toBeTruthy();
    const list = within(dialog).getByRole("list", { name: "机器配置列表" });
    const rows = within(list).getAllByRole("listitem");
    expect(rows.map((r) => r.textContent?.includes("网关 v") ?? false)).toEqual([true, false, false]);
    expect(rows[0].textContent).toContain(M);
    expect(rows[0].textContent).toContain("网关 v1.44.0");
    // 网关升级后重新应答：版本跟随
    act(() => {
      fake.recv(INFO(M, "1.44.1"));
    });
    expect(within(dialog).getByText("网关 v1.44.1")).toBeTruthy();
    expect(within(dialog).queryByText("网关 v1.44.0")).toBeNull();
  });

  it("⑨ 加密会话换代（重连后重新握手）时作废旧能力：网关此后不再应答（换成了旧二进制）→ 之前「发送中」的收敛为「未确认送达」，新消息不再跟踪", () => {
    vi.useFakeTimers();
    const { fake, gw, key } = mountEncrypted();
    gwSend(fake, key, INFO());
    send("换代前还在等回执");
    expect(screen.getByText("发送中")).toBeTruthy();
    // 掉线 → 退避 → 新连接 → 机器快照 → 重新握手（已 pin，网关应答即 active）
    act(() => {
      fake.emit("close", { code: 1006 });
    });
    act(() => vi.advanceTimersByTime(1000));
    const next = FakeWebSocket.latest as FakeWebSocket;
    expect(next).not.toBe(fake);
    act(() => {
      next.emit("open");
      next.recv({ v: 1, type: "hello_ack", role: "user" });
      next.recv({ v: 1, type: "machine", online: false });
      next.recv({ v: 1, type: "machines", online: [M] });
    });
    const hello = lastHelloFor(next);
    const ack = gw.ackFor(hello, M);
    act(() => {
      next.recv(ack);
    });
    const key2 = gw.gatewayKey(hello, ack.n2);
    expect(screen.getByText("已加密")).toBeTruthy();
    // 重查确实发出（握手窗口一条明文 + active 后一条密文）；旧网关不回 → 能力保持作废
    expect(infoQueries(next, key2)).toEqual([M, M]);
    // 此刻才过 1 秒出头（2 秒的断线规则还没到点）：收敛只能来自会话换代的作废
    expect(screen.queryByText("发送中")).toBeNull();
    expect(screen.getByText("未确认送达")).toBeTruthy();
    send("换代后的新消息");
    expect(screen.queryByText("发送中")).toBeNull();
    act(() => vi.advanceTimersByTime(ACK_TIMEOUT_MS + ACK_TIMEOUT_TICK_MS));
    expect(screen.getAllByText("未确认送达")).toHaveLength(1);
    expect(messages(next, key2).map((m) => m.frame.text)).toEqual(["换代后的新消息"]);
    // 新网关的应答到达 → 恢复跟踪
    act(() => {
      next.recv(sealFrame(M, key2, INFO() as unknown as InboundFrame));
    });
    send("能力重新确认后");
    expect(screen.getByText("发送中")).toBeTruthy();
  });

  it("⑩ 刷新恢复：落盘时还在等回执的回显按「未确认送达」恢复（不再「发送中」），已拒绝的原样；迟到的回执照常把它翻回已送达", async () => {
    setHistoryBackend(memoryHistoryBackend());
    await historyStore.saveConv(M, "web:default", [
      { kind: "user", key: "user:m_abc000_00000001", text: "刷新前没等到回执", at: Date.now() - 60_000, clientId: "m_abc000_00000001", ack: "pending" },
      { kind: "user", key: "user:m_abc000_00000002", text: "刷新前已被拒绝", at: Date.now() - 50_000, clientId: "m_abc000_00000002", ack: "failed", ackReason: "排队消息过多，这条被丢弃" },
    ]);
    try {
      const { fake, key } = mountEncrypted();
      await act(async () => {}); // loadAll 的微任务
      expect(screen.queryByText("发送中")).toBeNull();
      expect(screen.getByText("未确认送达")).toBeTruthy();
      expect(screen.getByText("未送达：排队消息过多，这条被丢弃")).toBeTruthy();
      expect(screen.getAllByRole("button", { name: "重发这条消息" })).toHaveLength(2);
      // 迟到的回执：恢复条目的键带 h: 前缀，照样按 clientId 关联
      gwSend(fake, key, ACK_RECV("m_abc000_00000001"));
      expect(screen.queryByText("未确认送达")).toBeNull();
      expect(screen.getByText("未送达：排队消息过多，这条被丢弃")).toBeTruthy();
      expect(screen.getAllByRole("button", { name: "重发这条消息" })).toHaveLength(1);
    } finally {
      setHistoryBackend(null);
    }
  });
});
