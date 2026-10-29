// passkey（WebAuthn）客户端（v1.3）：登录页「使用通行钥匙」入口 + 设备管理
// 「添加通行钥匙」注册流程。HTTP 端点走同源 fetch（CSP connect-src 'self'
// 已覆盖），两阶段 ceremony：
//   begin → navigator.credentials.{get,create} → complete
// 响应里的 challenge/user.id/allowCredentials id 是 base64url 字符串，
// 浏览器 credentials API 要 ArrayBuffer——本模块做双向转换（凭证回传的
// ArrayBuffer 字段转回 base64url JSON，对齐 webauthn-rs 的 serde 形状）。
// 依赖注入（fetchImpl / credentials / baseUrl）全部可替换，vitest 免真浏览器。

import type { SessionInfo } from "./protocol";

/** base64url（无 padding）编码：ArrayBuffer/Uint8Array → string。 */
export function toB64Url(buf: ArrayBuffer | Uint8Array): string {
  const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  const TBL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i]!;
    const b1 = bytes[i + 1];
    const b2 = bytes[i + 2];
    out += TBL[b0 >> 2];
    out += TBL[((b0 & 0x03) << 4) | ((b1 ?? 0) >> 4)];
    if (b1 !== undefined) {
      out += TBL[((b1 & 0x0f) << 2) | ((b2 ?? 0) >> 6)];
    }
    if (b2 !== undefined) {
      out += TBL[b2 & 0x3f];
    }
  }
  return out;
}

/** base64url（无 padding，容忍 padding）解码：string → Uint8Array。 */
export function fromB64Url(s: string): Uint8Array {
  const TBL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  const clean = s.replace(/=+$/, "");
  const out: number[] = [];
  let bits = 0;
  let acc = 0;
  for (const ch of clean) {
    const v = TBL.indexOf(ch);
    if (v < 0) {
      throw new Error(`非法 base64url 字符：${ch}`);
    }
    acc = (acc << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 0xff);
    }
  }
  return new Uint8Array(out);
}

/** credentials API 的最小形状（可注入 mock；浏览器原生经 `defaultCredentials`
 *  桥接——options 字典由本模块构造，DOM 类型只在桥接处收窄）。 */
export interface CredentialsApi {
  get(opts: { publicKey: unknown }): Promise<unknown>;
  create(opts: { publicKey: unknown }): Promise<unknown>;
}

/** 浏览器原生 credentials API → CredentialsApi（松类型桥接；字典形状由
 *  toGetOptions / toCreateOptions 保证）。 */
function defaultCredentials(): CredentialsApi {
  return {
    get: (o) => navigator.credentials.get(o as CredentialRequestOptions),
    create: (o) => navigator.credentials.create(o as CredentialCreationOptions),
  };
}

/** begin 响应：{"beginId", "publicKey":{…}}。 */
interface BeginResponse {
  beginId: string;
  publicKey: Record<string, unknown>;
}

/** complete 响应载荷。 */
interface LoginCompleteResponse {
  session: { id: string; token: string; expiresAt: number };
}

/** 统一错误：code 对齐 relay 的 {"error":"<code>"}；浏览器异常给
 *  NotAllowedError 等原始名（UI 分流文案）。 */
export class PasskeyError extends Error {
  constructor(
    public readonly code: string,
    message?: string,
  ) {
    super(message ?? code);
    this.name = "PasskeyError";
  }
}

/** 浏览器是否支持 WebAuthn（不支持 → 登录页隐藏通行钥匙入口、直接令牌）。 */
export function webauthnSupported(): boolean {
  return typeof PublicKeyCredential !== "undefined" && !!navigator.credentials;
}

/** API 地址前缀：缺省 Vite BASE_URL（/imagent/，公网 Caddy strip / 本地代理
 *  重写后都是同源 /webauthn/*）。 */
function defaultBase(): string {
  return import.meta.env.BASE_URL;
}

async function postJson(
  base: string,
  path: string,
  body: unknown,
  fetchImpl: typeof fetch,
  bearer?: string,
): Promise<{ status: number; payload: Record<string, unknown> }> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (bearer) {
    headers.authorization = `Bearer ${bearer}`;
  }
  let resp: Response;
  try {
    resp = await fetchImpl(`${base}${path}`, {
      method: "POST",
      headers,
      body: JSON.stringify(body ?? {}),
    });
  } catch {
    throw new PasskeyError("network", "网络错误，请稍后重试");
  }
  let payload: Record<string, unknown> = {};
  try {
    payload = (await resp.json()) as Record<string, unknown>;
  } catch {
    // 非 JSON 体（如反代错误页）：按状态码归并
  }
  return { status: resp.status, payload };
}

/** HTTP 错误 → PasskeyError（relay 的 error code 透传）。 */
function httpError(status: number, payload: Record<string, unknown>): PasskeyError {
  const code = typeof payload.error === "string" ? payload.error : `http_${status}`;
  const message = typeof payload.message === "string" ? payload.message : undefined;
  return new PasskeyError(code, message);
}

// ---------------------------------------------------------------------------
// 登录（discoverable：allowCredentials 空，免选用户）
// ---------------------------------------------------------------------------

/** begin 响应 publicKey → navigator.credentials.get 的参数字典。 */
function toGetOptions(publicKey: Record<string, unknown>): { publicKey: Record<string, unknown> } {
  const pk: Record<string, unknown> = { ...publicKey };
  pk.challenge = fromB64Url(publicKey.challenge as string);
  const allow = publicKey.allowCredentials;
  if (Array.isArray(allow)) {
    pk.allowCredentials = allow.map((c) => {
      const rec = c as Record<string, unknown>;
      return { type: rec.type, id: fromB64Url(rec.id as string), transports: rec.transports };
    });
  }
  return { publicKey: pk };
}

