// public/sw.js 行为单测（v1.41.7）。sw.js 是原生脚本（顶层 self.addEventListener
// 注册、无导出、不经打包器）：源码经 ?raw 导入为字符串，用 new Function 在假
// 环境里执行——形参遮蔽 self / caches / fetch / URL，捕获注册到的 handler 后
// 手工驱动 fetch / install 事件。响应用真 Response（ok / clone 即浏览器语义）。
// 覆盖：导航 network-first（成功回填 / 深 query 同键 / 离线回退 / 非 ok 回退 /
// 回填失败不拖垮导航）、非导航 cache-first、pass-through 与非 GET 不拦截、
// install 预缓存且 cache 名不含构建戳（迁移约束）；v1.42 push 事件的通知文案
// （sw.js 内的 notificationCopy 与 src/lib/push.ts 是手工同步副本——逐 kind
// 比对两边输出）与 notificationclick 的 peer: 会话深链。
import { describe, expect, it, vi } from "vitest";
import swSource from "../../public/sw.js?raw";
import { notificationCopy, validConvParam, validMachineParam } from "./push";

const ORIGIN = "https://imagent.example";
/** 首页文档地址（shell 缓存键 = 其路径 "/imagent/"）。 */
const DOC = `${ORIGIN}/imagent/`;
const html = (body: string) => new Response(`<html>${body}</html>`, { status: 200 });

interface FakeRequest {
  method: string;
  mode: string;
  url: string;
}
interface FetchEvent {
  request: FakeRequest;
  respondWith: ReturnType<typeof vi.fn<(p: Promise<Response>) => void>>;
}
interface InstallEvent {
  waitUntil: ReturnType<typeof vi.fn<(p: Promise<unknown>) => void>>;
}
/** push 事件最小形状：data.json() 取载荷；data 为 null = 无载荷推送。 */
interface PushEvent {
  data: { json: () => unknown } | null;
  waitUntil: ReturnType<typeof vi.fn<(p: Promise<unknown>) => void>>;
}
/** showNotification 的选项对象（title/body/tag/data 等）。 */
type NotifyOptions = { body?: string; tag?: string; data?: unknown; [k: string]: unknown };
type FetchImpl = (req: FakeRequest) => Promise<Response>;

/** 假环境装载 sw.js。cached = Cache Storage 预置条目（键按 ORIGIN 解析成绝对
 *  URL——对齐浏览器把字符串键 "/imagent/" 解析为站点绝对地址）；fetchImpl 缺省
 *  一调用即抛（「命中缓存时不得发网络请求」类断言的硬失败形态）。open 返回的
 *  cache 记录 addAll / put 调用，put 同时真写入条目（后续 match 可见）。
 *  self.registration.showNotification / clients.matchAll / openWindow 记录调用
 *  （push 与 notificationclick handler 用）。 */
