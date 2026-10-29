// lib/webauthn 单测（v1.3 passkey）：mock fetch + credentials API（无真浏览器
// 认证器）。覆盖：base64url 双向、登录两阶段（begin options 转换 → get →
// assertion 回传 JSON 形状 → complete 存 session）、注册两阶段（Bearer 头 +
// label + attestation 转换）、各失败码透传（no_credentials / aborted /
// verification_failed / 网络）。
import { describe, expect, it, vi } from "vitest";
import {
  PasskeyError,
  fromB64Url,
  passkeyLogin,
  passkeyRegister,
  toB64Url,
  webauthnSupported,
  type CredentialsApi,
} from "./webauthn";

// ---------------------------------------------------------------------------
// 测试基座：可控 fetch（按 URL 顺序回包）+ 可控 credentials
// ---------------------------------------------------------------------------

interface Stub {
  fetch: typeof fetch;
  calls: { url: string; init: RequestInit | undefined }[];
  creds: CredentialsApi;
  /** credentials.get / create 收到的 options（断言转换形状）。 */
  getOpts: unknown[];
  createOpts: unknown[];
}

/** base64url 编解码 Uint8Array ⇄ JSON 断言用。 */
function b64(s: string): string {
  return toB64Url(new TextEncoder().encode(s));
}

function makeStub(responses: { match: (url: string) => boolean; status: number; body: unknown }[]): Stub {
  const calls: { url: string; init: RequestInit | undefined }[] = [];
  const fetchMock = vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
    const u = typeof url === "string" ? url : url.toString();
    calls.push({ url: u, init });
    const hit = responses.find((r) => r.match(u));
    if (!hit) {
      return new Response(JSON.stringify({ error: "unexpected_url" }), { status: 404 });
    }
    return new Response(JSON.stringify(hit.body), { status: hit.status });
  }) as unknown as typeof fetch;
  const getOpts: unknown[] = [];
  const createOpts: unknown[] = [];
  const creds: CredentialsApi = {
    get: async (o) => {
      getOpts.push(o);
      return {
        id: "cred-1",
        rawId: new TextEncoder().encode("raw-cred"),
        type: "public-key",
        response: {
          authenticatorData: new TextEncoder().encode("auth-data"),
          clientDataJSON: new TextEncoder().encode("{}"),
          signature: new TextEncoder().encode("sig"),
          userHandle: new TextEncoder().encode("user"),
        },
      };
    },
    create: async (o) => {
      createOpts.push(o);
      return {
        id: "cred-1",
        rawId: new TextEncoder().encode("raw-cred"),
        type: "public-key",
        response: {
          attestationObject: new TextEncoder().encode("att-obj"),
          clientDataJSON: new TextEncoder().encode("{}"),
          getTransports: () => ["internal"],
        },
      };
    },
  };
  return { fetch: fetchMock, calls, creds, getOpts, createOpts };
}

const BASE = "/imagent/";

// ---------------------------------------------------------------------------
// base64url
// ---------------------------------------------------------------------------

