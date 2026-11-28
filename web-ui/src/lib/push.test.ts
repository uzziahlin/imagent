// lib/push 单测（v1.10 Web Push）：mock fetch + 全局 stub（Notification /
// navigator.serviceWorker，参照 webauthn.test 的 stub 风格——jsdom 无这些
// API，经 expando 注入）。覆盖：notificationCopy 全 kind + 未知兜底、
// urlB64ToUint8Array 标准样例往返、enablePush 成功/各失败码（浏览器不支持 /
// iOS 未添加主屏 / 权限拒绝 / 订阅失败 / 服务端拒绝 / 网络错误 / 无会话）、
// disablePush（有订阅/无订阅/服务端失败仍本地退订）、open-conv 消息监听、
// ?conv= 深链参数校验、initServiceWorker（v1.39 更新接管 controllerchange
// 自动重载 + 首装守卫 + cleanup 解绑；v1.41.5 运行中更新检查——定时 +
// visibilitychange 两个触发源、cleanup 回收、update reject 静默、卸载竞态；
// v1.41.9 多余更新提示的抑制——pageIsCurrentShell 判定的各形态、页面已是新
// 构建时不提示也不驱动接管、无法判定 / 旧构建照常提示；v1.42 窗口回信推送
// （peer_reply 文案 + peer: 深链参数）。
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  listenServiceWorkerMessages,
  notificationCopy,
  pageIsCurrentShell,
  pushSupported,
  urlB64ToUint8Array,
  validConvParam,
  validMachineParam,
  enablePush,
  disablePush,
  initServiceWorker,
  type ShellCacheStorage,
} from "./push";
import { toB64Url } from "./webauthn";

// ---------------------------------------------------------------------------
// 测试基座：可控 fetch + 全局 stub（Notification / serviceWorker / PushManager）
// ---------------------------------------------------------------------------

interface FetchCall {
  url: string;
  init: RequestInit | undefined;
}

type Responder = (
  url: string,
  init: RequestInit | undefined,
) => { status: number; body?: unknown } | undefined;

/** 可控 fetch：按 URL 匹配回包；未命中 404（unexpected_url）。 */
function makeFetch(respond: Responder): { fetch: typeof fetch; calls: FetchCall[] } {
  const calls: FetchCall[] = [];
  const impl = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
    const u = typeof url === "string" ? url : url.toString();
    calls.push({ url: u, init });
    const hit = respond(u, init);
    if (!hit) {
      return new Response(JSON.stringify({ error: "unexpected_url" }), { status: 404 });
    }
    return new Response(JSON.stringify(hit.body ?? {}), { status: hit.status });
  }) as unknown as typeof fetch;
  return { fetch: impl, calls };
}

/** pushManager 缺省 mock 形状（覆写场景经 cast 断言）。 */
type PushManagerStub = {
  subscribe: ReturnType<typeof vi.fn>;
  getSubscription: ReturnType<typeof vi.fn>;
};

/** SW 侧 stub 套件：registration.pushManager（订阅 mock）+ serviceWorker
 *  容器（register / getRegistration / 消息监听记录）。 */
function makeServiceWorkerStub(opts: { pushManager?: unknown } = {}) {
  const subscription = {
    endpoint: "https://push.example/sub-1",
    toJSON: () => ({
      endpoint: "https://push.example/sub-1",
      keys: { p256dh: "key-p256dh", auth: "key-auth" },
    }),
    unsubscribe: vi.fn(async () => true),
  };
  const pushManager = (
    "pushManager" in opts
      ? opts.pushManager
      : {
          subscribe: vi.fn(async () => subscription),
          getSubscription: vi.fn(async () => subscription),
        }
  ) as PushManagerStub;
  const registration = {
    pushManager,
    installing: null,
    addEventListener: vi.fn(),
    update: vi.fn(async () => undefined),
  };
  const container = {
    register: vi.fn(async () => registration),
    getRegistration: vi.fn(async () => registration),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    controller: null,
  };
  return { subscription, pushManager, registration, container };
}

/** 注入 pushSupported() 所需的全局能力（Notification / PushManager /
 *  navigator.serviceWorker——jsdom 实例级 expando，与 webauthn.test 的
 *  userAgent shadow 同法）。 */
function stubPushGlobals(container?: unknown): void {
  (globalThis as unknown as { Notification?: unknown }).Notification = {
    requestPermission: vi.fn(async () => "granted"),
  };
  (globalThis as unknown as { PushManager?: unknown }).PushManager =
    function FakePushManager() {};
  if (container !== undefined) {
    (navigator as unknown as { serviceWorker?: unknown }).serviceWorker = container;
  }
}

function requestPermissionMock(result: string): void {
  (globalThis as unknown as { Notification: { requestPermission: () => Promise<string> } })
    .Notification.requestPermission = vi.fn(async () => result);
}

const BASE = "/imagent/";
/** 标准 VAPID 公钥形态样例：65 字节（0x04 前缀 + 64 字节坐标）。 */
const VAPID_KEY_BYTES = new Uint8Array([
  0x04,
  ...Array.from({ length: 64 }, (_, i) => (i * 5 + 3) % 256),
]);
const VAPID_PUBLIC_KEY = toB64Url(VAPID_KEY_BYTES);