function loadSw(opts: { cached?: Record<string, Response>; fetchImpl?: FetchImpl } = {}) {
  const entries = new Map<string, Response>(
    Object.entries(opts.cached ?? {}).map(([k, v]) => [new URL(k, ORIGIN).href, v]),
  );
  const fetchImpl: FetchImpl =
    opts.fetchImpl ??
    (async () => {
      throw new Error("unexpected fetch");
    });
  const cache = {
    addAll: vi.fn(async (_urls: string[]) => undefined),
    put: vi.fn(async (req: string, res: Response) => {
      entries.set(new URL(req, ORIGIN).href, res);
    }),
  };
  const caches = {
    open: vi.fn(async (_name: string) => cache),
    // 真 Cache.match 的子集：接受字符串键或请求对象；缺省 ignoreSearch=false
    //（带 query 的请求不命中无 query 条目——与浏览器一致）
    match: vi.fn(async (req: string | { url: string }) => {
      const key = new URL(typeof req === "string" ? req : req.url, ORIGIN).href;
      return entries.get(key);
    }),
    keys: vi.fn(async () => [] as string[]),
    delete: vi.fn(async (_key: string) => false),
  };
  const handlers = new Map<string, (event: object) => void>();
  const registration = {
    showNotification: vi.fn(async (_title: string, _opts: NotifyOptions) => undefined),
  };
  const clients = {
    claim: vi.fn(async () => undefined),
    matchAll: vi.fn(async () => [] as unknown[]),
    openWindow: vi.fn(async (_url: string) => undefined),
  };
  const self = {
    location: { origin: ORIGIN },
    clients,
    registration,
    addEventListener: (type: string, fn: (event: object) => void) => {
      handlers.set(type, fn);
    },
  };
  const run = new Function("self", "caches", "fetch", "URL", swSource) as (
    self: unknown,
    caches: unknown,
    fetch: unknown,
    URL: unknown,
  ) => void;
  run(self, caches, fetchImpl, URL);
  const handler = <E extends object>(type: string): ((event: E) => void) => {
    const fn = handlers.get(type);
    if (!fn) {
      throw new Error(`sw.js 未注册 ${type} handler`);
    }
    return fn as (event: E) => void;
  };
  return {
    install: handler<InstallEvent>("install"),
    fetch: handler<FetchEvent>("fetch"),
    push: handler<PushEvent>("push"),
    notificationclick: handler<{ notification: { close: () => void; data?: unknown } }>(
      "notificationclick",
    ),
    cache,
    caches,
    registration,
    clients,
  };
}
type Sw = ReturnType<typeof loadSw>;

function fetchEvent(url: string, opts: { method?: string; mode?: string } = {}): FetchEvent {
  return {
    request: { method: opts.method ?? "GET", mode: opts.mode ?? "navigate", url },
    respondWith: vi.fn<(p: Promise<Response>) => void>(),
  };
}

/** 驱动 fetch handler，返回 respondWith 收到的 promise；未被拦截（pass-through
 *  时 respondWith 不被调用）→ undefined。 */
function dispatched(sw: Sw, event: FetchEvent): Promise<Response> | undefined {
  sw.fetch(event);
  return event.respondWith.mock.calls[0]?.[0];
}

// ---------------------------------------------------------------------------
// 导航请求（首页文档）network-first
// ---------------------------------------------------------------------------

