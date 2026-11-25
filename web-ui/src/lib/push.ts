// Web Push 订阅流（v1.10）：SPA 侧通知能力封装——SW 注册/更新提示、push
// 订阅（权限 → SW → VAPID → subscribe → HTTP 登记）与退订、通知点击深链
// 消息监听、?conv= 深链参数校验。HTTP 端点契约（中继，全部 Bearer
// sessionToken）：
//   GET  /imagent/push/status      → { supported, subscriptions, vapidPublicKey }
//   POST /imagent/push/subscribe   body { endpoint, keys:{p256dh, auth} }
//   POST /imagent/push/unsubscribe body { endpoint }
// 依赖注入（fetchImpl / base）可替换；Notification 与 navigator.serviceWorker
// 由测试以全局 stub 注入（参照 webauthn.test 的 stub 风格）。

import { machineNameValid } from "./protocol";
import { fromB64Url } from "./webauthn";

/** SW 脚本地址（public/sw.js，base /imagent/ 固定部署路径）。 */
const SW_URL = "/imagent/sw.js";

/** 运行中 SW 更新检查间隔（v1.41.5）：浏览器只在页面加载时自动检查 SW 更新，
 *  页面长开（PWA 常驻）场景部署新版后 updatefound 永不触发——需运行中主动
 *  reg.update()。10 分钟折中「及时感知」与「检查频率」；切回前台另有即时
 *  触发（见 initServiceWorker）。 */
const SW_UPDATE_INTERVAL_MS = 10 * 60 * 1000;

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

/** ?machine= 深链参数校验（v1.37）：机器名规则与 relay 一致（1..=32 位
 *  [A-Za-z0-9_-]，protocol.MACHINE_NAME_RE 同规）。合法返回原值，否则 null
 *  （调用方忽略 machine 仍按 conv 深链走）。 */
