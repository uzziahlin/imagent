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
import { historyStore } from "./lib/historyStore";
import { fingerprint, openBox } from "./lib/e2e";
import type { WsEvent } from "./lib/ws";

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
    // pin 落库 + StatusBar 锁标 + 弹层关闭
    expect(JSON.parse(localStorage.getItem("imagent.e2ePins") ?? "{}")).toHaveProperty("home-mac");
    expect(screen.getByText("已加密")).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: "确认机器加密指纹" })).toBeNull();

    // 发送：出站为 enc 帧，网关密钥可解回原帧
    fireEvent.change(screen.getByLabelText("消息输入框"), { target: { value: "机密指令" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    const encRaw = fake.sent.filter((s) => (JSON.parse(s) as { type: string }).type === "enc").at(-1);
    expect(encRaw).toBeTruthy();
    const enc = JSON.parse(encRaw!) as { machine: string; box: string };
    expect(enc.machine).toBe("home-mac");
    expect(openBox(gw.gatewayKey(hello, ack.n2), enc.box)).toBe(
      JSON.stringify({ v: 1, type: "message", convId: "web:default", text: "机密指令", machine: "home-mac" }),
    );
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
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "message", convId: "web:default", text: "明文", machine: "home-mac" }),
    );
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
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "message", text: "/switch proj", machine: "home-mac" }));
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
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "message", text: "/switch plain", machine: "home-mac" }));
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
    expect(fake.sent).toContain(JSON.stringify({ v: 1, type: "message", text: "/switch legacy" }));
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
    expect(fake.sent).toContain(
      JSON.stringify({ v: 1, type: "message", convId: `web:${shortId}`, text: "帮我写个部署脚本" }),
    );
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
    fireEvent.click(screen.getByRole("button", { name: /work/ }));
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
    fireEvent.click(screen.getByRole("button", { name: /work/ }));
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
