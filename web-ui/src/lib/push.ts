// Web Push 订阅流（v1.10）：SPA 侧通知能力封装——SW 注册/更新提示、push
// 订阅（权限 → SW → VAPID → subscribe → HTTP 登记）与退订、通知点击深链
// 消息监听、?conv= 深链参数校验。HTTP 端点契约（中继，全部 Bearer
// sessionToken）：
//   GET  /imagent/push/status      → { supported, subscriptions, vapidPublicKey }
//   POST /imagent/push/subscribe   body { endpoint, keys:{p256dh, auth} }
//   POST /imagent/push/unsubscribe body { endpoint }
// 依赖注入（fetchImpl / base）可替换；Notification 与 navigator.serviceWorker
// 由测试以全局 stub 注入（参照 webauthn.test 的 stub 风格）。

import { fromB64Url } from "./webauthn";

/** SW 脚本地址（public/sw.js，base /imagent/ 固定部署路径）。 */
const SW_URL = "/imagent/sw.js";

/** localStorage 标志键：用户显式开启过通知（登录后静默重挂 / 登出清理的
 *  依据）。 */
export const PUSH_ENABLED_KEY = "imagent-push-enabled";

// ---------------------------------------------------------------------------
// 纯函数（单测对象）
// ---------------------------------------------------------------------------

/** 通知文案视图：title = 通知标题（kind 映射），body = 正文（网关给的
 *  ≤32 字摘要：终态卡 task / 审批工具名；缺摘要用 kind 兜底文案）。 */
export type NotificationCopy = { title: string; body: string };

/** push 载荷 kind → 通知文案。未知 kind / 空白 title 兜底。
 *  ⚠️ 与 public/sw.js 内的同名函数保持一致（SW 无法 import TS 模块，此为
 *  同步副本——改文案两处同改）。 */