export function validMachineParam(machine: string | null): string | null {
  return machine !== null && machineNameValid(machine) ? machine : null;
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
 *  → App 内定位会话）。非 open-conv / 空 convId 忽略。v1.37：消息可携带
 *  machine（推送来源机器，App 先切机器再切会话）；machine 缺席/非字符串按
 *  未携带处理。返回取消监听 cleanup。 */
export function listenServiceWorkerMessages(
  container: SwMessageContainer,
  onOpenConv: (convId: string, machine?: string) => void,
): () => void {
  const handler = (event: Event): void => {
    const data = (event as MessageEvent).data as
      | { type?: unknown; convId?: unknown; machine?: unknown }
      | undefined;
    if (
      data &&
      typeof data === "object" &&
      data.type === "open-conv" &&
      typeof data.convId === "string" &&
      data.convId !== ""
    ) {
      onOpenConv(
        data.convId,
        typeof data.machine === "string" && data.machine !== "" ? data.machine : undefined,
      );
    }
  };
  container.addEventListener("message", handler);
  return () => {
    container.removeEventListener("message", handler);
  };
}

/** Cache Storage 的最小读取面（窗口全局 caches 的子集；测试注入用）。 */
export interface ShellCacheStorage {
  match(request: string): Promise<Response | undefined>;
}

/** 本页面是否已是「等待中新 SW 所属构建」的 app shell（v1.41.9）。
 *  判定依据：带构建戳的 sw.js 在 install 时把当时线上的首页文档（与它同
 *  一次部署产出）写进固定名字的 cache（imagent-shell-v1 的 "/imagent/" 条目，
 *  见 sw.js 的 SHELL_CACHE 注释），且 worker 走到 installed 时 install 必已
 *  完成——缓存里的这份文档就是「新 SW 所属构建首页」的权威样本。index.html
 *  引用的脚本 / 样式表都是内容哈希文件名，应用代码或样式任何变化都会反映
 *  到这些引用上：页面实际加载的引用（assetRefs）逐项都是缓存文档的子串 →
 *  页面与该构建是同一版。逐字 includes 比对属性原文，不做 URL 解析（页面侧
 *  读的与文档里写的是同一份 index.html 的同一属性）。
 *  无法判定一律 false（调用方照常提示——宁可多提示，不能漏提示）：assetRefs
 *  为空、全局 caches 缺失（jsdom 等）、match 抛错 / 未命中、text() 抛错；
 *  整体不向上抛。 */
export async function pageIsCurrentShell(opts: {
  /** 本页面实际加载的静态资源引用（index.html 里写死的属性原文）。 */
  assetRefs: readonly string[];
  /** 首页文档缓存键，缺省 import.meta.env.BASE_URL（= "/imagent/"）。 */
  docUrl?: string;
  /** 缺省取全局 caches；不存在（jsdom 等）→ 无法判定。 */
  cacheStorage?: ShellCacheStorage;
}): Promise<boolean> {
  if (opts.assetRefs.length === 0) {
    return false;
  }
  try {
    const storage: ShellCacheStorage | undefined =
      opts.cacheStorage ?? (typeof caches === "undefined" ? undefined : caches);
    if (!storage) {
      return false;
    }
    // 不带 cache 名 = 查全部 cache（与 sw.js 离线回退的 caches.match 同一写法）
    const cached = await storage.match(opts.docUrl ?? import.meta.env.BASE_URL);
    if (!cached) {
      return false;
    }
    const html = await cached.text();
    // 每一项都必须在：任一引用（脚本或样式表）对不上即不是同一构建
    return opts.assetRefs.every((ref) => html.includes(ref));
  } catch {
    return false;
  }
}

/** 页面静态资源引用采集（v1.41.9）：index.html 写死的入口脚本（module script
 *  的 src）与样式表（link[rel=stylesheet] 的 href）的属性原文。
 *  必须在**模块加载时**读一次：此刻应用代码尚未运行，DOM 里只有 index.html
 *  的静态元素；Vite 动态 import 会在运行期注入 <link rel="stylesheet">，
 *  之后再读会混入——它们不是 shell 的组成，却会让 pageIsCurrentShell 判为
 *  「不是当前构建」（v1.41.9 的「已是新版则不提示」失效、退回多提示一次）。
 *  无 DOM → 空数组（= 无法判定）。 */
function readShellAssetRefs(): string[] {
  if (typeof document === "undefined") {
    return [];
  }
  const refs: string[] = [];
  for (const script of document.querySelectorAll('script[type="module"][src]')) {
    const src = script.getAttribute("src");
    if (src) {
      refs.push(src);
    }
  }
  for (const link of document.querySelectorAll('link[rel="stylesheet"][href]')) {
    const href = link.getAttribute("href");
    if (href) {
      refs.push(href);
    }
  }
  return refs;
}
const SHELL_ASSET_REFS: readonly string[] = readShellAssetRefs();

/** SW 注册与接线（App 挂载时调用一次）：
 *  - 仅 https / localhost 且浏览器支持时注册 /imagent/sw.js（幂等——已注册
 *    同 scope 脚本时复用既有 registration；失败静默，SW 只影响离线与推送）；
 *  - updatefound → 新 worker 进入 installed 且当前已有 controller 接管（即
 *    更新而非首装）→ v1.41.9 先判本页面是否已是该 SW 所属构建
 *    （pageIsCurrentShell）：是 → 不提示，也不驱动接管（等待中的 SW 到下次
 *    冷启动自然激活）；否则（确属旧构建 / 无法判定）→ 置位更新标记 +
 *    onUpdateAvailable（App 顶部「新版本已就绪，点击刷新」可点提示，v1.39）；
 *  - controllerchange → 新 SW 接管控制器（更新标记已置位时）→ reload 一次
 *    （v1.39 双保险自动腿，与 toast 点击重载并存，守卫见内注释）；
 *  - 运行中主动更新检查（v1.41.5）：定时（SW_UPDATE_INTERVAL_MS）+ 切回前台
 *    （visibilitychange → visible）触发 reg.update()，新 SW install 落入上方
 *    既有 updatefound 链路；update() 失败静默；
 *  - open-conv 消息 → onOpenConv（通知点击定位会话）。
 *  v1.41.9：页面资源引用 / Cache Storage 经 opts 注入（缺省取模块加载时
 *  采集的 SHELL_ASSET_REFS 与全局 caches；测试用）。
 *  返回 cleanup：解绑消息监听 / controllerchange / 运行中检查两触发源。 */
export function initServiceWorker(
  handlers: {
    onUpdateAvailable?: () => void;
    onOpenConv?: (convId: string, machine?: string) => void;
  } = {},
  opts: { assetRefs?: readonly string[]; cacheStorage?: ShellCacheStorage } = {},
): () => void {
  if (!pushSupported() || !secureContextOk()) {
    return () => {};
  }
  // init 时捕获容器引用：cleanup 一律用捕获引用解绑（卸载时重读全局——测试
  // 环境删 expando / 极端场景容器被换——会炸，捕获引用幂等且与时序无关）。
  const container = navigator.serviceWorker;
  const cleanupMsg = listenServiceWorkerMessages(container, (convId, machine) =>
    handlers.onOpenConv?.(convId, machine),
  );

  // v1.39 更新接管即自动重载（双保险之一）：守卫 updateSeen 只在「更新接管」
  // 检测时置位（下方 statechange：installing→installed 且页面已有旧 controller，
  // 与 onUpdateAvailable 同条件、同批先后）。页面加载本身触发的首次
  // controllerchange（首装 activate 里 clients.claim() 接管尚无 controller 的
  // 页面）与点击重载落地的新页面被新 SW claim 的 controllerchange 均无检测
  // 置位 → 不重载：跳过非更新路径，防首装误刷、防重载落地页连环重载。
  // reloading 一次性守卫：本页面生命周期内最多重载一次。
  let updateSeen = false;
  let reloading = false;
  const onControllerChange = (): void => {
    if (!updateSeen || reloading) {
      return;
    }
    reloading = true;
    window.location.reload();
  };
  container.addEventListener("controllerchange", onControllerChange);

  // v1.41.5 运行中更新检查（定时 + 切回前台两个触发源）：定时器 id 与监听器
  // 引用在注册完成后才产生——提前声明，cleanup 按需回收。卸载先于注册完成
  // （StrictMode dev 双挂载 / 快速切换）时置位 disposed，注册落地后不再挂接，
  // 防 interval 泄漏到已卸载页面对旧 handlers 继续触发。
  let disposed = false;
  let updateTimer: ReturnType<typeof setInterval> | undefined;
  let onVisibilityChange: (() => void) | undefined;
  // 捕获引用：挂接与解绑同源（对齐 container 的捕获纪律）
  const doc = document;

  void navigator.serviceWorker
    .register(SW_URL)
    .then((reg) => {
      reg.addEventListener("updatefound", () => {
        const installing = reg.installing;
        if (!installing) {
          return;
        }
        installing.addEventListener("statechange", () => {
          // controller 非空 = 页面此前已被旧 SW 接管，此刻 installed 是更新。
          // 这里刻意用 `=== null`、不用「逻辑非 + 变量名」的简写：tailwind 的
          // content 会扫描 src 下全部 .ts（注释里的字面量也算），叹号紧贴
          // container 会被当成 container 工具类的 important 变体，给产物 CSS
          // 多塞几条没人用的规则（v1.41.9 踩过——本注释也别把那个写法原样
          // 抄回来）。
          if (installing.state !== "installed" || container.controller === null) {
            return;
          }
          // v1.41.9——提示之前先判「本页面是不是已经是该新 SW 所属的构建」。
          // 背景：首页文档 network-first（v1.41.7）之后，发版后第一次冷启动的
          // 导航由旧 SW 处理、取到的已是新首页——页面跑的已是新版；紧接着
          // 浏览器因 sw.js 构建戳变化装上新 SW，落到这里无条件弹「新版本已
          // 就绪」，而点了刷新也不会有任何变化（页面与缓存里的文档同一版）。
          // 判定为真（依据见 pageIsCurrentShell）→ 不提示，也不驱动接管：
          //  - 不提示：页面与等待中新 SW 所属的构建是同一版，这次提示是多余的；
          //  - 不驱动接管（不发 skip-waiting）：①接管不是必需的——本页已是
          //    新版，旧 SW 继续在管不影响行为（v1.41.7 起带戳 SW 的缓存策略
          //    一致），等待中的 SW 会在全部窗口关闭后的下一次冷启动自然激活；
          //    ②skip-waiting 一发，同一浏览器的所有标签页同时换 controller，
          //    其中旧构建的长开标签页已走过下面的提示分支（updateSeen 已
          //    置位），接管一发生就会被自动重载、未发送的草稿丢失（真实
          //    Chromium 冒烟实证）。
          // 判定为假 / 无法判定 → 既有提示路径原样保留（置位 updateSeen +
          // onUpdateAvailable，controllerchange 自动重载腿随之保持）：宁可
          // 多提示一次，不能漏提示。
          void pageIsCurrentShell({
            assetRefs: opts.assetRefs ?? SHELL_ASSET_REFS,
            cacheStorage: opts.cacheStorage,
          }).then((current) => {
            if (disposed) {
              return; // 组件已卸载：不再回调
            }
            if (current) {
              // 本页面已是该 SW 所属构建：不提示，也不驱动接管（取舍见上）。
              // 不置位 updateSeen → 上方 onControllerChange 的守卫不会 reload。
              return;
            }
            updateSeen = true;
            handlers.onUpdateAvailable?.();
          });
        });
      });
      if (disposed) {
        return;
      }
      // v1.41.5 运行中主动触发 SW 更新检查：update() 让浏览器重新拉取 sw.js，
      // 新 SW install 后由上方既有 updatefound 链路接管（页面确属旧构建时
      // 提示 + controllerchange 自动重载；v1.41.9 起先判定，见上）。两个触发
      // 源的 update() 失败一律静默（离线 / 反代异常——主功能不受影响）。
      updateTimer = setInterval(() => {
        void reg.update().catch(() => {});
      }, SW_UPDATE_INTERVAL_MS);
      onVisibilityChange = () => {
        if (doc.visibilityState === "visible") {
          void reg.update().catch(() => {});
        }
      };
      doc.addEventListener("visibilitychange", onVisibilityChange);
    })
    .catch(() => {
      // 注册失败（无 SW 权限 / 反代异常等）：静默——主功能不受影响
    });
  return () => {
    disposed = true;
    if (updateTimer !== undefined) {
      clearInterval(updateTimer);
      updateTimer = undefined;
    }
    if (onVisibilityChange !== undefined) {
      doc.removeEventListener("visibilitychange", onVisibilityChange);
      onVisibilityChange = undefined;
    }
    cleanupMsg();
    container.removeEventListener("controllerchange", onControllerChange);
  };
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