describe("sw.js 导航 network-first（v1.41.7）", () => {
  it("导航 + 网络成功 → 返回网络响应，且缓存写入键 /imagent/", async () => {
    const fetchImpl = vi.fn<FetchImpl>(async () => html("new"));
    const sw = loadSw({ fetchImpl });
    const res = await dispatched(sw, fetchEvent(DOC));
    expect(await res?.text()).toBe("<html>new</html>");
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    // 回填：键固定 "/imagent/"，内容即网络响应的克隆
    expect(sw.caches.open).toHaveBeenCalledWith("imagent-shell-v1");
    expect(sw.cache.put).toHaveBeenCalledTimes(1);
    const [key, stored] = sw.cache.put.mock.calls[0];
    expect(key).toBe("/imagent/");
    expect(await stored.text()).toBe("<html>new</html>");
  });

  it("导航 + 带 query（/imagent/?conv=x）→ 回源用原请求，回填键仍是 /imagent/", async () => {
    const fetchImpl = vi.fn<FetchImpl>(async () => html("deep"));
    const sw = loadSw({ fetchImpl });
    const deep = `${DOC}?conv=web%3Aabc&machine=home-mac`;
    await dispatched(sw, fetchEvent(deep));
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    expect(fetchImpl.mock.calls[0][0]).toMatchObject({ url: deep, mode: "navigate" });
    expect(sw.cache.put.mock.calls[0][0]).toBe("/imagent/");
  });

  it("深链回填的与预缓存的是同一条目：之后离线打开 /imagent/ 拿到的即深链取回的新文档", async () => {
    // 旧 SW 时代预缓存的旧文档
    let online = true;
    const sw = loadSw({
      cached: { "/imagent/": html("old") },
      fetchImpl: async () => {
        if (!online) {
          throw new TypeError("Failed to fetch");
        }
        return html("new");
      },
    });
    // 在线深链导航：取回新文档并回填
    const onlineRes = await dispatched(sw, fetchEvent(`${DOC}?conv=web%3Aabc`));
    expect(await onlineRes?.text()).toBe("<html>new</html>");
    // 断网后导航 /imagent/：回退缓存——命中的已是新文档（不是那份旧的）
    online = false;
    const offlineRes = await dispatched(sw, fetchEvent(DOC));
    expect(await offlineRes?.text()).toBe("<html>new</html>");
  });

  it("导航 + 网络抛错 → 返回缓存文档（不回填）", async () => {
    const cached = html("cached");
    const fetchImpl = vi.fn<FetchImpl>(async () => {
      throw new TypeError("Failed to fetch");
    });
    const sw = loadSw({ fetchImpl, cached: { "/imagent/": cached } });
    const res = await dispatched(sw, fetchEvent(`${DOC}?conv=web%3Aabc`));
    expect(res).toBe(cached);
    expect(sw.cache.put).not.toHaveBeenCalled();
  });

  it("导航 + 网络抛错 + 缓存也没有 → 重新抛出原错误", async () => {
    const err = new TypeError("Failed to fetch");
    const sw = loadSw({
      fetchImpl: async () => {
        throw err;
      },
    });
    await expect(dispatched(sw, fetchEvent(DOC))).rejects.toBe(err);
  });

  it("导航 + 网络非 ok → 有缓存返回缓存 / 无缓存返回原响应（均不回填）", async () => {
    const badGateway = () => new Response("bad gateway", { status: 502 });
    // 有缓存：返回缓存的文档而不是 502
    const withCache = loadSw({
      fetchImpl: async () => badGateway(),
      cached: { "/imagent/": html("cached") },
    });
    const fromCache = await dispatched(withCache, fetchEvent(DOC));
    expect(fromCache?.status).toBe(200);
    expect(await fromCache?.text()).toBe("<html>cached</html>");
    // 无缓存：原样返回 502（不伪装成功、不抛错）
    const noCache = loadSw({ fetchImpl: async () => badGateway() });
    const raw = await dispatched(noCache, fetchEvent(DOC));
    expect(raw?.status).toBe(502);
    expect(raw?.ok).toBe(false);
    expect(withCache.cache.put).not.toHaveBeenCalled();
    expect(noCache.cache.put).not.toHaveBeenCalled();
  });

  it("回填失败（cache.put 拒绝）不拖垮导航：仍返回网络响应", async () => {
    const sw = loadSw({ fetchImpl: async () => html("ok") });
    sw.cache.put.mockRejectedValue(new Error("QuotaExceededError"));
    const res = await dispatched(sw, fetchEvent(DOC));
    expect(await res?.text()).toBe("<html>ok</html>");
  });
});

// ---------------------------------------------------------------------------
// 非导航请求：cache-first（现状不变）
// ---------------------------------------------------------------------------