afterEach(() => {
  delete (globalThis as unknown as { Notification?: unknown }).Notification;
  delete (globalThis as unknown as { PushManager?: unknown }).PushManager;
  delete (navigator as unknown as { serviceWorker?: unknown }).serviceWorker;
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// notificationCopy（payload → 通知文案映射）
// ---------------------------------------------------------------------------

describe("notificationCopy", () => {
  it("三种 kind 映射：title 固定文案，title 摘要作正文", () => {
    expect(notificationCopy("round_done", "跑测试套件")).toEqual({
      title: "✅ 任务完成",
      body: "跑测试套件",
    });
    expect(notificationCopy("round_error", "npm install 失败")).toEqual({
      title: "⚠️ 任务出错",
      body: "npm install 失败",
    });
    expect(notificationCopy("approval", "bash")).toEqual({
      title: "🔐 等待审批",
      body: "bash",
    });
  });

  it("缺摘要 / 空白摘要用 kind 兜底正文", () => {
    expect(notificationCopy("round_done", undefined)).toEqual({
      title: "✅ 任务完成",
      body: "任务已执行完成",
    });
    expect(notificationCopy("round_error", "   ")).toEqual({
      title: "⚠️ 任务出错",
      body: "任务执行出错，请查看详情",
    });
    expect(notificationCopy("approval", "")).toEqual({
      title: "🔐 等待审批",
      body: "有操作等待你的审批",
    });
  });

  it("未知 / 缺失 kind 兜底（载荷损坏时 push 事件传 null）", () => {
    expect(notificationCopy("unknown_kind", "摘要")).toEqual({
      title: "imagent 通知",
      body: "摘要",
    });
    expect(notificationCopy(undefined, undefined)).toEqual({
      title: "imagent 通知",
      body: "有新事件，点击查看",
    });
    expect(notificationCopy(null, null)).toEqual({
      title: "imagent 通知",
      body: "有新事件，点击查看",
    });
  });

  // v1.42：本地窗口（peer 通道）回信推送
  it("peer_reply：标题「💬 窗口回复」，正文取回信摘要；空白摘要用兜底文案", () => {
    expect(notificationCopy("peer_reply", "已修好")).toEqual({
      title: "💬 窗口回复",
      body: "已修好",
    });
    // 契约：title 是回信正文前 32 码点，可为空串
    expect(notificationCopy("peer_reply", "")).toEqual({
      title: "💬 窗口回复",
      body: "本地窗口有了新回复",
    });
    expect(notificationCopy("peer_reply", "  \n")).toEqual({
      title: "💬 窗口回复",
      body: "本地窗口有了新回复",
    });
    expect(notificationCopy("peer_reply", undefined)).toEqual({
      title: "💬 窗口回复",
      body: "本地窗口有了新回复",
    });
  });

  // v1.43：本地窗口状态变化推送（契约 §3）。title = 窗口名（经 push_notify_title
  // 截 32 码点，可为空 → 走兜底文案）
  it("window_turn_end：标题「🏁 窗口完成一轮」，正文取窗口名；空白 / 缺失摘要用兜底文案", () => {
    expect(notificationCopy("window_turn_end", "jormungandr-f8")).toEqual({
      title: "🏁 窗口完成一轮",
      body: "jormungandr-f8",
    });
    for (const blank of ["", "  \n", undefined, null]) {
      expect(notificationCopy("window_turn_end", blank), String(blank)).toEqual({
        title: "🏁 窗口完成一轮",
        body: "你关注的窗口跑完了",
      });
    }
  });

  it("window_waiting：标题「⏸ 窗口在等你」，正文取窗口名；空白 / 缺失摘要用兜底文案", () => {
    expect(notificationCopy("window_waiting", "imagent-a7")).toEqual({
      title: "⏸ 窗口在等你",
      body: "imagent-a7",
    });
    for (const blank of ["", " \n ", undefined, null]) {
      expect(notificationCopy("window_waiting", blank), String(blank)).toEqual({
        title: "⏸ 窗口在等你",
        body: "有个本地窗口卡住了",
      });
    }
  });

  it("两个窗口 kind 互不串台；字面量须精确匹配（连字符 / 尾随空白的近似 kind 仍走兜底）", () => {
    expect(notificationCopy("window_turn_end", "w")).not.toEqual(notificationCopy("window_waiting", "w"));
    expect(notificationCopy("window_turn_end", "w")).not.toEqual(notificationCopy("peer_reply", "w"));
    expect(notificationCopy("window_waiting", "w")).not.toEqual(notificationCopy("approval", "w"));
    expect(notificationCopy("window-turn-end", "w")).toEqual({ title: "imagent 通知", body: "w" });
    expect(notificationCopy("window_turn_end ", "w")).toEqual({ title: "imagent 通知", body: "w" });
    expect(notificationCopy("window_wait", "w")).toEqual({ title: "imagent 通知", body: "w" });
  });
});

// ---------------------------------------------------------------------------
// urlB64ToUint8Array（VAPID applicationServerKey 解码）
// ---------------------------------------------------------------------------

describe("urlB64ToUint8Array", () => {
  it("标准 VAPID 公钥样例（65 字节 P-256 形态）往返", () => {
    const out = urlB64ToUint8Array(VAPID_PUBLIC_KEY);
    expect(out).toHaveLength(65);
    expect(Array.from(out)).toEqual(Array.from(VAPID_KEY_BYTES));
    // 编码无 padding、url-safe 字母表
    expect(VAPID_PUBLIC_KEY).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(VAPID_PUBLIC_KEY).not.toContain("=");
  });

  it("空串与 padding 容忍", () => {
    expect(urlB64ToUint8Array("")).toHaveLength(0);
    const bytes = new Uint8Array([0xfb, 0xff, 0x01]);
    expect(Array.from(urlB64ToUint8Array(toB64Url(bytes) + "=="))).toEqual(
      Array.from(bytes),
    );
  });
});

// ---------------------------------------------------------------------------
// pushSupported
// ---------------------------------------------------------------------------

describe("pushSupported", () => {
  it("jsdom 默认（无 SW/Push/Notification）：false", () => {
    expect(pushSupported()).toBe(false);
  });

  it("全局能力注入后：true", () => {
    stubPushGlobals(makeServiceWorkerStub().container);
    expect(pushSupported()).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// enablePush
// ---------------------------------------------------------------------------

describe("enablePush", () => {
  it("成功全流程：status 取 VAPID（Bearer）→ subscribe（key 解码正确）→ 登记契约 body", async () => {
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    const { fetch, calls } = makeFetch((url) => {
      if (url === `${BASE}push/status`) {
        return { status: 200, body: { supported: true, subscriptions: 0, vapidPublicKey: VAPID_PUBLIC_KEY } };
      }
      if (url === `${BASE}push/subscribe`) {
        return { status: 200, body: {} };
      }
      return undefined;
    });

    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });

    expect(result).toEqual({ ok: true });
    // SW 注册（幂等地址）
    expect(sw.container.register).toHaveBeenCalledWith("/imagent/sw.js");
    // status GET：Bearer sessionToken
    const status = calls.find((c) => c.url === `${BASE}push/status`);
    expect(status?.init).toBeDefined();
    expect(status?.init?.headers).toEqual({ authorization: "Bearer tok-1" });
    // pushManager.subscribe：userVisibleOnly + applicationServerKey（解码回原字节）
    expect(sw.pushManager.subscribe).toHaveBeenCalledTimes(1);
    const opts = sw.pushManager.subscribe.mock.calls[0]![0] as {
      userVisibleOnly: boolean;
      applicationServerKey: Uint8Array;
    };
    expect(opts.userVisibleOnly).toBe(true);
    expect(Array.from(opts.applicationServerKey)).toEqual(Array.from(VAPID_KEY_BYTES));
    // 登记契约：POST body { endpoint, keys:{p256dh, auth} } + Bearer
    const post = calls.find((c) => c.url === `${BASE}push/subscribe`);
    expect(post?.init?.method).toBe("POST");
    expect(JSON.parse(String(post?.init?.body))).toEqual({
      endpoint: "https://push.example/sub-1",
      keys: { p256dh: "key-p256dh", auth: "key-auth" },
    });
    expect((post?.init?.headers as Record<string, string>).authorization).toBe("Bearer tok-1");
  });

  it("浏览器不支持（无全局能力）：not_supported，register 未调用", async () => {
    const { fetch, calls } = makeFetch(() => undefined);
    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "not_supported" });
    expect(calls).toHaveLength(0);
  });

  it("无会话凭证：not_supported（推送端点需 Bearer 认证）", async () => {
    stubPushGlobals(makeServiceWorkerStub().container);
    const { fetch, calls } = makeFetch(() => undefined);
    const result = await enablePush(() => undefined, { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "not_supported" });
    expect(calls).toHaveLength(0);
  });

  it("iOS 未添加主屏（registration.pushManager 缺失）：not_supported", async () => {
    stubPushGlobals(makeServiceWorkerStub({ pushManager: undefined }).container);
    const { fetch, calls } = makeFetch(() => undefined);
    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "not_supported" });
    expect(calls).toHaveLength(0);
  });

  it("通知权限被拒：permission_denied，且不再发任何请求", async () => {
    stubPushGlobals(makeServiceWorkerStub().container);
    requestPermissionMock("denied");
    const { fetch, calls } = makeFetch(() => undefined);
    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "permission_denied" });
    expect(calls).toHaveLength(0);
  });

  it("pushManager.subscribe 抛错（如无效 VAPID）：subscribe_failed", async () => {
    const sw = makeServiceWorkerStub({
      pushManager: { subscribe: vi.fn(async () => { throw new Error("bad key"); }) },
    });
    stubPushGlobals(sw.container);
    const { fetch } = makeFetch((url) =>
      url === `${BASE}push/status`
        ? { status: 200, body: { vapidPublicKey: VAPID_PUBLIC_KEY } }
        : undefined,
    );
    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "subscribe_failed" });
  });

  it("服务端登记非 200：subscribe_failed", async () => {
    stubPushGlobals(makeServiceWorkerStub().container);
    const { fetch } = makeFetch((url) =>
      url === `${BASE}push/status`
        ? { status: 200, body: { vapidPublicKey: VAPID_PUBLIC_KEY } }
        : url === `${BASE}push/subscribe`
          ? { status: 500, body: { error: "internal" } }
          : undefined,
    );
    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "subscribe_failed" });
  });

  it("status 网络异常 / 缺 vapidPublicKey：network", async () => {
    stubPushGlobals(makeServiceWorkerStub().container);
    const boom = vi.fn(async () => {
      throw new TypeError("fetch failed");
    }) as unknown as typeof fetch;
    expect(await enablePush(() => "tok-1", { fetchImpl: boom, base: BASE })).toEqual({
      ok: false,
      code: "network",
    });

    const { fetch } = makeFetch((url) =>
      url === `${BASE}push/status` ? { status: 200, body: { supported: true } } : undefined,
    );
    expect(await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE })).toEqual({
      ok: false,
      code: "network",
    });
  });

  it("登记请求网络异常：network", async () => {
    stubPushGlobals(makeServiceWorkerStub().container);
    const { fetch } = makeFetch((url) => {
      if (url === `${BASE}push/status`) {
        return { status: 200, body: { vapidPublicKey: VAPID_PUBLIC_KEY } };
      }
      if (url === `${BASE}push/subscribe`) {
        throw new TypeError("fetch failed");
      }
      return undefined;
    });
    const result = await enablePush(() => "tok-1", { fetchImpl: fetch, base: BASE });
    expect(result).toEqual({ ok: false, code: "network" });
  });
});

