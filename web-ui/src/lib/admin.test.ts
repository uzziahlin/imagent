// lib/admin 单测（v1.5 adminToken 内存态）：有效期/清空/并发共享 ceremony/
// 过期后重验证/失败重试。fetch + credentials 全注入（与 webauthn.test 同构的
// 最小桩——reauth 成功路径只需要 begin/complete 两段回包）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  __resetAdminToken,
  adminTokenExpiresAt,
  clearAdminToken,
  ensureAdminToken,
  getAdminToken,
  reauthErrorText,
  setAdminToken,
} from "./admin";
import { PasskeyError, type CredentialsApi } from "./webauthn";

const BASE = "/imagent/";
const ADMIN_TOKEN = "at".repeat(32);

/** reauth begin/complete 两段桩；completeFail 非空则 complete 回失败。 */
function reauthStub(opts?: { beginStatus?: number; completeStatus?: number }) {
  const calls: string[] = [];
  const fetchMock = vi.fn(async (url: string | URL | Request) => {
    const u = typeof url === "string" ? url : url.toString();
    calls.push(u);
    if (u.endsWith("webauthn/reauth/begin")) {
      const status = opts?.beginStatus ?? 200;
      return new Response(
        JSON.stringify(
          status === 200
            ? { beginId: "b", publicKey: { challenge: "Zm9v", allowCredentials: [] } }
            : { error: "unauthorized" },
        ),
        { status },
      );
    }
    if (u.endsWith("webauthn/reauth/complete")) {
      const status = opts?.completeStatus ?? 200;
      return new Response(
        JSON.stringify(
          status === 200
            ? { adminToken: ADMIN_TOKEN, expiresAt: Math.floor(Date.now() / 1000) + 300 }
            : { error: "verification_failed" },
        ),
        { status },
      );
    }
    return new Response("{}", { status: 404 });
  }) as unknown as typeof fetch;
  const creds: CredentialsApi = {
    get: async () => ({
      id: "c1",
      rawId: new TextEncoder().encode("r"),
      type: "public-key",
      response: {
        authenticatorData: new TextEncoder().encode("a"),
        clientDataJSON: new TextEncoder().encode("{}"),
        signature: new TextEncoder().encode("s"),
        userHandle: null,
      },
    }),
    create: async () => {
      throw new Error("unused");
    },
  };
  const count = (frag: string) => calls.filter((c) => c.includes(frag)).length;
  return { fetch: fetchMock, creds, count };
}

beforeEach(() => {
  __resetAdminToken();
});

afterEach(() => {
  __resetAdminToken();
});

describe("adminToken 内存态", () => {
  it("set/get/clear：秒级 expiresAt 归一 ms；过期即 null；绝不触碰 localStorage", () => {
    const expSec = Math.floor(Date.now() / 1000) + 300;
    setAdminToken(ADMIN_TOKEN, expSec);
    expect(getAdminToken()).toBe(ADMIN_TOKEN);
    expect(adminTokenExpiresAt()).toBe(expSec * 1000);
    // now 注入：过期瞬间（== expiresAt）即失效
    expect(getAdminToken(expSec * 1000)).toBeNull();
    expect(getAdminToken(expSec * 1000 - 1)).toBe(ADMIN_TOKEN);
    // ms 级 expiresAt 原样（容错路径）
    setAdminToken("t2", expSec * 1000);
    expect(adminTokenExpiresAt()).toBe(expSec * 1000);
    clearAdminToken();
    expect(getAdminToken()).toBeNull();
    expect(adminTokenExpiresAt()).toBeNull();
  });

  it("ensureAdminToken：无 grant 发起 re-auth 并缓存；后续调用免验证", async () => {
    const stub = reauthStub();
    const t1 = await ensureAdminToken("sess", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE });
    expect(t1).toBe(ADMIN_TOKEN);
    const t2 = await ensureAdminToken("sess", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE });
    expect(t2).toBe(ADMIN_TOKEN);
    expect(stub.count("reauth/begin")).toBe(1); // 只一次 ceremony
  });

  it("并发调用共享同一次 ceremony（单弹窗语义）", async () => {
    const stub = reauthStub();
    const [a, b, c] = await Promise.all([
      ensureAdminToken("sess", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE }),
      ensureAdminToken("sess", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE }),
      ensureAdminToken("sess", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE }),
    ]);
    expect(a).toBe(b);
    expect(b).toBe(c);
    expect(stub.count("reauth/begin")).toBe(1);
  });

  it("grant 过期后自动重新验证（5 分钟窗语义）", async () => {
    // 预置一个已过期的 grant → ensure 须重新走 ceremony
    setAdminToken("stale", Math.floor(Date.now() / 1000) - 1);
    const stub = reauthStub();
    const t = await ensureAdminToken("sess", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE });
    expect(t).toBe(ADMIN_TOKEN);
    expect(stub.count("reauth/begin")).toBe(1);
  });

  it("re-auth 失败抛出且不留半态；下次调用可重试", async () => {
    const fail = reauthStub({ completeStatus: 401 });
    await expect(
      ensureAdminToken("sess", { fetchImpl: fail.fetch, credentials: fail.creds, base: BASE }),
    ).rejects.toMatchObject({ code: "verification_failed" });
    expect(getAdminToken()).toBeNull();
    const ok = reauthStub();
    const t = await ensureAdminToken("sess", { fetchImpl: ok.fetch, credentials: ok.creds, base: BASE });
    expect(t).toBe(ADMIN_TOKEN);
  });

  it("reauthErrorText：错误码分流文案", () => {
    expect(reauthErrorText(new PasskeyError("aborted"))).toContain("已取消");
    expect(reauthErrorText(new PasskeyError("no_credentials"))).toContain("尚未注册通行钥匙");
    expect(reauthErrorText(new PasskeyError("verification_failed"))).toContain("验证失败");
    expect(reauthErrorText(new Error("x"))).toBe("管理验证失败");
  });
});