describe("sw.js 非导航请求 cache-first", () => {
  it("非导航 /imagent/icon.svg → 命中缓存时不发网络请求", async () => {
    const icon = new Response("<svg/>", { headers: { "content-type": "image/svg+xml" } });
    const sw = loadSw({ cached: { "/imagent/icon.svg": icon } });
    const event = fetchEvent(`${ORIGIN}/imagent/icon.svg`, { mode: "no-cors" });
    expect(await dispatched(sw, event)).toBe(icon);
    expect(sw.caches.match).toHaveBeenCalledWith(event.request);
    expect(sw.cache.put).not.toHaveBeenCalled();
  });

  it("非导航未命中 → 回源且不回填", async () => {
    const fetched = new Response("<svg/>");
    const sw = loadSw({ fetchImpl: async () => fetched });
    const res = await dispatched(sw, fetchEvent(`${ORIGIN}/imagent/icon.svg`, { mode: "no-cors" }));
    expect(res).toBe(fetched);
    expect(sw.cache.put).not.toHaveBeenCalled();
  });

  // 导航分支只认路径恰为 /imagent/：其余路径即便以导航方式请求（用户直接在
  // 地址栏打开）也保持 cache-first——否则会把 JS/JSON 响应写进 /imagent/ 这个
  // shell 条目，之后被当成首页文档吐出来。
  it.each(["/imagent/assets/index-abc123.js", "/imagent/manifest.json", "/imagent/icon.svg"])(
    "导航到非首页文档路径 %s → 仍 cache-first，不写 /imagent/ 条目",
    async (path) => {
      const fetched = new Response("not-a-document");
      const fetchImpl = vi.fn<FetchImpl>(async () => fetched);
      const sw = loadSw({ fetchImpl });
      const res = await dispatched(sw, fetchEvent(`${ORIGIN}${path}`));
      expect(res).toBe(fetched);
      expect(fetchImpl).toHaveBeenCalledTimes(1);
      expect(sw.cache.put).not.toHaveBeenCalled();
      expect(sw.caches.open).not.toHaveBeenCalled();
    },
  );
});

// ---------------------------------------------------------------------------
// 不拦截的请求：pass-through 前缀 / 非 GET / 非本应用路径
// ---------------------------------------------------------------------------

describe("sw.js fetch handler 不拦截的请求", () => {
  it("pass-through 前缀（WS / 网关 / 认证 / 推送 / 探活）与非 GET → 不调用 respondWith", () => {
    const sw = loadSw();
    const passThrough = [
      "/imagent/push/status",
      "/imagent/push/subscribe",
      "/imagent/ws",
      "/imagent/gateway",
      "/imagent/webauthn/login/begin",
      "/imagent/healthz",
    ];
    for (const path of passThrough) {
      const event = fetchEvent(`${ORIGIN}${path}`);
      sw.fetch(event);
      expect(event.respondWith, path).not.toHaveBeenCalled();
    }
    const post = fetchEvent(DOC, { method: "POST" });
    sw.fetch(post);
    expect(post.respondWith).not.toHaveBeenCalled();
  });

  it("非本应用路径（跨源 / 不带 /imagent/ 前缀）→ 不调用 respondWith", () => {
    const sw = loadSw();
    for (const url of ["https://other.example/imagent/", `${ORIGIN}/favicon.ico`, `${ORIGIN}/imagent`]) {
      const event = fetchEvent(url);
      sw.fetch(event);
      expect(event.respondWith, url).not.toHaveBeenCalled();
    }
  });
});

// ---------------------------------------------------------------------------
// install 预缓存（cache 名迁移约束）
// ---------------------------------------------------------------------------

describe("sw.js install 预缓存", () => {
  // 钉死 cache 名不含构建戳（v1.41.7 迁移约束）：线上旧 SW 是 cache-first 且
  // cache 名同为 imagent-shell-v1；新 SW 的 install 必须把新文档覆写进**同一个**
  // cache——用户点「点击刷新」时导航仍由旧 SW 处理，其 cache-first 命中的才
  // 已是新文档。若把戳拼进 cache 名，旧 SW 会继续吐旧文档，迁移失败。
  it("install → caches.open(\"imagent-shell-v1\") + addAll(SHELL_URLS)；cache 名不含构建戳", async () => {
    const sw = loadSw();
    const event: InstallEvent = { waitUntil: vi.fn<(p: Promise<unknown>) => void>() };
    sw.install(event);
    await event.waitUntil.mock.calls[0][0];
    expect(sw.caches.open).toHaveBeenCalledTimes(1);
    expect(sw.caches.open.mock.calls[0][0]).toBe("imagent-shell-v1");
    expect(sw.cache.addAll).toHaveBeenCalledTimes(1);
    expect(sw.cache.addAll.mock.calls[0][0]).toEqual(["/imagent/", "/imagent/icon.svg"]);

    // 源码层钉死：剥掉注释后 BUILD 只出现在声明处（没有被拼进任何字符串 /
    // 模板），SHELL_CACHE 是不含戳的字面量
    const code = swSource.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/.*$/gm, "");
    expect(code.match(/\bBUILD\b/g)).toEqual(["BUILD"]);
    expect(code).not.toContain("${BUILD}");
    expect(code).toMatch(/^const SHELL_CACHE = "imagent-shell-v1";$/m);
  });
});

