// lib/push 单测（v1.10 Web Push）：mock fetch + 全局 stub（Notification /
// navigator.serviceWorker，参照 webauthn.test 的 stub 风格——jsdom 无这些
// API，经 expando 注入）。覆盖：notificationCopy 全 kind + 未知兜底、
// urlB64ToUint8Array 标准样例往返、enablePush 成功/各失败码（浏览器不支持 /
// iOS 未添加主屏 / 权限拒绝 / 订阅失败 / 服务端拒绝 / 网络错误 / 无会话）、
// disablePush（有订阅/无订阅/服务端失败仍本地退订）、open-conv 消息监听、
// ?conv= 深链参数校验。
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  listenServiceWorkerMessages,
  notificationCopy,
  pushSupported,
  urlB64ToUint8Array,
  validConvParam,
  enablePush,
  disablePush,
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
  const registration = { pushManager, installing: null, addEventListener: vi.fn() };
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

  it("非法形态拒绝：null / 空串 / 裸名字 / 冒号后空 / 含空白 / 超长", () => {
    expect(validConvParam(null)).toBeNull();
    expect(validConvParam("")).toBeNull();
    expect(validConvParam("abc")).toBeNull();
    expect(validConvParam("web:")).toBeNull();
    expect(validConvParam("web:a b")).toBeNull();
    expect(validConvParam(`web:${"x".repeat(121)}`)).toBeNull();
  });
});