/** 浏览器返回的 assertion → webauthn-rs PublicKeyCredential JSON 形状。 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function assertionToJson(cred: any): Record<string, unknown> {
  const r = cred.response;
  return {
    id: cred.id,
    rawId: toB64Url(r.rawId ?? cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: toB64Url(r.authenticatorData),
      clientDataJSON: toB64Url(r.clientDataJSON),
      signature: toB64Url(r.signature),
      userHandle: r.userHandle ? toB64Url(r.userHandle) : null,
    },
  };
}

/** passkey 登录：begin → credentials.get → complete → L2 会话（与 hello 换发
 *  同构）。成功返回 SessionInfo（调用方存 localStorage 进主界面）。 */
export async function passkeyLogin(opts: {
  fetchImpl?: typeof fetch;
  credentials?: CredentialsApi;
  base?: string;
} = {}): Promise<SessionInfo> {
  const fetchImpl = opts.fetchImpl ?? fetch;
  const credentials = opts.credentials ?? defaultCredentials();
  const base = opts.base ?? defaultBase();

  const begin = await postJson(base, "webauthn/login/begin", {}, fetchImpl);
  if (begin.status !== 200) {
    throw httpError(begin.status, begin.payload);
  }
  const { beginId, publicKey } = begin.payload as unknown as BeginResponse;

  let assertion: unknown;
  try {
    assertion = await credentials.get(toGetOptions(publicKey));
  } catch (e) {
    // NotAllowedError（用户取消/超时）/ 无可用凭证 → 统一 abort（UI 提示回落）
    throw new PasskeyError("aborted", e instanceof Error ? e.message : "认证器调用失败");
  }
  if (!assertion) {
    throw new PasskeyError("no_credential", "未找到可用通行钥匙");
  }

  const done = await postJson(
    base,
    "webauthn/login/complete",
    { beginId, credential: assertionToJson(assertion) },
    fetchImpl,
  );
  if (done.status !== 200) {
    throw httpError(done.status, done.payload);
  }
  const { session } = done.payload as unknown as LoginCompleteResponse;
  if (!session || typeof session.token !== "string") {
    throw new PasskeyError("bad_response", "登录响应异常");
  }
  return session;
}

// ---------------------------------------------------------------------------
// 注册（已登录会话经 Bearer sessionToken；label 缺省设备名由调用方传）
// ---------------------------------------------------------------------------

/** begin 响应 publicKey → navigator.credentials.create 的参数字典。 */
function toCreateOptions(publicKey: Record<string, unknown>): { publicKey: Record<string, unknown> } {
  const pk: Record<string, unknown> = { ...publicKey };
  pk.challenge = fromB64Url(publicKey.challenge as string);
  const user = publicKey.user as Record<string, unknown> | undefined;
  if (user) {
    pk.user = { ...user, id: fromB64Url(user.id as string) };
  }
  const exclude = publicKey.excludeCredentials;
  if (Array.isArray(exclude)) {
    pk.excludeCredentials = exclude.map((c) => {
      const rec = c as Record<string, unknown>;
      return { type: rec.type, id: fromB64Url(rec.id as string) };
    });
  }
  return { publicKey: pk };
}

/** 浏览器返回的注册凭证 → webauthn-rs RegisterPublicKeyCredential JSON。 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function attestationToJson(cred: any): Record<string, unknown> {
  const r = cred.response;
  const transports =
    typeof r.getTransports === "function" ? (r.getTransports() as string[]) : [];
  return {
    id: cred.id,
    rawId: toB64Url(r.rawId ?? cred.rawId),
    type: cred.type,
    response: {
      attestationObject: toB64Url(r.attestationObject),
      clientDataJSON: toB64Url(r.clientDataJSON),
      transports,
    },
  };
}

/** 注册结果（relay 回执的凭证视图）。 */
export interface RegisteredPasskey {
  id: string;
  label: string;
  created?: number;
}

/** passkey 注册：begin（Bearer sessionToken + label）→ credentials.create →
 *  complete。成功返回凭证视图（cred_list 随后会刷新）。 */
export async function passkeyRegister(
  sessionToken: string,
  label: string,
  opts: {
    fetchImpl?: typeof fetch;
    credentials?: CredentialsApi;
    base?: string;
  } = {},
): Promise<RegisteredPasskey> {
  const fetchImpl = opts.fetchImpl ?? fetch;
  const credentials = opts.credentials ?? defaultCredentials();
  const base = opts.base ?? defaultBase();

  const begin = await postJson(base, "webauthn/register/begin", { label }, fetchImpl, sessionToken);
  if (begin.status !== 200) {
    throw httpError(begin.status, begin.payload);
  }
  const { beginId, publicKey } = begin.payload as unknown as BeginResponse;

  let credential: unknown;
  try {
    credential = await credentials.create(toCreateOptions(publicKey));
  } catch (e) {
    throw new PasskeyError("aborted", e instanceof Error ? e.message : "认证器调用失败");
  }
  if (!credential) {
    throw new PasskeyError("no_credential", "认证器未返回凭证");
  }

  const done = await postJson(
    base,
    "webauthn/register/complete",
    { beginId, credential: attestationToJson(credential) },
    fetchImpl,
    sessionToken,
  );
  if (done.status !== 200) {
    throw httpError(done.status, done.payload);
  }
  const c = (done.payload.credential ?? {}) as Record<string, unknown>;
  if (typeof c.id !== "string" || typeof c.label !== "string") {
    throw new PasskeyError("bad_response", "注册响应异常");
  }
  return { id: c.id, label: c.label, created: c.created as number | undefined };
}