// ---------------------------------------------------------------------------
// v1.42 push / notificationclick：sw.js 内的通知文案是 src/lib/push.ts 的手工
// 同步副本（SW 无法 import TS 模块）——逐 kind 比对两边输出，防两份改岔；
// peer_reply 的字面文案另钉一份（只有一致性比对的话，两份一起写错仍是绿的）。
// ---------------------------------------------------------------------------

/** 驱动 push handler（载荷经 data.json() 交给 SW，同真实 PushEvent），返回
 *  **本次** showNotification 收到的 [title, options]（同一假环境可连推多条）；
 *  未调用 → undefined。 */
function shownNotification(sw: Sw, data: unknown): [string, NotifyOptions] | undefined {
  const event: PushEvent = { data: { json: () => data }, waitUntil: vi.fn() };
  sw.push(event);
  expect(event.waitUntil).toHaveBeenCalledTimes(1);
  return sw.registration.showNotification.mock.calls.at(-1) as [string, NotifyOptions] | undefined;
}

describe("sw.js 通知文案与 push.ts 同步（notificationCopy 两份副本一致）", () => {
  /** (kind, title) 样本：全部已知 kind + 未知 kind，各取「有摘要 / 空摘要」两形态。 */
  const samples: Array<[string | undefined | null, string | undefined | null]> = [
    ["round_done", "跑测试套件"],
    ["round_done", undefined],
    ["round_error", "npm install 失败"],
    ["round_error", "   "],
    ["approval", "bash"],
    ["approval", ""],
    ["peer_reply", "已修好"],
    ["peer_reply", ""],
    ["peer_reply", undefined],
    ["peer_reply", " \n "],
    ["peer_reply", "窗口回信的摘要可以很长很长很长很长很长很长很长很长很长很长"],
    ["unknown_kind", "摘要"],
    [undefined, undefined],
    [null, null],
  ];

  it.each(samples)("kind=%j title=%j：SW 侧 showNotification 的标题/正文与 push.ts 一致", (kind, title) => {
    const sw = loadSw();
    const shown = shownNotification(sw, { kind, title, convId: "web:default", machine: "home-mac" });
    expect(shown).toBeDefined();
    const expected = notificationCopy(kind, title);
    expect(shown![0]).toBe(expected.title);
    expect(shown![1].body).toBe(expected.body);
  });

  it("peer_reply 字面文案：标题「💬 窗口回复」、正文取回信摘要；空摘要走兜底", () => {
    const sw = loadSw();
    expect(shownNotification(sw, { kind: "peer_reply", title: "已修好" })![0]).toBe("💬 窗口回复");
    expect(shownNotification(sw, { kind: "peer_reply", title: "已修好" })![1].body).toBe("已修好");
    sw.registration.showNotification.mockClear();
    const fallback = shownNotification(sw, { kind: "peer_reply", title: "", convId: "peer:imagent-bc" });
    expect(fallback).toEqual([
      "💬 窗口回复",
      expect.objectContaining({ body: "本地窗口有了新回复" }),
    ]);
  });

  it("通知选项：tag = convId（同会话连发只留最新；无 convId 归全局 tag），data 原样进通知供点击深链", () => {
    const sw = loadSw();
    const payload = { kind: "peer_reply", title: "已修好", convId: "peer:imagent-bc", machine: "home-mac" };
    const [, opts] = shownNotification(sw, payload)!;
    expect(opts).toMatchObject({ tag: "peer:imagent-bc", data: payload, icon: "/imagent/icon.svg", renotify: true });
    const [, noConv] = shownNotification(sw, { kind: "round_done", title: "完成" })!;
    expect(noConv.tag).toBe("imagent");
  });

  it("无载荷 / 载荷损坏（非 JSON）：兜底文案不炸", () => {
    const sw = loadSw();
    const none: PushEvent = { data: null, waitUntil: vi.fn() };
    sw.push(none);
    expect(sw.registration.showNotification.mock.calls[0][0]).toBe("imagent 通知");
    const broken: PushEvent = {
      data: {
        json: () => {
          throw new SyntaxError("Unexpected token");
        },
      },
      waitUntil: vi.fn(),
    };
    sw.push(broken);
    expect(sw.registration.showNotification.mock.calls[1][1]).toMatchObject({
      body: "有新事件，点击查看",
    });
  });
});

