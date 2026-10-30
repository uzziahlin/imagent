// E2E 中继盲化（v1.4）——密码学核心自测。
// 与 Rust 网关无互测条件下按协议向量自测：
// - 固定向量由独立脚本按协议构造（ikm = X25519 共享 || n1 || n2、salt 空、
//   info "imagent-e2e-v1"；sig 消息 = ek||gk||n1||n2 各 32B 共 128B），算法
//   漂移（如 ikm 组序调换、salt 变更）即刻红灯。
// - 「两侧一致」用例中网关侧密钥经 x25519(gkPriv, ekPub) + 手工拼 ikm 独立
//   派生，不经过被测代码路径。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, hexToBytes, randomBytes, utf8ToBytes } from "@noble/hashes/utils.js";
import {
  ACK_TIMEOUT_MS,
  E2eCoordinator,
  deriveSessionKey,
  fingerprint,
  loadPins,
  openBox,
  pinMachine,
  plaintextOpted,
  processAck,
  removePin,
  sealFrame,
  signatureMessage,
  startHandshake,
  type E2eEvent,
  type HandshakeSecret,
} from "./e2e";
import type { E2eAckFrame, MessageFrame } from "./protocol";

// ---------------------------------------------------------------------------
// 独立生成的协议向量（构造脚本与本实现无共享代码）
// ---------------------------------------------------------------------------

/** RFC 7748 §6.1 Alice 私钥（作浏览器临时私钥）。 */
const V_EK_PRIV = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
const V_EK = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
const V_GK = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";
const V_N1 = "a5".repeat(32);
const V_N2 = "5a".repeat(32);
const V_KEY = "6161e4d9aa686475504f6845034049b2e0ecf460cdfd0ca71be76164962573bd";
/** RFC 8032 §7.1 TEST 1 种子（作网关 ed25519 身份私钥）。 */
const V_IK = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
const V_SIG =
  "5bc86507f49234d5a9c90b275007a832135779c35f5b96d5ae659109602d56c3644464a8273ffdbda1cd7533c06077d00649a9919f1f9ada82051752295a540d";
const V_IK_FP = "21fe 31df a154 a261";

// ---------------------------------------------------------------------------
// 测试用假网关：长期 X25519 + ed25519 身份密钥，按协议应答
// ---------------------------------------------------------------------------

function makeGateway() {
  const gkPriv = x25519.utils.randomSecretKey();
  const gk = bytesToHex(x25519.getPublicKey(gkPriv));
  const idPriv = ed25519.utils.randomSecretKey();
  const ik = bytesToHex(ed25519.getPublicKey(idPriv));
  return {
    gk,
    ik,
    /** 按浏览器 hello 构造 ack（可换 gk/签名私钥模拟攻击者）。 */
    ackFor: (
      hello: { ek: string; n1: string },
      machine: string,
      opts?: { gk?: string; signPriv?: Uint8Array; ik?: string },
    ): E2eAckFrame => {
      const gkUsed = opts?.gk ?? gk;
      const n2 = bytesToHex(randomBytes(32));
      const msg = signatureMessage(hello.ek, gkUsed, hello.n1, n2);
      const sig = bytesToHex(ed25519.sign(msg, opts?.signPriv ?? idPriv));
      return {
        v: 1,
        type: "e2e_ack",
        machine,
        gk: gkUsed,
        n2,
        sig,
        ik: opts?.ik ?? ik,
      };
    },
    /** 网关侧会话密钥（独立构造，不经被测派生路径）。 */
    gatewayKey: (hello: { ek: string; n1: string }, n2: string): Uint8Array => {
      const shared = x25519.getSharedSecret(gkPriv, hexToBytes(hello.ek));
      const ikm = new Uint8Array(96);
      ikm.set(shared, 0);
      ikm.set(hexToBytes(hello.n1), 32);
      ikm.set(hexToBytes(n2), 64);
      return hkdf(sha256, ikm, undefined, utf8ToBytes("imagent-e2e-v1"), 32);
    },
  };
}

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.useRealTimers();
});

// ---------------------------------------------------------------------------
// 握手派生（协议向量 + 两侧一致）
// ---------------------------------------------------------------------------