export function notificationCopy(
  kind: string | undefined | null,
  title: string | undefined | null,
): NotificationCopy {
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

/** base64url（VAPID applicationServerKey）→ Uint8Array：与 webauthn 的
 *  fromB64Url 同一编码，别名导出维持 Web Push 生态惯用名（非法字符抛错，
 *  由 enablePush 的 try 归并到 subscribe_failed）。 */
export const urlB64ToUint8Array = fromB64Url;

/** ?conv= 深链参数校验：仅接受完整 conv 形态（`web:<名>` / `local:<名>`，
 *  对齐 protocol 的 conv 形态约定；裸名字不合法）。合法返回原值，否则 null。 */
export function validConvParam(conv: string | null): string | null {
  return conv !== null && /^(?:web|local):\S{1,120}$/.test(conv) ? conv : null;
}

// ---------------------------------------------------------------------------
// 能力检测
// ---------------------------------------------------------------------------

/** 浏览器是否具备 Web Push 基础能力（SW + PushManager + Notification）。
 *  iOS Safari 16.4+ 需 PWA 已添加主屏才有 PushManager——那一步在
 *  enablePush 里检测（registration.pushManager 缺失 → not_supported）。 */
export function pushSupported(): boolean {
  return (
    typeof navigator !== "undefined" &&
    "serviceWorker" in navigator &&
    typeof window !== "undefined" &&
    "PushManager" in window &&
    "Notification" in window
  );
}

/** SW 注册安全上下文门槛：仅 https 或 localhost（SW 硬性要求，http 部署
 *  形态下静默跳过注册）。 */
function secureContextOk(): boolean {
  return (
    location.protocol === "https:" ||
    location.hostname === "localhost" ||
    location.hostname === "127.0.0.1"
  );
}

// ---------------------------------------------------------------------------
// SW 生命周期：注册 + 更新提示 + 通知点击消息
// ---------------------------------------------------------------------------

/** SW 消息容器的最小结构（navigator.serviceWorker 结构兼容；测试注入 fake）。 */
interface SwMessageContainer {
  addEventListener(type: string, listener: (event: Event) => void): void;
  removeEventListener(type: string, listener: (event: Event) => void): void;
}

/** 监听 SW 的 open-conv 消息（notificationclick 聚焦已有窗口时 postMessage
 *  → App 内定位会话）。非 open-conv / 空 convId 忽略。返回取消监听 cleanup。 */
export function listenServiceWorkerMessages(
  container: SwMessageContainer,
  onOpenConv: (convId: string) => void,
): () => void {
  const handler = (event: Event): void => {
    const data = (event as MessageEvent).data as
      | { type?: unknown; convId?: unknown }
      | undefined;
    if (
      data &&
      typeof data === "object" &&
      data.type === "open-conv" &&
      typeof data.convId === "string" &&
      data.convId !== ""
    ) {
      onOpenConv(data.convId);
    }
  };
  container.addEventListener("message", handler);
  return () => {
    container.removeEventListener("message", handler);
  };
}

/** SW 注册与接线（App 挂载时调用一次）：
 *  - 仅 https / localhost 且浏览器支持时注册 /imagent/sw.js（幂等——已注册
 *    同 scope 脚本时复用既有 registration；失败静默，SW 只影响离线与推送）；
 *  - updatefound → 新 worker 进入 installed 且当前已有 controller 接管（即
 *    更新而非首装）→ onUpdateAvailable（App 顶部「新版本可用」提示，不自动
 *    刷新——刷新后等待中的新 SW 随旧页面卸载自然激活）；
 *  - open-conv 消息 → onOpenConv（通知点击定位会话）。
 *  返回消息监听的 cleanup（注册部分幂等，无需清理）。 */
export function initServiceWorker(
  handlers: {
    onUpdateAvailable?: () => void;
    onOpenConv?: (convId: string) => void;
  } = {},
): () => void {
  if (!pushSupported() || !secureContextOk()) {
    return () => {};
  }
  const cleanup = listenServiceWorkerMessages(navigator.serviceWorker, (convId) =>
    handlers.onOpenConv?.(convId),
  );
  void navigator.serviceWorker
    .register(SW_URL)
    .then((reg) => {
      reg.addEventListener("updatefound", () => {
        const installing = reg.installing;
        if (!installing) {
          return;
        }
        installing.addEventListener("statechange", () => {
          // controller 非空 = 页面此前已被旧 SW 接管，此刻 installed 是更新
          if (installing.state === "installed" && navigator.serviceWorker.controller) {
            handlers.onUpdateAvailable?.();
          }
        });
      });
    })
    .catch(() => {
      // 注册失败（无 SW 权限 / 反代异常等）：静默——主功能不受影响
    });
  return cleanup;
}

// ---------------------------------------------------------------------------
// 订阅流
// ---------------------------------------------------------------------------

/** enablePush 失败码（UI 按码分流文案）。 */
export type PushErrorCode =
  | "not_supported"
  | "permission_denied"
  | "subscribe_failed"
  | "network";

export type PushResult = { ok: true } | { ok: false; code: PushErrorCode };

const fail = (code: PushErrorCode): PushResult => ({ ok: false, code });

/** 开启推送全流程：注册 SW（幂等）→ 检测 pushManager（iOS 未添加主屏时为
 *  undefined → not_supported）→ 请求通知权限（denied → permission_denied）→
 *  GET push/status 拿 vapidPublicKey → pushManager.subscribe（userVisibleOnly
 *  + applicationServerKey）→ POST push/subscribe 登记（Bearer sessionToken，
 *  body 按契约：endpoint + keys.{p256dh, auth}——subscription.toJSON() 直取）。
 *  每步失败返回结构化错误码；HTTP 层异常归 network，业务失败归对应步骤码。 */
export async function enablePush(
  getToken: () => string | undefined,
  opts: { fetchImpl?: typeof fetch; base?: string } = {},
): Promise<PushResult> {
  const fetchImpl = opts.fetchImpl ?? fetch;
  const base = opts.base ?? import.meta.env.BASE_URL;

  if (!pushSupported() || typeof navigator.serviceWorker === "undefined") {
    return fail("not_supported");
  }
  const token = getToken();
  if (!token) {
    // 推送端点需会话认证（UI 已限会话模式可见；防御旧 relay 直连形态）
    return fail("not_supported");
  }

  let registration: ServiceWorkerRegistration;
  try {
    registration = await navigator.serviceWorker.register(SW_URL); // 已注册复用
  } catch {
    return fail("not_supported");
  }
  // iOS 未添加到主屏：registration.pushManager 为 undefined
  if (!registration.pushManager) {
    return fail("not_supported");
  }

  let permission: NotificationPermission;
  try {
    permission = await Notification.requestPermission();
  } catch {
    return fail("permission_denied");
  }
  if (permission !== "granted") {
    return fail("permission_denied");
  }

  let vapidPublicKey: string;
  try {
    const resp = await fetchImpl(`${base}push/status`, {
      headers: { authorization: `Bearer ${token}` },
    });
    if (!resp.ok) {
      return fail("network");
    }
    const payload = (await resp.json()) as { vapidPublicKey?: unknown };
    if (typeof payload.vapidPublicKey !== "string" || payload.vapidPublicKey === "") {
      return fail("network");
    }
    vapidPublicKey = payload.vapidPublicKey;
  } catch {
    return fail("network");
  }

  let subscription: PushSubscription;
  try {
    subscription = await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: urlB64ToUint8Array(vapidPublicKey),
    });
  } catch {
    return fail("subscribe_failed");
  }
  const json = subscription.toJSON();
  const keys = json.keys as { p256dh?: string; auth?: string } | undefined;
  if (!json.endpoint || !keys?.p256dh || !keys?.auth) {
    return fail("subscribe_failed");
  }

  try {
    const resp = await fetchImpl(`${base}push/subscribe`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({
        endpoint: json.endpoint,
        keys: { p256dh: keys.p256dh, auth: keys.auth },
      }),
    });
    if (!resp.ok) {
      return fail("subscribe_failed");
    }
  } catch {
    return fail("network");
  }
  return { ok: true };
}

/** 关闭推送：取既有本地订阅 → POST push/unsubscribe（服务端先退，绑定当前
 *  session）→ subscription.unsubscribe()（本地退订）。无订阅 / 能力缺失直接
 *  返回（幂等）；服务端退订失败不阻断本地清理（重复 endpoint 幂等覆盖）。 */
export async function disablePush(
  token: string,
  opts: { fetchImpl?: typeof fetch; base?: string } = {},
): Promise<void> {
  const fetchImpl = opts.fetchImpl ?? fetch;
  const base = opts.base ?? import.meta.env.BASE_URL;
  if (!pushSupported()) {
    return;
  }
  const registration = await navigator.serviceWorker.getRegistration();
  if (!registration || !registration.pushManager) {
    return;
  }
  const subscription = await registration.pushManager
    .getSubscription()
    .catch(() => null);
  if (!subscription) {
    return;
  }
  try {
    await fetchImpl(`${base}push/unsubscribe`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ endpoint: subscription.endpoint }),
    });
  } catch {
    // 服务端退订失败也继续本地退订
  }
  await subscription.unsubscribe().catch(() => {});
}