describe("sw.js notificationclick 深链（v1.42 接受 peer: 会话）", () => {
  /** 驱动 notificationclick；返回 waitUntil 接到的 promise（openWindow 在其中）。 */
  function click(sw: Sw, data: unknown): Promise<unknown> {
    const close = vi.fn();
    const event = { notification: { close, data }, waitUntil: vi.fn() };
    sw.notificationclick(event);
    expect(close).toHaveBeenCalledTimes(1);
    const p = event.waitUntil.mock.calls[0][0] as Promise<unknown>;
    return p;
  }

  it("无已开窗口 + peer 会话推送 → openWindow 深链带 conv（URL 编码）与 machine；该 conv 过得了 push.ts 的深链校验", async () => {
    const sw = loadSw();
    sw.clients.matchAll.mockResolvedValue([]);
    await click(sw, { kind: "peer_reply", title: "已修好", convId: "peer:imagent-bc", machine: "home-mac" });
    expect(sw.clients.openWindow).toHaveBeenCalledTimes(1);
    const target = sw.clients.openWindow.mock.calls[0][0] as string;
    // 冒号经 encodeURIComponent 编码（query 里的裸冒号虽合法，编码形态才是稳定契约）
    expect(target).toBe("/imagent/?conv=peer%3Aimagent-bc&machine=home-mac");
    // 深链落地的 App 侧用 push.ts validConvParam 校验 ?conv=——v1.42 起接受 peer 前缀
    const params = new URL(target, ORIGIN).searchParams;
    expect(validConvParam(params.get("conv"))).toBe("peer:imagent-bc");
    expect(validMachineParam(params.get("machine"))).toBe("home-mac");
  });

  it("已有应用窗口：聚焦并向其 postMessage open-conv（convId / machine 透传），不开新窗口", async () => {
    const sw = loadSw();
    const client = { focus: vi.fn(async () => undefined), postMessage: vi.fn() };
    sw.clients.matchAll.mockResolvedValue([client]);
    await click(sw, { kind: "peer_reply", title: "已修好", convId: "peer:imagent-bc", machine: "home-mac" });
    expect(client.focus).toHaveBeenCalledTimes(1);
    expect(client.postMessage).toHaveBeenCalledWith({
      type: "open-conv",
      convId: "peer:imagent-bc",
      machine: "home-mac",
    });
    expect(sw.clients.openWindow).not.toHaveBeenCalled();
  });

  it("回归：无 machine 的推送深链不带 machine 参数；无 convId 回首页", async () => {
    const sw = loadSw();
    sw.clients.matchAll.mockResolvedValue([]);
    await click(sw, { kind: "round_done", title: "完成", convId: "web:default" });
    expect(sw.clients.openWindow.mock.calls[0][0]).toBe("/imagent/?conv=web%3Adefault");
    await click(sw, null);
    expect(sw.clients.openWindow.mock.calls[1][0]).toBe("/imagent/");
  });
});