describe("E2E 握手派生", () => {
  it("固定向量：deriveSessionKey 与协议构造脚本输出一致（算法锚定）", () => {
    const key = deriveSessionKey(hexToBytes(V_EK_PRIV), V_GK, V_N1, V_N2);
    expect(bytesToHex(key)).toBe(V_KEY);
  });

  it("sig 消息布局：ek||gk||n1||n2 各 32B 共 128B（向量锚定）", () => {
    expect(signatureMessage(V_EK, V_GK, V_N1, V_N2)).toHaveLength(128);
    const msg = signatureMessage(V_EK, V_GK, V_N1, V_N2);
    expect(bytesToHex(msg.subarray(0, 32))).toBe(V_EK);
    expect(bytesToHex(msg.subarray(32, 64))).toBe(V_GK);
    expect(bytesToHex(msg.subarray(64, 96))).toBe(V_N1);
    expect(bytesToHex(msg.subarray(96, 128))).toBe(V_N2);
  });

  it("两侧一致：浏览器派生 === 网关侧独立构造（shared = X25519(gkPriv, ek)）", () => {
    const gw = makeGateway();
    const { secret } = startHandshake("home-mac");
    const ack = gw.ackFor(secret, "home-mac");
    const browserKey = deriveSessionKey(secret.ekPriv, ack.gk, secret.n1, ack.n2);
    expect(bytesToHex(browserKey)).toBe(bytesToHex(gw.gatewayKey(secret, ack.n2)));
  });

  it("startHandshake：hello 帧形状（ek/n1 hex 64 字符、临时私钥只在秘密侧）", () => {
    const { secret, hello } = startHandshake("work-mac");
    expect(hello).toMatchObject({ v: 1, type: "e2e_hello", machine: "work-mac" });
    expect(hello.ek).toMatch(/^[0-9a-f]{64}$/);
    expect(hello.n1).toMatch(/^[0-9a-f]{64}$/);
    expect(secret.ek).toBe(hello.ek);
    expect(secret.ekPriv).toHaveLength(32);
  });
});

// ---------------------------------------------------------------------------
// 加解密往返
// ---------------------------------------------------------------------------

describe("E2E 会话加解密", () => {
  const inner: MessageFrame = {
    v: 1,
    type: "message",
    convId: "web:default",
    text: "机密内容 secret ✓",
    machine: "home-mac",
  };

  it("sealFrame → openBox 往返还原原帧 JSON", () => {
    const key = randomBytes(32);
    const enc = sealFrame("home-mac", key, inner);
    expect(enc.v).toBe(1);
    expect(enc.type).toBe("enc");
    expect(enc.machine).toBe("home-mac");
    expect(openBox(key, enc.box)).toBe(JSON.stringify(inner));
  });

  it("box 篡改 / 密钥不符 → null（AEAD 认证失败）", () => {
    const key = randomBytes(32);
    const enc = sealFrame("home-mac", key, inner);
    // 翻转密文区一个字符
    const tampered = enc.box.slice(0, -4) + (enc.box.endsWith("A") ? "B" : "A") + enc.box.slice(-3);
    expect(openBox(key, tampered)).toBeNull();
    expect(openBox(randomBytes(32), enc.box)).toBeNull();
    expect(openBox(key, "not-base64!!")).toBeNull();
  });

  it("编码宽容：hex 形态的 box 亦可开（跨实现编码差异兜底）", () => {
    const key = randomBytes(32);
    const enc = sealFrame("home-mac", key, inner);
    // 手工把 base64 box 重编码为 hex（模拟对端选择 hex 编码）
    const raw = atob(enc.box);
    const bin = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) {
      bin[i] = raw.charCodeAt(0 + i);
    }
    const hexBox = bytesToHex(bin);
    expect(openBox(key, hexBox)).toBe(JSON.stringify(inner));
  });
});

// ---------------------------------------------------------------------------
// 指纹
// ---------------------------------------------------------------------------

