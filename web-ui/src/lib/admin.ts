// adminToken 内存态管理（v1.5 管理门槛）：
// - re-auth（passkeyReauth）产物 {adminToken, expiresAt(unix秒)}，5 分钟有效；
// - 只存模块级内存变量，**绝不落 localStorage**——刷新页面即失，管理操作
//   重新走一次 passkey 验证（安全默认：敏感管理面每次会话最多 5 分钟免验）；
// - ensureAdminToken：无有效令牌自动发起 re-auth，并发调用共享同一次
//   ceremony（多个管理操作同时触发不会弹多次系统验证窗）；
// - relay 回 error{admin_auth_required}（本地时钟偏移导致的提前失效）时，
//   调用方应 clearAdminToken() 后重试——ensureAdminToken 会重新验证。

import { normalizeExpiryMs } from "./protocol";
import { PasskeyError, passkeyReauth } from "./webauthn";

/** 内存态 grant（expiresAt 统一 ms epoch）。 */
type Grant = { token: string; expiresAtMs: number };

/** 模块级单例：内存唯一持有处（不落盘；登出/认证失败由 App 调 clear）。 */
let grant: Grant | null = null;

/** 进行中的 re-auth（并发共享：同窗口多操作只触发一次系统验证弹窗）。 */
let inflight: Promise<string> | null = null;

/** 存入 grant（expiresAt 秒级 unix 经 normalizeExpiryMs 容错放大）。 */
export function setAdminToken(adminToken: string, expiresAtSec: number): Grant {
  grant = { token: adminToken, expiresAtMs: normalizeExpiryMs(expiresAtSec) };
  return grant;
}

/** 当前有效 adminToken；过期/缺失返回 null。now 可注入（单测）。 */
export function getAdminToken(now: number = Date.now()): string | null {
  return grant !== null && now < grant.expiresAtMs ? grant.token : null;
}

/** 当前 grant 的过期时刻（ms epoch）；无 grant 返回 null（管理页剩余免验时间展示）。 */
export function adminTokenExpiresAt(): number | null {
  return grant?.expiresAtMs ?? null;
}

/** 作废当前 grant（登出 / 认证失败 / relay 回 admin_auth_required）。 */
export function clearAdminToken(): void {
  grant = null;
}

/** 供测试读取内部状态复位（生产代码不使用）。 */
export function __resetAdminToken(): void {
  grant = null;
  inflight = null;
}

/** 取有效 adminToken；无/过期/失败自动发起 passkey re-auth。并发调用共享同
 *  一次 ceremony。失败：清 inflight 并原样抛出（PasskeyError 等，调用方给
 *  UI 分流文案）。 */
export async function ensureAdminToken(
  sessionToken: string,
  opts?: Parameters<typeof passkeyReauth>[1],
): Promise<string> {
  const existing = getAdminToken();
  if (existing !== null) {
    return existing;
  }
  if (inflight !== null) {
    return inflight;
  }
  inflight = (async () => {
    try {
      const g = await passkeyReauth(sessionToken, opts);
      setAdminToken(g.adminToken, g.expiresAt);
      return g.adminToken;
    } finally {
      inflight = null;
    }
  })();
  return inflight;
}

/** re-auth 失败文案（管理面板 / 配对卡共用；与登录失败文案分流口径一致）。 */
export function reauthErrorText(e: unknown): string {
  if (e instanceof PasskeyError) {
    switch (e.code) {
      case "no_credentials":
        return "尚未注册通行钥匙，请先在支持设备上注册";
      case "no_credential":
        return "未找到可用通行钥匙，请先注册";
      case "aborted":
        return "已取消管理验证";
      case "rate_limited":
        return "验证过于频繁，请稍后再试";
      case "network":
        return "网络错误，请稍后重试";
      case "verification_failed":
        return "通行钥匙验证失败";
      default:
        return `管理验证失败（${e.code}）`;
    }
  }
  return "管理验证失败";
}