describe("base64url 编解码", () => {
  it("RFC 4648 测试向量（无 padding）", () => {
    expect(toB64Url(new Uint8Array([]))).toBe("");
    expect(toB64Url(new TextEncoder().encode("f"))).toBe("Zg");
    expect(toB64Url(new TextEncoder().encode("fo"))).toBe("Zm8");
    expect(toB64Url(new TextEncoder().encode("foo"))).toBe("Zm9v");
    expect(toB64Url(new TextEncoder().encode("foob"))).toBe("Zm9vYg");
    expect(toB64Url(new TextEncoder().encode("fooba"))).toBe("Zm9vYmE");
    expect(toB64Url(new TextEncoder().encode("foobar"))).toBe("Zm9vYmFy");
  });

  it("roundtrip + url-safe 字母表 + padding 容忍", () => {
    const bytes = new Uint8Array([0xfb, 0xff, 0xfe, 0x00, 0x7f, 0x80, 0x01]);
    const enc = toB64Url(bytes);
    expect(enc).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(Array.from(fromB64Url(enc))).toEqual(Array.from(bytes));
    expect(Array.from(fromB64Url(enc + "=="))).toEqual(Array.from(bytes));
  });

  it("非法字符抛错", () => {
    expect(() => fromB64Url("ab?d")).toThrow();
  });

  it("webauthnSupported：无 PublicKeyCredential 时 false（jsdom）", () => {
    expect(webauthnSupported()).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 登录流程
// ---------------------------------------------------------------------------

describe("passkeyLogin", () => {
  it("两阶段成功：begin（challenge/allowCredentials 转 ArrayBuffer）→ get → complete 回传 JSON 形状 → 返回 session", async () => {
    const challengeB64 = b64("challenge-bytes");
    const stub = makeStub([
      {
        match: (u) => u.endsWith("webauthn/login/begin"),
        status: 200,
        body: {
          beginId: "begin-1",
          publicKey: {
            rpId: "localhost",
            challenge: challengeB64,
            allowCredentials: [],
            userVerification: "required",
          },
        },
      },
      {
        match: (u) => u.endsWith("webauthn/login/complete"),
        status: 200,
        body: { session: { id: "sid-1", token: "tok-1", expiresAt: 1_900_000_000 } },
      },
    ]);
    const session = await passkeyLogin({
      fetchImpl: stub.fetch,
      credentials: stub.creds,
      base: BASE,
    });
    expect(session).toEqual({ id: "sid-1", token: "tok-1", expiresAt: 1_900_000_000 });

    // begin：POST 空 body、无 Authorization。
    expect(stub.calls[0]?.url).toBe("/imagent/webauthn/login/begin");
    expect((stub.calls[0]?.init?.headers as Record<string, string>).authorization).toBeUndefined();

    // get options：challenge 转成 Uint8Array，allowCredentials 数组保留。
    const opts = stub.getOpts[0] as { publicKey: Record<string, unknown> };
    expect(opts.publicKey.rpId).toBe("localhost");
    // TypedArray 跨构造实例的 toEqual 在 vitest 有假阴性，统一比 byte 数组。
    expect(Array.from(opts.publicKey.challenge as Uint8Array)).toEqual(
      Array.from(new TextEncoder().encode("challenge-bytes")),
    );
    expect(opts.publicKey.allowCredentials).toEqual([]);

    // complete：credential 字段全部 base64url 字符串（对齐 webauthn-rs serde）。
    const complete = JSON.parse(stub.calls[1]!.init!.body as string);
    expect(complete.beginId).toBe("begin-1");
    expect(complete.credential.id).toBe("cred-1");
    expect(complete.credential.rawId).toBe(toB64Url(new TextEncoder().encode("raw-cred")));
    expect(complete.credential.response.authenticatorData).toBe(
      toB64Url(new TextEncoder().encode("auth-data")),
    );
    expect(complete.credential.response.userHandle).toBe(
      toB64Url(new TextEncoder().encode("user")),
    );
  });

  it("begin 400 no_credentials → 透传错误码", async () => {
    const stub = makeStub([
      {
        match: (u) => u.endsWith("webauthn/login/begin"),
        status: 400,
        body: { error: "no_credentials" },
      },
    ]);
    await expect(
      passkeyLogin({ fetchImpl: stub.fetch, credentials: stub.creds, base: BASE }),
    ).rejects.toMatchObject({ code: "no_credentials" });
    expect(stub.getOpts.length).toBe(0); // begin 失败不得调认证器
  });

  it("complete 401 verification_failed → 透传", async () => {
    const stub = makeStub([
      {
        match: (u) => u.endsWith("login/begin"),
        status: 200,
        body: { beginId: "b", publicKey: { challenge: b64("c"), allowCredentials: [] } },
      },
      {
        match: (u) => u.endsWith("login/complete"),
        status: 401,
        body: { error: "verification_failed" },
      },
    ]);
    await expect(
      passkeyLogin({ fetchImpl: stub.fetch, credentials: stub.creds, base: BASE }),
    ).rejects.toMatchObject({ code: "verification_failed" });
  });

  it("用户取消（NotAllowedError）→ aborted；网络断开 → network", async () => {
    const cancelCreds: CredentialsApi = {
      get: async () => {
        throw new DOMException("The operation either timed out or was canceled", "NotAllowedError");
      },
      create: async () => {
        throw new Error("unused");
      },
    };
    const stub = makeStub([
      {
        match: (u) => u.endsWith("login/begin"),
        status: 200,
        body: { beginId: "b", publicKey: { challenge: b64("c"), allowCredentials: [] } },
      },
    ]);
    await expect(
      passkeyLogin({ fetchImpl: stub.fetch, credentials: cancelCreds, base: BASE }),
    ).rejects.toMatchObject({ code: "aborted" });

    const deadFetch = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    }) as unknown as typeof fetch;
    await expect(passkeyLogin({ fetchImpl: deadFetch, base: BASE })).rejects.toMatchObject({
      code: "network",
    });
  });
});

// ---------------------------------------------------------------------------
// 注册流程
// ---------------------------------------------------------------------------

describe("passkeyRegister", () => {
  it("两阶段成功：Bearer 头 + label；user.id/challenge/excludeCredentials 转换；transports 采集", async () => {
    const stub = makeStub([
      {
        match: (u) => u.endsWith("register/begin"),
        status: 200,
        body: {
          beginId: "reg-1",
          publicKey: {
            rp: { id: "localhost", name: "imagent" },
            challenge: b64("reg-challenge"),
            user: { id: b64("user-id"), name: "imagent", displayName: "imagent" },
            excludeCredentials: [{ type: "public-key", id: b64("old-cred") }],
            pubKeyCredParams: [{ alg: -7, type: "public-key" }],
          },
        },
      },
      {
        match: (u) => u.endsWith("register/complete"),
        status: 200,
        body: { credential: { id: "cred-1", label: "Mac/Chrome", created: 1_900 } },
      },
    ]);
    const cred = await passkeyRegister("session-token-1", "Mac/Chrome", {
      fetchImpl: stub.fetch,
      credentials: stub.creds,
      base: BASE,
    });
    expect(cred).toEqual({ id: "cred-1", label: "Mac/Chrome", created: 1_900 });

    // begin：Bearer sessionToken + label body。
    const beginHeaders = stub.calls[0]?.init?.headers as Record<string, string>;
    expect(beginHeaders.authorization).toBe("Bearer session-token-1");
    expect(JSON.parse(stub.calls[0]!.init!.body as string)).toEqual({ label: "Mac/Chrome" });

    // create options：challenge/user.id/exclude id 全转 Uint8Array。
    const opts = stub.createOpts[0] as {
      publicKey: { challenge: unknown; user: { id: unknown }; excludeCredentials: { id: unknown }[] };
    };
    expect(Array.from(opts.publicKey.challenge as Uint8Array)).toEqual(
      Array.from(new TextEncoder().encode("reg-challenge")),
    );
    expect(Array.from(opts.publicKey.user.id as Uint8Array)).toEqual(
      Array.from(new TextEncoder().encode("user-id")),
    );
    expect(Array.from(opts.publicKey.excludeCredentials[0]!.id as Uint8Array)).toEqual(
      Array.from(new TextEncoder().encode("old-cred")),
    );

    // complete：credential JSON + attestation 字段 + transports。
    const complete = JSON.parse(stub.calls[1]!.init!.body as string);
    expect(complete.beginId).toBe("reg-1");
    expect(complete.credential.response.attestationObject).toBe(
      toB64Url(new TextEncoder().encode("att-obj")),
    );
    expect(complete.credential.response.transports).toEqual(["internal"]);
  });

  it("401 unauthorized 透传（无/过期会话）", async () => {
    const stub = makeStub([
      {
        match: (u) => u.endsWith("register/begin"),
        status: 401,
        body: { error: "unauthorized" },
      },
    ]);
    await expect(
      passkeyRegister("stale", "x", { fetchImpl: stub.fetch, credentials: stub.creds, base: BASE }),
    ).rejects.toBeInstanceOf(PasskeyError);
  });
});