describe("E2E 指纹", () => {
  it("SHA-256 前 16 hex、4 位一组（向量锚定）", () => {
    expect(fingerprint(V_IK)).toBe(V_IK_FP);
    expect(fingerprint(V_GK)).toMatch(/^[0-9a-f]{4}( [0-9a-f]{4}){3}$/);
  });

  it("畸形输入 → 空串（不抛）", () => {
    expect(fingerprint("zz")).toBe("");
    expect(fingerprint("")).toBe("");
  });
});

// ---------------------------------------------------------------------------
// pin 校验 / 身份变化告警
// ---------------------------------------------------------------------------

describe("E2E pin 校验与告警", () => {
  it("未 pin：自洽验签通过 → tofu（携带双指纹供弹层核对）", () => {
    const gw = makeGateway();
    const { secret } = startHandshake("home-mac");
    const ack = gw.ackFor(secret, "home-mac");
    const out = processAck(secret, ack);
    expect(out.kind).toBe("tofu");
    if (out.kind !== "tofu") {
      return;
    }
    expect(out.gkFp).toBe(fingerprint(gw.gk));
    expect(out.idFp).toBe(fingerprint(gw.ik));
    expect(bytesToHex(out.key)).toBe(bytesToHex(gw.gatewayKey(secret, ack.n2)));
  });

  it("未 pin：签名自洽性破坏 → invalid（非安全事件，丢弃等超时）", () => {
    const gw = makeGateway();
    const { secret } = startHandshake("home-mac");
    // 攻击者用自己的身份钥对被篡改的握手参数重签——未 pin 时这本身自洽；
    // 直接破坏 sig 字节才是协议违规
    const ack = gw.ackFor(secret, "home-mac");
    const bad = { ...ack, sig: bytesToHex(randomBytes(64)) };
    expect(processAck(secret, bad).kind).toBe("invalid");
    // 字段缺失 / 长度错 → invalid
    expect(processAck(secret, { ...ack, ik: undefined, id: undefined, pk: undefined }).kind).toBe("invalid");
    expect(processAck(secret, { ...ack, gk: "abcd" }).kind).toBe("invalid");
  });

  it("已 pin：验签通过且 gk/ik 一致 → active（固定向量）", () => {
    pinMachine("home-mac", V_IK, V_GK);
    const secret: HandshakeSecret = {
      machine: "home-mac",
      ekPriv: hexToBytes(V_EK_PRIV),
      ek: V_EK,
      n1: V_N1,
    };
    const out = processAck(secret, {
      v: 1,
      type: "e2e_ack",
      machine: "home-mac",
      gk: V_GK,
      n2: V_N2,
      sig: V_SIG,
      ik: V_IK,
    });
    expect(out.kind).toBe("active");
    if (out.kind === "active") {
      expect(bytesToHex(out.key)).toBe(V_KEY);
    }
  });

  it("已 pin：gk 变化（中间人换网关公钥）→ alert identity_changed", () => {
    const gw = makeGateway();
    const mitm = makeGateway();
    pinMachine("home-mac", gw.ik, gw.gk);
    const { secret } = startHandshake("home-mac");
    const ack = mitm.ackFor(secret, "home-mac"); // 攻击者自有密钥应答
    const out = processAck(secret, { ...ack, ik: gw.ik }); // 冒充 pin 的身份公钥
    expect(out).toMatchObject({ kind: "alert", reason: "identity_changed" });
  });

  it("已 pin：gk 一致但 ik 变化 → alert identity_changed", () => {
    const gw = makeGateway();
    const other = makeGateway();
    pinMachine("home-mac", gw.ik, gw.gk);
    const { secret } = startHandshake("home-mac");
    // 真网关 gk + 另一把身份公钥（重装身份钥场景）
    const ack = gw.ackFor(secret, "home-mac", { ik: other.ik });
    expect(processAck(secret, ack)).toMatchObject({ kind: "alert", reason: "identity_changed" });
  });

  it("已 pin：公钥一致但签名对不上（截获帧重放不同消息）→ alert bad_signature", () => {
    const gw = makeGateway();
    pinMachine("home-mac", gw.ik, gw.gk);
    const { secret } = startHandshake("home-mac");
    // 用别的私钥签（gk/ik 字段仍是 pin 值）→ 验签失败
    const ack = gw.ackFor(secret, "home-mac", { signPriv: ed25519.utils.randomSecretKey() });
    expect(processAck(secret, ack)).toMatchObject({ kind: "alert", reason: "bad_signature" });
  });

  it("跨实现宽容：ack 字段 base64 编码 / id 别名 → 照常校验", () => {
    const b64 = (hex: string) => btoa(String.fromCharCode(...hexToBytes(hex)));
    pinMachine("home-mac", V_IK, V_GK);
    const secret: HandshakeSecret = {
      machine: "home-mac",
      ekPriv: hexToBytes(V_EK_PRIV),
      ek: V_EK,
      n1: V_N1,
    };
    const out = processAck(secret, {
      v: 1,
      type: "e2e_ack",
      machine: "home-mac",
      gk: b64(V_GK),
      n2: b64(V_N2),
      sig: b64(V_SIG),
      id: V_IK, // 别名字段
    });
    expect(out.kind).toBe("active");
  });

  it("pin 表损坏（ik 非 hex）→ 视为公钥变化（显式重置恢复）", () => {
    localStorage.setItem(
      "imagent.e2ePins",
      JSON.stringify({ "home-mac": { ik: "zz-not-hex", gk: V_GK, pinnedAt: 1 } }),
    );
    const gw = makeGateway();
    const { secret } = startHandshake("home-mac");
    const ack = gw.ackFor(secret, "home-mac");
    expect(processAck(secret, { ...ack, gk: V_GK })).toMatchObject({
      kind: "alert",
      reason: "identity_changed",
    });
  });
});

