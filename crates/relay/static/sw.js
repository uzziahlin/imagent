/*
 * imagent Service Worker（v1.10 Web Push + shell 缓存）。手写原生 SW——public/
 * 下的文件不参与 Vite 构建处理，原样随构建拷贝到站点根。
 *
 * 缓存策略（契约）：只缓存静态 shell（/imagent/ 文档 + /imagent/icon.svg）
 * ——asset hash 文件交给浏览器 HTTP 缓存（relay 对 /assets/ 发 immutable 长
 * 缓存），业务 WS 流量与 API 绝不缓存（fetch handler 前缀黑名单直通网络）。
 *
 * 通知文案映射（notificationCopy）与 web-ui/src/lib/push.ts 保持一致——SW
 * 无法 import TS 模块，此处为同步副本；改文案请两处同改。
 */

const SHELL_CACHE = "imagent-shell-v1";
// install 预缓存的 shell（只此两项）
const SHELL_URLS = ["/imagent/", "/imagent/icon.svg"];
// 前缀命中即不拦截（WS 升级 / 网关代理 / 认证 / 推送登记 / 探活——业务流量）
const PASS_THROUGH_PREFIXES = [
  "/imagent/ws",
  "/imagent/gateway",
  "/imagent/webauthn",
  "/imagent/push",
  "/imagent/healthz",
];

/** push 载荷 → 通知文案（与 src/lib/push.ts 的 notificationCopy 同步副本）。 */
function notificationCopy(kind, title) {
  const digest = (title ?? "").trim();
  switch (kind) {
    case "round_done":
      return { title: "✅ 任务完成", body: digest || "任务已执行完成" };
    case "round_error":
      return { title: "⚠️ 任务出错", body: digest || "任务执行出错，请查看详情" };
    case "approval":
      return { title: "🔐 等待审批", body: digest || "有操作等待你的审批" };
    default:
      return { title: "imagent 通知", body: digest || "有新事件，点击查看" };
  }
}

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(SHELL_CACHE).then((cache) => cache.addAll(SHELL_URLS)),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      // 清理历史版本 cache（shell 缓存版本升级时旧名字整体淘汰）
      const keys = await caches.keys();
      await Promise.all(
        keys.filter((k) => k !== SHELL_CACHE).map((k) => caches.delete(k)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  // 非 GET（WS 升级、POST API）直接 pass
  if (event.request.method !== "GET") {
    return;
  }
  const url = new URL(event.request.url);
  // 非本应用路径：network
  if (url.origin !== self.location.origin || !url.pathname.startsWith("/imagent/")) {
    return;
  }
  // 业务/API 请求绝不缓存
  if (PASS_THROUGH_PREFIXES.some((p) => url.pathname.startsWith(p))) {
    return;
  }
  // /imagent/ 静态资源：cache-first（未命中回源且不回填——缓存内容恒为
  // install 时的 shell 两项；asset hash 文件实际由 HTTP 缓存承担）
  event.respondWith(
    caches.match(event.request).then((hit) => hit ?? fetch(event.request)),
  );
});

self.addEventListener("push", (event) => {
  let data = null;
  try {
    data = event.data ? event.data.json() : null;
  } catch {
    data = null; // 载荷损坏 / 非 JSON：兜底文案
  }
  const copy = notificationCopy(data?.kind, data?.title);
  event.waitUntil(
    self.registration.showNotification(copy.title, {
      body: copy.body,
      icon: "/imagent/icon.svg",
      // tag = convId 防重（同会话连发只留最新）；无 conv 归到全局 tag
      tag: data?.convId || "imagent",
      data: data ?? {},
      renotify: true, // 同 tag 重发也提醒（依赖上面的 tag）
    }),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const convId = event.notification.data?.convId;
  event.waitUntil(
    (async () => {
      // 已有应用窗口：聚焦第一个并请 App 内定位会话（open-conv 消息）；
      // 没有窗口则新开窗口，经 ?conv= 深链直达对应会话
      const clientList = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      for (const client of clientList) {
        await client.focus();
        if (convId) {
          client.postMessage({ type: "open-conv", convId });
        }
        return;
      }
      const target = convId
        ? `/imagent/?conv=${encodeURIComponent(convId)}`
        : "/imagent/";
      await self.clients.openWindow(target);
    })(),
  );
});

// 更新流的 SW 侧出口：新版本 SW 处于等待态时，页面可 postMessage
// {type:"skip-waiting"} 令其立即接管。当前 UI 只提示手动刷新，不自动调用。
self.addEventListener("message", (event) => {
  if (event.data && event.data.type === "skip-waiting") {
    self.skipWaiting();
  }
});