// ---------------------------------------------------------------------------
// disablePush
// ---------------------------------------------------------------------------

describe("disablePush", () => {
  it("有订阅：POST unsubscribe（endpoint + Bearer）→ 本地 unsubscribe", async () => {
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    const { fetch, calls } = makeFetch((url) =>
      url === `${BASE}push/unsubscribe` ? { status: 200, body: {} } : undefined,
    );

    await disablePush("tok-1", { fetchImpl: fetch, base: BASE });

    const post = calls.find((c) => c.url === `${BASE}push/unsubscribe`);
    expect(post?.init?.method).toBe("POST");
    expect(JSON.parse(String(post?.init?.body))).toEqual({
      endpoint: "https://push.example/sub-1",
    });
    expect((post?.init?.headers as Record<string, string>).authorization).toBe("Bearer tok-1");
    expect(sw.subscription.unsubscribe).toHaveBeenCalledTimes(1);
  });

  it("无既有订阅：不发请求、不调 unsubscribe（幂等）", async () => {
    const sw = makeServiceWorkerStub({
      pushManager: { getSubscription: vi.fn(async () => null) },
    });
    stubPushGlobals(sw.container);
    const { fetch, calls } = makeFetch(() => undefined);

    await disablePush("tok-1", { fetchImpl: fetch, base: BASE });

    expect(calls).toHaveLength(0);
    expect((sw.pushManager as { getSubscription: ReturnType<typeof vi.fn> }).getSubscription).toHaveBeenCalled();
  });

  it("服务端退订失败：仍执行本地退订", async () => {
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    const { fetch } = makeFetch(() => {
      throw new TypeError("fetch failed");
    });

    await disablePush("tok-1", { fetchImpl: fetch, base: BASE });

    expect(sw.subscription.unsubscribe).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// open-conv 消息监听（notificationclick → App 定位会话）
// ---------------------------------------------------------------------------

describe("listenServiceWorkerMessages", () => {
  type Listener = (event: Event) => void;

  function makeEmitter() {
    const listeners = new Map<string, Set<Listener>>();
    return {
      addEventListener: (type: string, l: Listener) => {
        const set = listeners.get(type) ?? new Set<Listener>();
        set.add(l);
        listeners.set(type, set);
      },
      removeEventListener: (type: string, l: Listener) => {
        listeners.get(type)?.delete(l);
      },
      emit: (data: unknown) => {
        for (const l of listeners.get("message") ?? []) {
          l({ data } as unknown as Event);
        }
      },
      listenerCount: () => (listeners.get("message") ?? new Set()).size,
    };
  }

  it("open-conv 消息触发回调；其它类型 / 空 convId 忽略", () => {
    const emitter = makeEmitter();
    const seen: string[] = [];
    listenServiceWorkerMessages(emitter, (c) => seen.push(c));

    emitter.emit({ type: "open-conv", convId: "web:abc" });
    emitter.emit({ type: "other" });
    emitter.emit({ type: "open-conv", convId: "" });
    emitter.emit({ type: "open-conv" });
    emitter.emit("garbage");

    expect(seen).toEqual(["web:abc"]);
  });

  it("cleanup 后不再触发", () => {
    const emitter = makeEmitter();
    const seen: string[] = [];
    const cleanup = listenServiceWorkerMessages(emitter, (c) => seen.push(c));
    expect(emitter.listenerCount()).toBe(1);

    cleanup();
    expect(emitter.listenerCount()).toBe(0);
    emitter.emit({ type: "open-conv", convId: "web:abc" });
    expect(seen).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// ?conv= 深链参数校验
// ---------------------------------------------------------------------------

describe("validConvParam", () => {
  it("完整 conv 形态（web:/local: 前缀 + 非空名）接受并原样返回", () => {
    expect(validConvParam("web:default")).toBe("web:default");
    expect(validConvParam("web:s-1a2b")).toBe("web:s-1a2b");
    expect(validConvParam("web:我的会话")).toBe("web:我的会话");
    expect(validConvParam("local:terminal-1")).toBe("local:terminal-1");
  });

  // v1.42：窗口回信推送（peer_reply）的 convId 是 peer:<窗口名>——深链须能定位
  it("peer:<窗口名> 接受并原样返回（v1.42 窗口回信推送深链）", () => {
    expect(validConvParam("peer:imagent-bc")).toBe("peer:imagent-bc");
    expect(validConvParam("peer:my-window_01")).toBe("peer:my-window_01");
  });

  it("非法形态拒绝：null / 空串 / 裸名字 / 冒号后空 / 含空白 / 超长", () => {
    expect(validConvParam(null)).toBeNull();
    expect(validConvParam("")).toBeNull();
    expect(validConvParam("abc")).toBeNull();
    expect(validConvParam("web:")).toBeNull();
    expect(validConvParam("web:a b")).toBeNull();
    expect(validConvParam(`web:${"x".repeat(121)}`)).toBeNull();
  });

  it("peer 形态同受约束：裸窗口名 / 冒号后空 / 含空白 / 未知前缀仍拒绝", () => {
    expect(validConvParam("imagent-bc")).toBeNull(); // 裸名（无前缀）
    expect(validConvParam("peer")).toBeNull();
    expect(validConvParam("peer:")).toBeNull();
    expect(validConvParam("peer:a b")).toBeNull();
    expect(validConvParam("peers:win")).toBeNull(); // 前缀须整词匹配
    expect(validConvParam(`peer:${"x".repeat(121)}`)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// v1.37 ?machine= 深链参数校验 + open-conv 消息 machine 透传
// ---------------------------------------------------------------------------

describe("validMachineParam", () => {
  it("合法机器名（1..=32 位 [A-Za-z0-9_-]，relay 同规）原样返回", () => {
    expect(validMachineParam("home-mac")).toBe("home-mac");
    expect(validMachineParam("Work_Mac1")).toBe("Work_Mac1");
    expect(validMachineParam("a".repeat(32))).toBe("a".repeat(32));
  });

  it("非法拒绝：null / 空串 / 超长 / 空格 / 中文 / 点号", () => {
    expect(validMachineParam(null)).toBeNull();
    expect(validMachineParam("")).toBeNull();
    expect(validMachineParam("a".repeat(33))).toBeNull();
    expect(validMachineParam("bad mac")).toBeNull();
    expect(validMachineParam("机器一")).toBeNull();
    expect(validMachineParam("home.mac")).toBeNull();
  });
});

describe("open-conv 消息 machine 透传（v1.37）", () => {
  type Listener = (event: Event) => void;

  function makeEmitter() {
    const listeners = new Map<string, Set<Listener>>();
    return {
      addEventListener: (type: string, l: Listener) => {
        const set = listeners.get(type) ?? new Set<Listener>();
        set.add(l);
        listeners.set(type, set);
      },
      removeEventListener: (type: string, l: Listener) => {
        listeners.get(type)?.delete(l);
      },
      emit: (data: unknown) => {
        for (const l of listeners.get("message") ?? []) {
          l({ data } as unknown as Event);
        }
      },
    };
  }

  it("machine 随消息透传给回调；machine 非字符串/空串/缺席按未携带（undefined）", () => {
    const emitter = makeEmitter();
    const seen: { convId: string; machine?: string }[] = [];
    listenServiceWorkerMessages(emitter, (c, m) => seen.push({ convId: c, machine: m }));

    emitter.emit({ type: "open-conv", convId: "web:abc", machine: "work-mac" });
    emitter.emit({ type: "open-conv", convId: "web:abc", machine: "" });
    emitter.emit({ type: "open-conv", convId: "web:abc", machine: 7 });
    emitter.emit({ type: "open-conv", convId: "web:abc" });

    expect(seen.map((s) => s.machine)).toEqual(["work-mac", undefined, undefined, undefined]);
    expect(seen.map((s) => s.convId)).toEqual(["web:abc", "web:abc", "web:abc", "web:abc"]);
  });
});

// ---------------------------------------------------------------------------
// initServiceWorker 更新流驱动夹具（v1.39 自动重载 / v1.41.9 提示抑制共用）。
// 驱动方式：registration stub 的 addEventListener 是 vi.fn——从 mock.calls 取
// 捕获到的 updatefound 处理器，手工喂 installing worker 的 statechange；再从
// 容器上取捕获的 controllerchange 处理器模拟新 SW 接管。reload 为 jsdom 不可
// 改写属性——经 vi.stubGlobal("location", {...原值, reload}) 整体替换（用到的
// describe 自己 afterEach vi.unstubAllGlobals）。
// ---------------------------------------------------------------------------

/** installing worker stub：state 可变（驱动 installing → installed）；
 *  postMessage 记录页面发往 SW 的消息（钉住 v1.41.9 起更新链路不驱动 SW）。 */
type InstallingWorker = {
  state: string;
  addEventListener: ReturnType<typeof vi.fn>;
  postMessage: ReturnType<typeof vi.fn>;
};
type UpdateStub = {
  registration: {
    pushManager: PushManagerStub;
    installing: InstallingWorker | null;
    addEventListener: ReturnType<typeof vi.fn>;
  };
  container: {
    register: ReturnType<typeof vi.fn>;
    getRegistration: ReturnType<typeof vi.fn>;
    addEventListener: ReturnType<typeof vi.fn>;
    removeEventListener: ReturnType<typeof vi.fn>;
    controller: unknown;
  };
};

/** initServiceWorker 的依赖注入参数（v1.41.9：assetRefs / cacheStorage）。 */
type InitOpts = Parameters<typeof initServiceWorker>[1];

/** 可变 controller/installing 的 registration stub（基座面向 enablePush
 *  场景恒 null，就地放宽类型）。controller 非空 = 页面已被旧 SW 接管。 */
function makeUpdateStub(controller: unknown): UpdateStub {
  const sw = makeServiceWorkerStub();
  const registration = sw.registration as unknown as UpdateStub["registration"];
  const container = sw.container as unknown as UpdateStub["container"];
  container.controller = controller;
  return { registration, container };
}

/** 挂接线 + 微任务冲刷（register().then 后 updatefound 监听才挂上），
 *  返回提示回调、捕获的 updatefound 处理器与 cleanup。init 缺省为文件顶部
 *  import 的 initServiceWorker；换成 push 模块重求值后的实例，可测「模块
 *  加载时采集资源引用」的缺省路径（见 v1.41.9 缺省采集用例）。 */
async function initAndGetUpdatefound(
  sw: UpdateStub,
  opts: InitOpts = {},
  init: typeof initServiceWorker = initServiceWorker,
) {
  const onUpdateAvailable = vi.fn();
  const cleanup = init({ onUpdateAvailable }, opts);
  await new Promise((r) => setTimeout(r, 0));
  const updatefound = sw.registration.addEventListener.mock.calls.find(
    ([type]) => type === "updatefound",
  )?.[1] as (ev: Event) => void | undefined;
  expect(updatefound).toBeDefined();
  return {
    onUpdateAvailable,
    updatefound: updatefound as (ev: Event) => void,
    cleanup,
  };
}

/** 模拟新 worker 安装完成：updatefound → installing → installed。
 *  返回捕获的 statechange 处理器（供后续直接驱动状态）。 */
function installWorker(
  sw: UpdateStub,
  updatefound: (ev: Event) => void,
): { installing: InstallingWorker; statechange: (ev: Event) => void } {
  const installing: InstallingWorker = {
    state: "installing",
    addEventListener: vi.fn(),
    postMessage: vi.fn(),
  };
  sw.registration.installing = installing;
  updatefound(new Event("updatefound"));
  const statechange = installing.addEventListener.mock.calls.find(
    ([type]) => type === "statechange",
  )?.[1] as (ev: Event) => void | undefined;
  expect(statechange).toBeDefined();
  return { installing, statechange: statechange as (ev: Event) => void };
}

function controllerchangeHandler(sw: UpdateStub): (ev: Event) => void {
  const handler = sw.container.addEventListener.mock.calls.find(
    ([type]) => type === "controllerchange",
  )?.[1] as (ev: Event) => void | undefined;
  expect(handler).toBeDefined();
  return handler as (ev: Event) => void;
}

/** 冲刷全部微任务（宏任务边界）：v1.41.9 起更新提示前多一步异步判定，
 *  statechange 之后要等判定 promise 链（match → text → then）走完才能断言。 */
const flushAsync = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

describe("initServiceWorker 更新接管自动重载（v1.39）", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("更新接管（installed 且有旧 controller）后 controllerchange 触发 reload，重复事件仅一次", async () => {
    const sw = makeUpdateStub({ scriptURL: "/imagent/sw.js" }); // 旧 SW 在管 = 更新场景
    stubPushGlobals(sw.container);
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });

    const { onUpdateAvailable, updatefound } = await initAndGetUpdatefound(sw);
    const { installing, statechange } = installWorker(sw, updatefound);
    installing.state = "installed";
    statechange(new Event("statechange"));
    // v1.41.9 起提示前多一步异步判定（jsdom 无 caches、无 module script →
    // 无法判定 → 照常提示）：冲刷后再断言，断言含义不变
    await flushAsync();
    expect(onUpdateAvailable).toHaveBeenCalledTimes(1); // 提示弹出（toast 的推送源）

    const controllerchange = controllerchangeHandler(sw);
    controllerchange(new Event("controllerchange")); // 模拟新 SW 接管（他处触发的 controllerchange）
    controllerchange(new Event("controllerchange")); // 连环事件：一次性守卫只重载一次
    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("首装激活（页面加载本身触发的首次 controllerchange，controller 缺席）不重载", async () => {
    const sw = makeUpdateStub(null); // 页面尚无 controller = 首装场景
    stubPushGlobals(sw.container);
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });

    const { onUpdateAvailable, updatefound } = await initAndGetUpdatefound(sw);
    const { installing, statechange } = installWorker(sw, updatefound);
    installing.state = "installed";
    statechange(new Event("statechange"));
    // v1.41.9 起提示回调是异步的（判定之后才发生）：必须冲刷后再断言——
    // 否则「不提示」成了空断言，守卫回归（首装也进提示路径）时抓不到
    await flushAsync();
    expect(onUpdateAvailable).not.toHaveBeenCalled(); // 无旧 controller = 非更新，不提示

    // 首装 activate → clients.claim() 接管本页 → 页面加载本身的首次
    // controllerchange：守卫未置位，不得重载（防首装误刷）
    controllerchangeHandler(sw)(new Event("controllerchange"));
    expect(reload).not.toHaveBeenCalled();
  });

  it("重载落地的新页面被 claim 触发的 controllerchange 同样不重载（无检测置位）", async () => {
    const sw = makeUpdateStub(null);
    stubPushGlobals(sw.container);
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });

    // 点击重载后的新页面：任何 controllerchange 先于更新检测发生
    await initAndGetUpdatefound(sw);
    controllerchangeHandler(sw)(new Event("controllerchange"));
    expect(reload).not.toHaveBeenCalled();
  });

  it("cleanup 解绑 controllerchange 监听（挂载效应卸载后不再响应）", async () => {
    const sw = makeUpdateStub(null);
    stubPushGlobals(sw.container);
    const cleanup = initServiceWorker({});
    await new Promise((r) => setTimeout(r, 0));
    cleanup();
    expect(sw.container.removeEventListener).toHaveBeenCalledWith(
      "controllerchange",
      expect.any(Function),
    );
  });
});

// ---------------------------------------------------------------------------
// initServiceWorker 运行中更新检查（v1.41.5）：页面长开时浏览器只在页面加载
// 时自动检查 SW 更新——两个运行中触发源（SW_UPDATE_INTERVAL_MS = 10 分钟定时
// + visibilitychange 切回前台）主动 reg.update()，让既有 updatefound→提示→
// controllerchange 自动重载链路接管。驱动方式同上（register stub + 微任务
// 冲刷）；定时用 fake timers，可见性直接往 document 派发 visibilitychange
// （jsdom 默认 visibilityState 为 "visible"，hidden 场景经实例属性覆盖）。
// ---------------------------------------------------------------------------

describe("initServiceWorker 运行中更新检查（v1.41.5）", () => {
  /** 间隔参数（10 分钟，毫秒）——与 push.ts 的 SW_UPDATE_INTERVAL_MS 钉死。 */
  const INTERVAL_MS = 10 * 60 * 1000;
  type RegWithUpdate = { update: ReturnType<typeof vi.fn> };

  afterEach(() => {
    vi.useRealTimers();
    delete (document as unknown as { visibilityState?: unknown }).visibilityState;
  });

  it("定时触发：每 10 分钟调一次 reg.update()", async () => {
    vi.useFakeTimers();
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    initServiceWorker({});
    await vi.advanceTimersByTimeAsync(0); // 冲刷 register().then（updatefound/触发源挂上）
    const registration = sw.registration as unknown as RegWithUpdate;

    expect(registration.update).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(INTERVAL_MS);
    expect(registration.update).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(INTERVAL_MS);
    expect(registration.update).toHaveBeenCalledTimes(2);
  });

  it("切回前台（visibilitychange 且 visible）触发 update；hidden 不触发", async () => {
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    initServiceWorker({});
    await new Promise((r) => setTimeout(r, 0));
    const registration = sw.registration as unknown as RegWithUpdate;

    // jsdom 默认 visibilityState = "visible"
    document.dispatchEvent(new Event("visibilitychange"));
    expect(registration.update).toHaveBeenCalledTimes(1);

    // hidden：事件照常派发但不触发检查
    Object.defineProperty(document, "visibilityState", {
      value: "hidden",
      configurable: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    expect(registration.update).toHaveBeenCalledTimes(1);

    // 恢复 visible 再触发
    Object.defineProperty(document, "visibilityState", {
      value: "visible",
      configurable: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    expect(registration.update).toHaveBeenCalledTimes(2);
  });

  it("cleanup 清理定时与 visibilitychange 监听：之后不再触发 update", async () => {
    vi.useFakeTimers();
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    const cleanup = initServiceWorker({});
    await vi.advanceTimersByTimeAsync(0);
    const registration = sw.registration as unknown as RegWithUpdate;
    const removeSpy = vi.spyOn(document, "removeEventListener");

    await vi.advanceTimersByTimeAsync(INTERVAL_MS);
    expect(registration.update).toHaveBeenCalledTimes(1); // cleanup 前定时照常

    cleanup();
    expect(removeSpy).toHaveBeenCalledWith("visibilitychange", expect.any(Function));

    await vi.advanceTimersByTimeAsync(INTERVAL_MS * 2);
    document.dispatchEvent(new Event("visibilitychange"));
    expect(registration.update).toHaveBeenCalledTimes(1); // cleanup 后两源均不再触发
  });

  it("reg.update() reject 静默（离线/反代异常：无未处理 rejection）", async () => {
    vi.useFakeTimers();
    const sw = makeServiceWorkerStub();
    sw.registration.update = vi.fn(async () => {
      throw new Error("network down");
    });
    stubPushGlobals(sw.container);
    initServiceWorker({});
    await vi.advanceTimersByTimeAsync(0);
    const registration = sw.registration as unknown as RegWithUpdate;

    // 两个触发源都打在 reject 的 update 上——catch 吞掉（若有未处理 rejection
    // vitest 会判本用例失败）
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(INTERVAL_MS);
    expect(registration.update).toHaveBeenCalledTimes(2);
  });

  it("cleanup 先于 register 完成时不挂接触发源（StrictMode 卸载竞态防泄漏）", async () => {
    vi.useFakeTimers();
    const sw = makeServiceWorkerStub();
    stubPushGlobals(sw.container);
    const cleanup = initServiceWorker({});
    cleanup(); // register 尚未 resolve
    await vi.advanceTimersByTimeAsync(INTERVAL_MS * 3);
    const registration = sw.registration as unknown as RegWithUpdate;
    expect(registration.update).not.toHaveBeenCalled();
    document.dispatchEvent(new Event("visibilitychange"));
    expect(registration.update).not.toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// v1.41.9 pageIsCurrentShell：本页面是否已是「等待中新 SW 所属构建」的 app
// shell——把页面实际加载的脚本 / 样式表引用（index.html 属性原文）逐项与缓存
// 里的首页文档做子串比对。假 cacheStorage 只需 match（返回真 Response，
// text() 即 Promise<string>）。无法判定的每一种形态都必须是 false（调用方照常
// 提示），且不向上抛。
// ---------------------------------------------------------------------------

const NEW_JS = "/imagent/assets/index-new111.js";
const NEW_CSS = "/imagent/assets/index-new222.css";
/** 页面属「新构建」时，模块加载时从 index.html 采集到的资源引用。 */
const NEW_SHELL_REFS: readonly string[] = [NEW_JS, NEW_CSS];

/** 首页文档样本：形态对齐 Vite 产物 index.html（module script + 样式表 link，
 *  内容哈希文件名）。 */
const shellHtml = (js: string, css: string): string =>
  '<!doctype html>\n<html lang="zh-CN">\n  <head>\n' +
  `    <script type="module" crossorigin src="${js}"></script>\n` +
  `    <link rel="stylesheet" crossorigin href="${css}">\n` +
  '  </head>\n  <body><div id="root"></div></body>\n</html>\n';

/** 假 Cache Storage：match 对任意键返回给定的首页文档（match 可断言）。 */
function makeShellCache(html: string): ShellCacheStorage & {
  match: ReturnType<typeof vi.fn>;
} {
  const match = vi.fn(async () => new Response(html));
  return { match };
}

/** 手动落定的 deferred（让判定停在途中，驱动 cleanup 竞态）。 */
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

describe("pageIsCurrentShell（v1.41.9）", () => {
  it("缓存文档含全部引用（脚本 + 样式表）→ true", async () => {
    const cacheStorage = makeShellCache(shellHtml(NEW_JS, NEW_CSS));
    await expect(
      pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, cacheStorage }),
    ).resolves.toBe(true);
  });

  it("缓存文档的入口脚本哈希不同（页面是旧构建）→ false", async () => {
    const cacheStorage = makeShellCache(shellHtml("/imagent/assets/index-old333.js", NEW_CSS));
    await expect(
      pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, cacheStorage }),
    ).resolves.toBe(false);
  });

  it("样式表哈希不同、脚本相同（仅样式变化的发版也要提示）→ false", async () => {
    const cacheStorage = makeShellCache(shellHtml(NEW_JS, "/imagent/assets/index-old444.css"));
    await expect(
      pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, cacheStorage }),
    ).resolves.toBe(false);
  });

  it("assetRefs 为空 → false（无法判定），且不读缓存", async () => {
    const cacheStorage = makeShellCache(shellHtml(NEW_JS, NEW_CSS));
    await expect(pageIsCurrentShell({ assetRefs: [], cacheStorage })).resolves.toBe(false);
    expect(cacheStorage.match).not.toHaveBeenCalled();
  });

  it("cacheStorage 缺省且全局无 caches（jsdom）→ false（无法判定）", async () => {
    expect(typeof caches).toBe("undefined"); // 前提：jsdom 没有 Cache Storage
    await expect(pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS })).resolves.toBe(false);
  });

  it("match 同步抛错 / 异步 reject / 未命中（undefined）→ false，不向上抛", async () => {
    const syncThrow: ShellCacheStorage = {
      match: () => {
        throw new Error("TypeError: bad url");
      },
    };
    const asyncThrow: ShellCacheStorage = {
      match: async () => {
        throw new Error("QuotaExceededError");
      },
    };
    const missing: ShellCacheStorage = { match: async () => undefined };
    for (const cacheStorage of [syncThrow, asyncThrow, missing]) {
      await expect(
        pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, cacheStorage }),
      ).resolves.toBe(false);
    }
  });

  it("缓存文档 text() 抛错 → false，不向上抛", async () => {
    const unreadable: ShellCacheStorage = {
      match: async () => ({ text: async () => { throw new Error("boom"); } }) as unknown as Response,
    };
    await expect(
      pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, cacheStorage: unreadable }),
    ).resolves.toBe(false);
  });

  it('docUrl 缺省 → match 收到 "/imagent/"；显式传入时原样使用', async () => {
    const cacheStorage = makeShellCache(shellHtml(NEW_JS, NEW_CSS));
    await pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, cacheStorage });
    expect(cacheStorage.match).toHaveBeenCalledTimes(1);
    expect(cacheStorage.match).toHaveBeenCalledWith("/imagent/");
    // 带 query 的键：证明是不改写地透传
    await pageIsCurrentShell({ assetRefs: NEW_SHELL_REFS, docUrl: "/imagent/?v=2", cacheStorage });
    expect(cacheStorage.match).toHaveBeenLastCalledWith("/imagent/?v=2");
  });
});

// ---------------------------------------------------------------------------
// initServiceWorker 多余更新提示的抑制（v1.41.9）：updatefound →
// statechange(installed) 且页面已有旧 controller 时，先判本页面是否已是该新
// SW 所属构建——
// 是 → 不提示，也不驱动接管（不 postMessage）：页面继续由旧 SW 服务，等待中
//   的 SW 到下次冷启动自然激活，同一浏览器的其它标签页不受牵连；且因不置位
//   updateSeen，之后即便发生 controllerchange 也不 reload；
// 否（确属旧构建 / 无法判定）→ 既有提示 + controllerchange 自动重载腿原样
//   保留，同样不 postMessage（换不换 SW 由用户点提示刷新决定）。
// 驱动方式同 v1.39（共用上方夹具）；判定是异步的，statechange 后须冲刷。
// ---------------------------------------------------------------------------

describe("initServiceWorker 页面已是新构建时不提示（v1.41.9）", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  /** 挂接线（页面已被旧 SW 接管 = 更新场景而非首装）并驱动新 worker 的
   *  statechange：返回提示回调、installing worker（含 postMessage）、reload
   *  桩与 cleanup。此刻判定已在途、尚未冲刷（由调用方 await flushAsync）。
   *  state 缺省 "installed"；传 "installing" 模拟尚未装完的状态变化。 */
  async function mountInstalled(opts: InitOpts, state: "installed" | "installing" = "installed") {
    const sw = makeUpdateStub({ scriptURL: "/imagent/sw.js" });
    stubPushGlobals(sw.container);
    const reload = vi.fn();
    vi.stubGlobal("location", { ...window.location, reload });
    const { onUpdateAvailable, updatefound, cleanup } = await initAndGetUpdatefound(sw, opts);
    const { installing, statechange } = installWorker(sw, updatefound);
    installing.state = state;
    statechange(new Event("statechange"));
    return { sw, onUpdateAvailable, installing, reload, cleanup };
  }

  it("statechange 时 state 尚未到 installed → 不判定（不读缓存）、不提示、不 postMessage", async () => {
    // 「installed 时 install 的预缓存已落位」是判定的前提：没到 installed
    // 就去读缓存，读到的可能是上一个构建的文档
    const cacheStorage = makeShellCache(shellHtml(NEW_JS, NEW_CSS));
    const { onUpdateAvailable, installing } = await mountInstalled(
      { assetRefs: NEW_SHELL_REFS, cacheStorage },
      "installing",
    );
    await flushAsync();

    expect(cacheStorage.match).not.toHaveBeenCalled();
    expect(onUpdateAvailable).not.toHaveBeenCalled();
    expect(installing.postMessage).not.toHaveBeenCalled();
  });

  it("页面已是新版（缓存文档含本页面的全部资源引用）：不提示、不 postMessage（不驱动接管）；之后 controllerchange 也不 reload", async () => {
    const cacheStorage = makeShellCache(shellHtml(NEW_JS, NEW_CSS));
    const { sw, onUpdateAvailable, installing, reload } = await mountInstalled({
      assetRefs: NEW_SHELL_REFS,
      cacheStorage,
    });
    await flushAsync();

    // 读的是首页文档条目（缓存键缺省 "/imagent/"）
    expect(cacheStorage.match).toHaveBeenCalledWith("/imagent/");
    expect(onUpdateAvailable).not.toHaveBeenCalled();
    expect(installing.postMessage).not.toHaveBeenCalled(); // 不驱动接管

    // updateSeen 未置位：之后若发生 controllerchange（如别处触发接管）也不重载
    controllerchangeHandler(sw)(new Event("controllerchange"));
    expect(reload).not.toHaveBeenCalled();
  });

  it("页面是旧构建（缓存文档的入口脚本哈希不同）：提示恰好一次、不 postMessage；接管 reload 恰好一次", async () => {
    const cacheStorage = makeShellCache(shellHtml("/imagent/assets/index-old333.js", NEW_CSS));
    const { sw, onUpdateAvailable, installing, reload } = await mountInstalled({
      assetRefs: NEW_SHELL_REFS,
      cacheStorage,
    });
    await flushAsync();

    expect(onUpdateAvailable).toHaveBeenCalledTimes(1);
    expect(installing.postMessage).not.toHaveBeenCalled();

    // 既有自动重载腿保持：他处触发接管 → 本页重载
    controllerchangeHandler(sw)(new Event("controllerchange"));
    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("无法判定（不注入 cacheStorage，jsdom 无 caches）：照常提示恰好一次，不 postMessage", async () => {
    expect(typeof caches).toBe("undefined"); // 前提：jsdom 没有 Cache Storage
    const { onUpdateAvailable, installing } = await mountInstalled({
      assetRefs: NEW_SHELL_REFS,
    });
    await flushAsync();

    expect(onUpdateAvailable).toHaveBeenCalledTimes(1); // 宁可多提示，不能漏提示
    expect(installing.postMessage).not.toHaveBeenCalled();
  });

  // 缓存文档取旧构建——无 disposed 守卫时这条会弹提示。（新构建结局下 current
  // 分支本就没有副作用，守卫与否看不出差别，不作为用例。）
  it("判定完成前已 cleanup：不提示", async () => {
    const pending = deferred<Response | undefined>();
    const sw = makeUpdateStub({ scriptURL: "/imagent/sw.js" });
    stubPushGlobals(sw.container);
    const { onUpdateAvailable, updatefound, cleanup } = await initAndGetUpdatefound(sw, {
      assetRefs: NEW_SHELL_REFS,
      cacheStorage: { match: () => pending.promise },
    });
    const { installing, statechange } = installWorker(sw, updatefound);
    installing.state = "installed";
    statechange(new Event("statechange"));
    expect(sw.container.removeEventListener).not.toHaveBeenCalled(); // 判定在途

    cleanup(); // 判定完成前组件卸载
    pending.resolve(new Response(shellHtml("/imagent/assets/index-old333.js", NEW_CSS)));
    await flushAsync();

    expect(onUpdateAvailable).not.toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// v1.41.9 缺省路径——页面资源引用在**模块加载时**从 index.html 采集一次
// （readShellAssetRefs / SHELL_ASSET_REFS 均不导出）。上方用例都显式注入
// assetRefs，而 jsdom 的 DOM 为空（缺省值恒为 []，走不到判定），故这里经
// vi.resetModules() 让 push 模块带着预置的 DOM 重新求值，再走不注入 assetRefs
// 的 initServiceWorker 端到端验证。钉两点：
//  ① 采集到的是 index.html 写死的 module script（src）与样式表 link（href）；
//  ② 模块加载之后才出现的元素（Vite 动态 import 运行期注入的样式表）不算
//     ——否则缺省判定会与「页面已是当前构建」脱钩。
// ---------------------------------------------------------------------------

describe("initServiceWorker 缺省采集页面资源引用（v1.41.9）", () => {
  const SCRIPT_SRC = "/imagent/assets/index-aaa111.js";
  const CSS_HREF = "/imagent/assets/index-bbb222.css";
  const added: Element[] = [];

  /** 往 document.head 放一个 index.html 形态的静态元素（afterEach 移除）。 */
  function addShellElement(tag: "script" | "link", attrs: Record<string, string>): void {
    const el = document.createElement(tag);
    for (const [name, value] of Object.entries(attrs)) {
      el.setAttribute(name, value);
    }
    document.head.appendChild(el);
    added.push(el);
  }

  /** push 模块重求值：此刻的 DOM 就是模块加载时看到的。 */
  async function importFreshPush(): Promise<typeof import("./push")> {
    vi.resetModules();
    return import("./push");
  }

  /** 驱动到新 worker installed（页面已被旧 SW 接管）：init 用给定的模块
   *  实例，不注入 assetRefs（走模块加载时采集的缺省值）。 */
  async function mountFresh(mod: typeof import("./push"), opts: InitOpts) {
    const sw = makeUpdateStub({ scriptURL: "/imagent/sw.js" });
    stubPushGlobals(sw.container);
    const { onUpdateAvailable, updatefound } = await initAndGetUpdatefound(
      sw,
      opts,
      mod.initServiceWorker,
    );
    const { installing, statechange } = installWorker(sw, updatefound);
    installing.state = "installed";
    statechange(new Event("statechange"));
    return { onUpdateAvailable, installing };
  }

  afterEach(() => {
    for (const el of added.splice(0)) {
      el.remove();
    }
    vi.unstubAllGlobals();
  });

  it("模块加载时 index.html 的静态 script / link 已在 DOM → 缺省采集到，判定为「已是新构建」：不提示、不 postMessage", async () => {
    addShellElement("script", { type: "module", src: SCRIPT_SRC });
    addShellElement("link", { rel: "stylesheet", href: CSS_HREF });
    const cacheStorage = makeShellCache(shellHtml(SCRIPT_SRC, CSS_HREF));
    const { onUpdateAvailable, installing } = await mountFresh(await importFreshPush(), {
      cacheStorage,
    });
    await flushAsync();

    expect(onUpdateAvailable).not.toHaveBeenCalled();
    expect(installing.postMessage).not.toHaveBeenCalled();
  });

  it("缺省采集到的引用确实参与比对：缓存文档的入口脚本哈希不同 → 提示", async () => {
    addShellElement("script", { type: "module", src: SCRIPT_SRC });
    addShellElement("link", { rel: "stylesheet", href: CSS_HREF });
    const cacheStorage = makeShellCache(shellHtml("/imagent/assets/index-old333.js", CSS_HREF));
    const { onUpdateAvailable, installing } = await mountFresh(await importFreshPush(), {
      cacheStorage,
    });
    await flushAsync();

    expect(onUpdateAvailable).toHaveBeenCalledTimes(1);
    expect(installing.postMessage).not.toHaveBeenCalled();
  });

  it("样式表 link 同样采集并参与比对：缓存文档仅样式表哈希不同 → 提示", async () => {
    addShellElement("script", { type: "module", src: SCRIPT_SRC });
    addShellElement("link", { rel: "stylesheet", href: CSS_HREF });
    const cacheStorage = makeShellCache(shellHtml(SCRIPT_SRC, "/imagent/assets/index-old444.css"));
    const { onUpdateAvailable, installing } = await mountFresh(await importFreshPush(), {
      cacheStorage,
    });
    await flushAsync();

    expect(onUpdateAvailable).toHaveBeenCalledTimes(1);
    expect(installing.postMessage).not.toHaveBeenCalled();
  });

  it("非 module 的 script 与 icon link 不算 shell 引用：只有它们时引用为空、不读缓存、照常提示", async () => {
    // index.html 里另有 <link rel="icon" href=...>；普通脚本同理——都不是内容
    // 哈希的入口资源，混进来会让判定永远对不上
    addShellElement("script", { src: "/imagent/legacy.js" });
    addShellElement("link", { rel: "icon", href: "/imagent/icon.svg" });
    const cacheStorage = makeShellCache(
      shellHtml("/imagent/assets/index-old333.js", "/imagent/assets/index-old444.css"),
    );
    const { onUpdateAvailable, installing } = await mountFresh(await importFreshPush(), {
      cacheStorage,
    });
    await flushAsync();

    expect(cacheStorage.match).not.toHaveBeenCalled(); // 引用为空 = 无法判定
    expect(onUpdateAvailable).toHaveBeenCalledTimes(1);
    expect(installing.postMessage).not.toHaveBeenCalled();
  });

  it("模块加载之后才注入的样式表 link 不算（运行期动态 import 形态）→ 引用为空，照常提示", async () => {
    const fresh = await importFreshPush(); // 模块求值时 head 为空
    addShellElement("link", { rel: "stylesheet", href: CSS_HREF });
    const cacheStorage = makeShellCache(shellHtml(SCRIPT_SRC, CSS_HREF)); // 文档含它
    const { onUpdateAvailable, installing } = await mountFresh(fresh, { cacheStorage });
    await flushAsync();

    expect(cacheStorage.match).not.toHaveBeenCalled();
    expect(onUpdateAvailable).toHaveBeenCalledTimes(1);
    expect(installing.postMessage).not.toHaveBeenCalled();
  });
});