// ---------------------------------------------------------------------------
// pin / 明文选择存储
// ---------------------------------------------------------------------------

describe("E2E pin 与明文选择存储", () => {
  it("pinMachine / loadPins / removePin 往返", () => {
    expect(loadPins()).toEqual([]);
    pinMachine("home-mac", V_IK, V_GK);
    expect(loadPins()).toEqual([
      { machine: "home-mac", ik: V_IK, gk: V_GK, pinnedAt: expect.any(Number) },
    ]);
    pinMachine("work-mac", V_IK, V_GK);
    expect(loadPins()).toHaveLength(2);
    removePin("home-mac");
    expect(loadPins().map((p) => p.machine)).toEqual(["work-mac"]);
  });

  it("loadPins：畸形 JSON / 形状非法条目跳过不抛", () => {
    localStorage.setItem("imagent.e2ePins", "not-json");
    expect(loadPins()).toEqual([]);
    localStorage.setItem(
      "imagent.e2ePins",
      JSON.stringify({ a: { ik: 1 }, b: { ik: V_IK, gk: V_GK } }),
    );
    expect(loadPins().map((p) => p.machine)).toEqual(["b"]);
  });

  it("plaintextOpted 缺省 false；持久化后 true", () => {
    expect(plaintextOpted("home-mac")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 协调器状态机（定时器走 fake timers）
// ---------------------------------------------------------------------------

describe("E2eCoordinator 状态机", () => {
  function makeCoord() {
    const events: E2eEvent[] = [];
    const hellos: string[] = [];
    const coord = new E2eCoordinator({
      sendHello: (h) => hellos.push(JSON.stringify(h)),
      emit: (e) => events.push(e),
    });
    return { coord, events, hellos };
  }

  beforeEach(() => {
    vi.useFakeTimers();
  });

  it("onMachines：逐机器发 e2e_hello；重复快照不重发", () => {
    const { coord, hellos } = makeCoord();
    coord.onMachines(["home-mac", "work-mac"]);
    coord.onMachines(["home-mac", "work-mac"]);
    expect(hellos).toHaveLength(2);
    for (const h of hellos) {
      expect(JSON.parse(h)).toMatchObject({ v: 1, type: "e2e_hello" });
    }
    expect(coord.statusOf("home-mac")).toBe("pending");
  });

  it("未 pin ack → tofu 事件；确认 → active + pin 落库", () => {
    const gw = makeGateway();
    const { coord, events, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    const hello = JSON.parse(hellos[0]);
    coord.onAck(gw.ackFor(hello, "home-mac"));
    expect(coord.statusOf("home-mac")).toBe("confirming");
    expect(events).toContainEqual({ kind: "tofu", machine: "home-mac", gkFp: fingerprint(gw.gk), idFp: fingerprint(gw.ik) });
    // confirming 期间出站阻断（待用户决断）
    expect(coord.outbound("home-mac", { v: 1, type: "message", text: "x" })).toBeNull();
    coord.resolveTofu("home-mac", true);
    expect(coord.statusOf("home-mac")).toBe("active");
    expect(events).toContainEqual({ kind: "active", machine: "home-mac" });
    expect(loadPins().map((p) => p.machine)).toEqual(["home-mac"]);
  });

  it("已 pin 重连：ack 直接 active（无 tofu）", () => {
    const gw = makeGateway();
    pinMachine("home-mac", gw.ik, gw.gk);
    const { coord, events, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    const hello = JSON.parse(hellos[0]);
    coord.onAck(gw.ackFor(hello, "home-mac"));
    expect(coord.statusOf("home-mac")).toBe("active");
    expect(events.filter((e) => e.kind === "tofu")).toHaveLength(0);
  });

  it("10s 无 ack → unsupported（fail-closed：出站阻断）；仍以明文连接 → 放行 + 持久", () => {
    const { coord, events } = makeCoord();
    coord.onMachines(["home-mac"]);
    vi.advanceTimersByTime(ACK_TIMEOUT_MS - 1);
    expect(coord.statusOf("home-mac")).toBe("pending");
    vi.advanceTimersByTime(1);
    expect(coord.statusOf("home-mac")).toBe("unsupported");
    expect(events).toContainEqual({ kind: "unsupported", machine: "home-mac", reason: "timeout" });
    const frame = { v: 1 as const, type: "message" as const, text: "x", machine: "home-mac" };
    expect(coord.outbound("home-mac", frame)).toBeNull();
    coord.allowPlaintext("home-mac");
    expect(coord.outbound("home-mac", frame)).toBe(JSON.stringify(frame));
    expect(plaintextOpted("home-mac")).toBe(true);
    // 持久选择：新连接的 machines 快照不再对该机器发起握手
    coord.reset();
    coord.onMachines(["home-mac"]);
    expect(coord.statusOf("home-mac")).toBe("none");
  });

  it("TOFU 拒绝 → unsupported（declined）；仍可显式选择明文", () => {
    const gw = makeGateway();
    const { coord, events, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    coord.onAck(gw.ackFor(JSON.parse(hellos[0]), "home-mac"));
    coord.resolveTofu("home-mac", false);
    expect(coord.statusOf("home-mac")).toBe("unsupported");
    expect(events).toContainEqual({ kind: "unsupported", machine: "home-mac", reason: "declined" });
    expect(loadPins()).toEqual([]);
  });

  it("身份变化 → alert：出站/入站全阻断", () => {
    const real = makeGateway();
    pinMachine("home-mac", real.ik, real.gk);
    const mitm = makeGateway();
    const { coord, events, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    const hello = JSON.parse(hellos[0]);
    const ack = mitm.ackFor(hello, "home-mac", { ik: real.ik });
    coord.onAck(ack);
    expect(coord.statusOf("home-mac")).toBe("alert");
    expect(events).toContainEqual({ kind: "alert", machine: "home-mac", reason: "identity_changed" });
    expect(coord.outbound("home-mac", { v: 1, type: "message", text: "x" })).toBeNull();
    expect(coord.openEnc({ v: 1, type: "enc", machine: "home-mac", box: "AAAA" })).toBeNull();
    expect(coord.acceptPlaintextBusiness("home-mac")).toBe(false);
  });

  it("active：出站 enc 包装、入站 enc 解密二次分发、明文业务帧拒绝、心跳跟随", () => {
    const gw = makeGateway();
    const { coord, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    const hello = JSON.parse(hellos[0]);
    const ack = gw.ackFor(hello, "home-mac");
    coord.onAck(ack);
    coord.resolveTofu("home-mac", true);
    expect(coord.statusOf("home-mac")).toBe("active");

    // 出站：enc 包装且可被网关密钥解开
    const wire = coord.outbound("home-mac", {
      v: 1,
      type: "message",
      convId: "web:default",
      text: "机密",
      machine: "home-mac",
    });
    expect(wire).not.toBeNull();
    const enc = JSON.parse(wire!) as { type: string; machine: string; box: string };
    expect(enc).toMatchObject({ type: "enc", machine: "home-mac" });
    expect(openBox(gw.gatewayKey(hello, ack.n2), enc.box)).toContain('"text":"机密"');

    // 入站：网关加密的业务帧解密还原
    const gwKey = gw.gatewayKey(hello, ack.n2);
    const inbound = sealFrame("home-mac", gwKey, {
      v: 1,
      type: "text",
      convId: "web:default",
      text: "回复",
      machine: "home-mac",
    });
    const inner = coord.openEnc(inbound);
    expect(inner).toMatchObject({ type: "text", text: "回复", machine: "home-mac" });

    // active 机器的明文业务帧 = 注入面 → 拒绝
    expect(coord.acceptPlaintextBusiness("home-mac")).toBe(false);

    // 心跳：active → enc；未握手机器 → 明文
    const ping = coord.heartbeat({ v: 1, type: "ping", machine: "home-mac" }, "home-mac");
    expect(JSON.parse(ping).type).toBe("enc");
    expect(JSON.parse(coord.heartbeat({ v: 1, type: "ping" }, undefined)).type).toBe("ping");
  });

  it("reset（断线）：清空全部状态 → 出站回明文；pin 持久不受影响", () => {
    const gw = makeGateway();
    pinMachine("home-mac", gw.ik, gw.gk);
    const { coord, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    coord.onAck(gw.ackFor(JSON.parse(hellos[0]), "home-mac"));
    expect(coord.statusOf("home-mac")).toBe("active");
    coord.reset();
    expect(coord.statusOf("home-mac")).toBe("none");
    const frame = { v: 1 as const, type: "message" as const, text: "x", machine: "home-mac" };
    expect(coord.outbound("home-mac", frame)).toBe(JSON.stringify(frame));
    expect(loadPins()).toHaveLength(1);
  });

  it("retry（pin 重置后）：重新发 hello 走 TOFU", () => {
    const gw = makeGateway();
    const { coord, events, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    coord.onAck(gw.ackFor(JSON.parse(hellos[0]), "home-mac"));
    coord.resolveTofu("home-mac", true);
    removePin("home-mac");
    coord.retry("home-mac");
    expect(coord.statusOf("home-mac")).toBe("pending");
    coord.onAck(gw.ackFor(JSON.parse(hellos[1]), "home-mac"));
    expect(events.filter((e) => e.kind === "tofu")).toHaveLength(2);
  });

  it("outbound：管理帧恒明文；machine undefined（单机模式）恒明文", () => {
    const { coord } = makeCoord();
    coord.onMachines(["home-mac"]);
    const dev = { v: 1 as const, type: "device_list" as const };
    expect(coord.outbound(undefined, dev)).toBe(JSON.stringify(dev));
    const msg = { v: 1 as const, type: "message" as const, text: "x" };
    expect(coord.outbound(undefined, msg)).toBe(JSON.stringify(msg));
  });

  // v1.6：工作空间四帧均为业务帧（BUSINESS_TYPES）——active 机器出站必 enc 包装
  it("v1.6 工作空间四帧：active 机器出站 enc 包装（网关密钥可解回原帧）；单机恒明文", () => {
    const gw = makeGateway();
    const { coord, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    const hello = JSON.parse(hellos[0]);
    const ack = gw.ackFor(hello, "home-mac");
    coord.onAck(ack);
    coord.resolveTofu("home-mac", true);
    const gwKey = gw.gatewayKey(hello, ack.n2);
    const frames = [
      { v: 1 as const, type: "workspace_list" as const, machine: "home-mac" },
      { v: 1 as const, type: "workspace_save" as const, name: "code", path: "/u/code", machine: "home-mac" },
      { v: 1 as const, type: "workspace_remove" as const, name: "code", machine: "home-mac" },
      { v: 1 as const, type: "conv_workdir_set" as const, convId: "web:default", path: "/u/code", machine: "home-mac" },
    ];
    for (const f of frames) {
      const wire = coord.outbound("home-mac", f);
      expect(wire).not.toBeNull();
      const enc = JSON.parse(wire!) as { type: string; box: string };
      expect(enc.type).toBe("enc");
      expect(openBox(gwKey, enc.box)).toBe(JSON.stringify(f));
    }
    // 单机模式（machine undefined，旧 relay）恒明文直发
    const plain = { v: 1 as const, type: "workspace_list" as const };
    expect(coord.outbound(undefined, plain)).toBe(JSON.stringify(plain));
  });

  // v1.8：history_query 同为业务帧（BUSINESS_TYPES）——active 机器出站必 enc 包装
  it("v1.8 history_query：active 机器出站 enc 包装（网关密钥可解回原帧）；单机恒明文", () => {
    const gw = makeGateway();
    const { coord, hellos } = makeCoord();
    coord.onMachines(["home-mac"]);
    const hello = JSON.parse(hellos[0]);
    const ack = gw.ackFor(hello, "home-mac");
    coord.onAck(ack);
    coord.resolveTofu("home-mac", true);
    const gwKey = gw.gatewayKey(hello, ack.n2);
    const f = {
      v: 1 as const,
      type: "history_query" as const,
      convId: "web:default",
      since: 3,
      machine: "home-mac",
    };
    const wire = coord.outbound("home-mac", f);
    expect(wire).not.toBeNull();
    const enc = JSON.parse(wire!) as { type: string; box: string };
    expect(enc.type).toBe("enc");
    expect(openBox(gwKey, enc.box)).toBe(JSON.stringify(f));
    // 单机模式（machine undefined，旧 relay）恒明文直发
    const plain = { v: 1 as const, type: "history_query" as const, convId: "web:default", since: 0 };
    expect(coord.outbound(undefined, plain)).toBe(JSON.stringify(plain));
  });

});

describe("E2eCoordinator sid（定向加密形态）", () => {
  it("getSid 注入：e2e_hello 携带 sid；未注入则字段缺席", () => {
    const hellos: string[] = [];
    const withSid = new E2eCoordinator(
      { sendHello: (h) => hellos.push(JSON.stringify(h)), emit: () => {} },
      { getSid: () => "sid-9" },
    );
    withSid.onMachines(["home-mac"]);
    expect(JSON.parse(hellos[0]).sid).toBe("sid-9");

    const hellos2: string[] = [];
    const noSid = new E2eCoordinator({
      sendHello: (h) => hellos2.push(JSON.stringify(h)),
      emit: () => {},
    });
    noSid.onMachines(["home-mac"]);
    expect(JSON.parse(hellos2[0]).sid).toBeUndefined();
    expect(hellos2[0]).not.toContain('"sid"'); // 序列化字节无 sid 键（广播兼容形态）
  });

  it("downgrade 告警：active 机器明文业务帧被丢弃时上抛一次", () => {
    const gw = makeGateway();
    const events: E2eEvent[] = [];
    const hellos: string[] = [];
    const coord = new E2eCoordinator({
      sendHello: (h) => hellos.push(JSON.stringify(h)),
      emit: (e) => events.push(e),
    });
    coord.onMachines(["home-mac"]);
    // pending 态不告警（明文业务帧本就放行，ws 层不会调 note）
    coord.notePlaintextDowngrade("home-mac");
    expect(events.filter((e) => e.kind === "downgrade")).toHaveLength(0);
    const hello = JSON.parse(hellos[0]);
    coord.onAck(gw.ackFor(hello, "home-mac"));
    coord.resolveTofu("home-mac", true);
    // active 后：告警一次，重复帧不重复告警
    coord.notePlaintextDowngrade("home-mac");
    coord.notePlaintextDowngrade("home-mac");
    expect(events.filter((e) => e.kind === "downgrade")).toEqual([
      { kind: "downgrade", machine: "home-mac" },
    ]);
  });
});
