// E2E 中继盲化（协议 v1.4）——浏览器侧密码学与逐机器握手状态机。
//
// 威胁模型：relay（ECS）只做盲转发，不得解读业务帧。浏览器 ↔ 每台机器的
// 网关各自建立 X25519 会话，业务帧 XChaCha20-Poly1305 加密后经 relay 透传；
// relay 自产管理帧（hello_ack/machines/devices/creds/error/pong）恒明文。
//
// 握手（每台在线机器一个会话，单 WS 连接复用）：
//   浏览器 → e2e_hello{machine, ek, n1}          ek=临时 X25519 公钥 32B
//   网关   → e2e_ack{machine, gk, n2, sig, ik}   gk=网关 X25519 公钥 32B
//                                                 ik=网关 ed25519 身份公钥 32B
//   k = HKDF-SHA256(ikm = X25519(ek_priv, gk) || n1 || n2, salt=空,
//                    info = "imagent-e2e-v1", L = 32)
//   sig = ed25519.sign(ik_priv, bytes(ek) || bytes(gk) || bytes(n1) || bytes(n2))
//
// 编码规约（跨实现对齐点，Rust 网关侧需一致）：
// - ek/gk/n1/n2/ik/sig：hex（小写，出站规范形）；入站宽容 base64（hex 字符
//   集 ⊂ base64 字符集，按「先 hex 后 base64 + 字节长度校验」消歧）。
// - box：base64（标准含 padding）of nonce(24B) || 密文||tag(16B)；入站在
//   base64 解密失败时回退按 hex 解一次（AEAD 认证失败即判编码不符，天然
//   消歧——错误编码不可能碰巧通过 Poly1305 校验）。
// - ik 首次连接必须随 e2e_ack 携带（未 pin 无信任锚）；id/pk 为解析侧
//   宽容别名。
//
// TOFU pin 模型：localStorage `imagent.e2ePins`（machine → {ik, gk, pinnedAt}）。
// gk/ik 是机器长期密钥（`imagent start` 日志打印其指纹），每连接的临时熵由
// 浏览器侧 ek + 双侧 n1/n2 提供。已 pin：验签失败或 gk/ik 与 pin 不符 →
// 红色告警（机器身份变化，可能被中间人）+ 断开该机器加密（fail-closed）。
// 未 pin（每机器首次）：指纹确认弹层（SHA-256 前 16 hex 分组展示），用户与
// 终端日志核对后 pin。
//
// fail-closed：握手发起 10s 无 e2e_ack（旧网关）→ 「此机器不支持加密连接」
// 横幅，发送禁用（不自动明文降级）；「仍以明文连接」显式按钮记录选择
// （localStorage `imagent.e2ePlaintext`，此后该机器不再尝试握手）。

import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { xchacha20poly1305 } from "@noble/ciphers/chacha.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, concatBytes, hexToBytes, randomBytes, utf8ToBytes } from "@noble/hashes/utils.js";
import {
  encodeFrame,
  parseFrame,
  type EncFrame,
  type E2eAckFrame,
  type E2eHelloFrame,
  type InboundFrame,
  type OutboundFrame,
} from "./protocol";

/** HKDF info（协议常量，Rust 侧一致）。 */
export const E2E_INFO = "imagent-e2e-v1";

/** pin 表存储位置（machine → {ik, gk, pinnedAt}）。 */
export const PIN_KEY = "imagent.e2ePins";

/** 「仍以明文连接」显式选择记录（machine → true）。 */
export const PLAINTEXT_OPT_KEY = "imagent.e2ePlaintext";

/** 握手应答超时（旧网关不回 e2e_ack → 判定不支持加密）。 */
export const ACK_TIMEOUT_MS = 10_000;

/** 出站随机数长度（XChaCha20 nonce = 24B）。 */
const NONCE_LEN = 24;

/** 需要走 e2e 包装的出站业务帧类型（其余管理帧恒明文）。v1.6 工作空间四帧
 *  （workspace_list/workspace_save/workspace_remove/conv_workdir_set）同为
 *  网关消费的业务帧，relay 不可见。 */
const BUSINESS_TYPES: ReadonlySet<string> = new Set([
  "message",
  "approval_response",
  "control",
  "sync",
  "ping",
  "workspace_list",
  "workspace_save",
  "workspace_remove",
  "conv_workdir_set",
]);

// ---------------------------------------------------------------------------
// 二进制编解码（hex 规范出站 / 入站宽容 base64）
// ---------------------------------------------------------------------------

const HEX_RE = /^[0-9a-f]+$/i;

function bytesToB64(bytes: Uint8Array): string {
  // 分块 String.fromCharCode 避免 apply 栈溢出（box 可达数十 KB）
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}

function b64ToBytes(s: string): Uint8Array | null {
  try {
    // base64url 宽容（-/ → +/、补齐 padding）——跨实现编码差异兜底
    let norm = s.replace(/-/g, "+").replace(/_/g, "/");
    while (norm.length % 4 !== 0) {
      norm += "=";
    }
    const bin = atob(norm);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) {
      out[i] = bin.charCodeAt(i);
    }
    return out;
  } catch {
    return null;
  }
}

/** 入站二进制解码：先 hex（字符集严格 + 长度匹配），后 base64（长度匹配）。
 *  两者都不符 → null（调用方按协议违规丢弃）。 */
function decodeBinary(s: string, expectBytes: number): Uint8Array | null {
  if (s.length % 2 === 0 && s.length === expectBytes * 2 && HEX_RE.test(s)) {
    return hexToBytes(s);
  }
  const b = b64ToBytes(s);
  return b !== null && b.length === expectBytes ? b : null;
}

function eqBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a[i] ^ b[i];
  }
  return diff === 0;
}

// ---------------------------------------------------------------------------
// 指纹（SHA-256 前 16 hex，4 组空格分隔：aaaa bbbb cccc dddd）
// ---------------------------------------------------------------------------

/** 公钥指纹：SHA-256(公钥字节) 前 16 hex 分组展示（弹层/设备管理/终端日志
 *  核对用——与 Rust 侧日志展示约定一致：前 16 hex、4 位一组）。畸形输入
 *  （空串/非 hex/奇长）→ 空串，不抛。 */
export function fingerprint(pubHex: string): string {
  if (pubHex.length === 0 || pubHex.length % 2 !== 0 || !HEX_RE.test(pubHex)) {
    return "";
  }
  let bytes: Uint8Array;
  try {
    bytes = hexToBytes(pubHex);
  } catch {
    return "";
  }
  const h = bytesToHex(sha256(bytes)).slice(0, 16);
  return h.replace(/(.{4})(?=.)/g, "$1 ");
}

// ---------------------------------------------------------------------------
// pin 表 / 明文选择（localStorage）
// ---------------------------------------------------------------------------

/** 已 pin 机器条目（ik = ed25519 身份公钥 hex，gk = X25519 公钥 hex）。 */
export type E2ePin = { machine: string; ik: string; gk: string; pinnedAt: number };

/** localStorage 形状（machine → 条目，machine 提到条目外便于列表渲染）。 */
type StoredPin = Omit<E2ePin, "machine">;

export function loadPins(): E2ePin[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(localStorage.getItem(PIN_KEY) ?? "{}");
  } catch {
    return [];
  }
  if (typeof parsed !== "object" || parsed === null) {
    return [];
  }
  const out: E2ePin[] = [];
  for (const [machine, v] of Object.entries(parsed as Record<string, unknown>)) {
    if (typeof v !== "object" || v === null) {
      continue;
    }
    const r = v as Record<string, unknown>;
    if (typeof r.ik === "string" && typeof r.gk === "string") {
      out.push({
        machine,
        ik: r.ik,
        gk: r.gk,
        pinnedAt: typeof r.pinnedAt === "number" ? r.pinnedAt : 0,
      });
    }
  }
  return out;
}

function pinOf(machine: string): StoredPin | null {
  return loadPins().find((p) => p.machine === machine) ?? null;
}

/** TOFU 确认后 pin（覆盖旧条目——仅显式重置 pin 流程会先 remove）。 */
export function pinMachine(machine: string, ik: string, gk: string): void {
  const all = Object.fromEntries(loadPins().map(({ machine: m, ...rest }) => [m, rest]));
  all[machine] = { ik, gk, pinnedAt: Date.now() };
  localStorage.setItem(PIN_KEY, JSON.stringify(all));
}

export function removePin(machine: string): void {
  const all = Object.fromEntries(loadPins().map(({ machine: m, ...rest }) => [m, rest]));
  delete all[machine];
  localStorage.setItem(PIN_KEY, JSON.stringify(all));
}

/** 该机器是否被用户显式选择「仍以明文连接」（跨连接持久）。 */
export function plaintextOpted(machine: string): boolean {
  try {
    const parsed = JSON.parse(localStorage.getItem(PLAINTEXT_OPT_KEY) ?? "{}");
    return Boolean((parsed as Record<string, unknown>)[machine]);
  } catch {
    return false;
  }
}

function persistPlaintextOpt(machine: string): void {
  let current: Record<string, boolean> = {};
  try {
    current = JSON.parse(localStorage.getItem(PLAINTEXT_OPT_KEY) ?? "{}");
  } catch {
    current = {};
  }
  if (typeof current !== "object" || current === null) {
    current = {};
  }
  current[machine] = true;
  localStorage.setItem(PLAINTEXT_OPT_KEY, JSON.stringify(current));
}

// ---------------------------------------------------------------------------
// 握手纯函数（可测：随机数可注入）
// ---------------------------------------------------------------------------

/** 浏览器侧握手秘密（ek 私钥只存在于本连接内存，断线即弃）。 */
export interface HandshakeSecret {
  machine: string;
  ekPriv: Uint8Array;
  ek: string;
  n1: string;
}

export type Rng = (n: number) => Uint8Array;

/** 发起握手：生成临时密钥对 + n1，产出出站 e2e_hello 帧。opts.sid 携带
 *  会话 id（定向加密形态；缺席 = 网关广播兼容模式）。 */
export function startHandshake(
  machine: string,
  opts?: { sid?: string; rng?: Rng },
): {
  secret: HandshakeSecret;
  hello: E2eHelloFrame;
} {
  const rng = opts?.rng ?? randomBytes;
  const ekPriv = x25519.utils.randomSecretKey();
  const ek = bytesToHex(x25519.getPublicKey(ekPriv));
  const n1 = bytesToHex(rng(32));
  return {
    secret: { machine, ekPriv, ek, n1 },
    hello: { v: 1, type: "e2e_hello", machine, ek, n1, sid: opts?.sid },
  };
}

/** sig 签名消息：bytes(ek) || bytes(gk) || bytes(n1) || bytes(n2)（96B）。 */
export function signatureMessage(ek: string, gk: string, n1: string, n2: string): Uint8Array {
  return concatBytes(
    hexToBytes(ek),
    hexToBytes(gk),
    hexToBytes(n1),
    hexToBytes(n2),
  );
}

/** 会话密钥派生（协议向量注释：ikm = X25519 共享密钥 32B + n1 32B + n2 32B，
 *  共 96B；salt 空；info "imagent-e2e-v1"；SHA-256；输出 32B。测试中网关侧
 *  以 X25519(gk_priv, ek_pub) 独立构造同一 ikm 验证两侧一致）。 */
export function deriveSessionKey(ekPriv: Uint8Array, gk: string, n1: string, n2: string): Uint8Array {
  const shared = x25519.getSharedSecret(ekPriv, hexToBytes(gk));
  return hkdf(
    sha256,
    concatBytes(shared, hexToBytes(n1), hexToBytes(n2)),
    undefined,
    utf8ToBytes(E2E_INFO),
    32,
  );
}

/** processAck 结果（invalid = 编码/形状/自洽性违规——非安全事件，丢弃等超时）。 */
export type AckOutcome =
  | { kind: "active"; key: Uint8Array; ik: string; gk: string }
  | {
      kind: "tofu";
      key: Uint8Array;
      ik: string;
      gk: string;
      gkFp: string;
      idFp: string;
    }
  | { kind: "alert"; reason: "identity_changed" | "bad_signature" }
  | { kind: "invalid" };

/** 验签 + 派生。已 pin：sig 必须对 pin 的 ik 验证通过、且 gk 与 pin 一致，
 *  否则 alert（identity_changed = 公钥变化 / bad_signature = 验签失败）。
 *  未 pin：sig 对随帧 ik 自洽验证（TOFU 信任锚是用户核对终端日志，非本帧）。 */
export function processAck(
  secret: HandshakeSecret,
  ack: E2eAckFrame,
): AckOutcome {
  const gk = decodeBinary(ack.gk, 32);
  const n2 = decodeBinary(ack.n2, 32);
  const sig = decodeBinary(ack.sig, 64);
  const ikRaw = ack.ik ?? ack.id ?? ack.pk;
  const ikBytes = ikRaw !== undefined ? decodeBinary(ikRaw, 32) : null;
  if (!gk || !n2 || !sig || !ikBytes) {
    return { kind: "invalid" };
  }
  const ik = bytesToHex(ikBytes);
  const msg = concatBytes(hexToBytes(secret.ek), gk, hexToBytes(secret.n1), n2);

  const pin = pinOf(secret.machine);
  if (pin) {
    const pinnedIk = decodeBinary(pin.ik, 32);
    const pinnedGk = decodeBinary(pin.gk, 32);
    if (!pinnedIk || !pinnedGk) {
      // pin 条目损坏：视为公钥变化（用户需显式重置重新 TOFU）
      return { kind: "alert", reason: "identity_changed" };
    }
    if (!eqBytes(gk, pinnedGk) || !eqBytes(ikBytes, pinnedIk)) {
      return { kind: "alert", reason: "identity_changed" };
    }
    if (!ed25519.verify(sig, msg, pinnedIk)) {
      return { kind: "alert", reason: "bad_signature" };
    }
    return {
      kind: "active",
      key: deriveSessionKey(secret.ekPriv, bytesToHex(gk), secret.n1, bytesToHex(n2)),
      ik,
      gk: bytesToHex(gk),
    };
  }

  // 未 pin：自洽验签（失败 = 协议违规而非身份事件）
  if (!ed25519.verify(sig, msg, ikBytes)) {
    return { kind: "invalid" };
  }
  const gkHex = bytesToHex(gk);
  return {
    kind: "tofu",
    key: deriveSessionKey(secret.ekPriv, gkHex, secret.n1, bytesToHex(n2)),
    ik,
    gk: gkHex,
    gkFp: fingerprint(gkHex),
    idFp: fingerprint(ik),
  };
}

// ---------------------------------------------------------------------------
// 会话加解密（AEAD 包装）
// ---------------------------------------------------------------------------

/** 出站包装：box = base64(nonce24 || XChaCha20-Poly1305(原帧 JSON))。
 *  frame 覆盖双向帧型（网关侧加密的是 text/card 等入站型帧——测试模拟用）。 */
export function sealFrame(
  machine: string,
  key: Uint8Array,
  frame: OutboundFrame | InboundFrame,
  rng: Rng = randomBytes,
): EncFrame {
  const nonce = rng(NONCE_LEN);
  const ct = xchacha20poly1305(key, nonce).encrypt(utf8ToBytes(JSON.stringify(frame)));
  return { v: 1, type: "enc", machine, box: bytesToB64(concatBytes(nonce, ct)) };
}

/** 入站解密：base64 优先、hex 回退（AEAD 认证失败即编码不符），任何失败 → null。 */
export function openBox(key: Uint8Array, box: string): string | null {
  const tryDecrypt = (blob: Uint8Array): string | null => {
    if (blob.length < NONCE_LEN + 16) {
      return null;
    }
    try {
      const pt = xchacha20poly1305(key, blob.subarray(0, NONCE_LEN)).decrypt(
        blob.subarray(NONCE_LEN),
      );
      return new TextDecoder().decode(pt);
    } catch {
      return null;
    }
  };
  const b = b64ToBytes(box);
  if (b) {
    const opened = tryDecrypt(b);
    if (opened !== null) {
      return opened;
    }
  }
  if (box.length % 2 === 0 && HEX_RE.test(box)) {
    try {
      return tryDecrypt(hexToBytes(box));
    } catch {
      return null;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// 协调器：逐机器握手状态机（ws 层接线；纯逻辑，定时器走标准 setTimeout）
// ---------------------------------------------------------------------------

export type E2eMachineStatus = "pending" | "confirming" | "active" | "unsupported" | "alert";

/** 上抛 UI 事件。 */
export type E2eEvent =
  | { kind: "tofu"; machine: string; gkFp: string; idFp: string }
  | { kind: "active"; machine: string }
  | { kind: "unsupported"; machine: string; reason: "timeout" | "declined" }
  | { kind: "alert"; machine: string; reason: "identity_changed" | "bad_signature" }
  | { kind: "plaintext"; machine: string }
  /** active 机器收到明文业务帧（ws 层已丢弃）：链路可能被降级（恶意中继吞
   *  握手帧 → 网关半开会话超时回退明文出站——浏览器侧唯一可观测点）。 */
  | { kind: "downgrade"; machine: string };

interface E2eEntry {
  status: E2eMachineStatus;
  secret?: HandshakeSecret;
  key?: Uint8Array;
  ik?: string;
  gk?: string;
  gkFp?: string;
  idFp?: string;
  /** unsupported + 用户显式选择明文 → 发送放行（明文）。 */
  plaintext?: boolean;
  /** 降级告警已上抛（每连接每机器一次，防事件风暴）。 */
  downgradeWarned?: boolean;
  timer?: ReturnType<typeof setTimeout>;
}

export interface CoordinatorHooks {
  /** 发出 e2e_hello（ws 层 rawSend；连接未就绪时静默）。 */
  sendHello: (hello: E2eHelloFrame) => void;
  /** 上抛 UI 事件（TOFU 弹层 / 告警横幅 / 状态指示）。 */
  emit: (ev: E2eEvent) => void;
}

/**
 * 逐机器 e2e 状态机：
 * - onMachines：relay 机器快照 → 为无条目的在线机器发起握手（用户已显式
 *   选择明文的机器跳过）。
 * - onAck：pending 态收到 ack → active / tofu(confirming) / alert。
 * - 出站：outbound() 给出线上的帧（active → enc 包装；pending/无条目/明文
 *   选择 → 原帧明文；confirming/unsupported 未选择/alert → null 阻断）。
 * - 入站：openEnc() 解密二次分发；acceptPlaintext() 判定明文业务帧放行
 *   （active/alert 态收明文业务帧 = 注入面，丢弃）。
 * - reset()：连接断开清空全部条目（临时密钥绑定本连接；pin/明文选择持久）。
 */
export class E2eCoordinator {
  private entries = new Map<string, E2eEntry>();
  private readonly hooks: CoordinatorHooks;
  private readonly ackTimeoutMs: number;
  private readonly rng: Rng;
  private readonly getSid: (() => string | undefined) | undefined;

  constructor(
    hooks: CoordinatorHooks,
    opts?: { ackTimeoutMs?: number; rng?: Rng; getSid?: () => string | undefined },
  ) {
    this.hooks = hooks;
    this.ackTimeoutMs = opts?.ackTimeoutMs ?? ACK_TIMEOUT_MS;
    this.rng = opts?.rng ?? randomBytes;
    this.getSid = opts?.getSid;
  }

  statusOf(machine: string): E2eMachineStatus | "none" {
    return this.entries.get(machine)?.status ?? "none";
  }

  /** 出站帧线上形态：null = 阻断（该机器处于 fail-closed 态且用户未放行）。 */
  outbound(machine: string | undefined, frame: OutboundFrame): string | null {
    if (!BUSINESS_TYPES.has(frame.type)) {
      return encodeFrame(frame); // 管理帧恒明文（device_*/cred_*/hello/e2e_hello/enc 不会到此）
    }
    if (machine === undefined) {
      return encodeFrame(frame); // 旧 relay 单机模式（无 machines 帧，条目恒空）
    }
    const e = this.entries.get(machine);
    if (!e || e.status === "pending" || e.plaintext) {
      // 未握手/握手中的短暂窗口（v1.1 行为）/用户显式选择明文的机器
      return encodeFrame(frame);
    }
    if (e.status === "active" && e.key) {
      return encodeFrame(sealFrame(machine, e.key, frame, this.rng));
    }
    return null; // confirming（待用户确认指纹）/ unsupported 未放行 / alert
  }

  /** 内部保活 ping 的线上形态：active → enc，其余恒明文（保命线不得阻断）。 */
  heartbeat(frame: OutboundFrame, machine: string | undefined): string {
    if (machine !== undefined) {
      const e = this.entries.get(machine);
      if (e?.status === "active" && e.key) {
        return encodeFrame(sealFrame(machine, e.key, frame, this.rng));
      }
    }
    return encodeFrame(frame);
  }

  /** 入站 enc 帧 → 解密后的内帧（二次分发）；无会话/失败 → null（丢弃）。 */
  openEnc(frame: EncFrame): InboundFrame | null {
    const e = this.entries.get(frame.machine);
    if (!e?.key || (e.status !== "active" && e.status !== "confirming")) {
      return null; // 无会话 / alert / unsupported：enc 帧不可信亦不可解
    }
    const json = openBox(e.key, frame.box);
    if (json === null) {
      return null;
    }
    return parseFrame(json);
  }

  /** 入站明文业务帧放行策略：active（会话已立，明文=注入面）与 alert
   *  （身份存疑）丢弃；pending/confirming/unsupported/无条目放行（旧网关
   *  兼容 + ack 在途竞态容错）。 */
  acceptPlaintextBusiness(machine: string): boolean {
    const e = this.entries.get(machine);
    if (!e) {
      return true;
    }
    return e.status === "pending" || e.status === "confirming" || e.status === "unsupported";
  }

  /** active 机器的明文业务帧被丢弃时（ws 层调用）：上抛降级告警（每连接每
   *  机器一次）。恶意中继吞握手帧可让网关半开会话超时后回退明文出站——
   *  浏览器侧此告警是该降级的唯一可观测点。 */
  notePlaintextDowngrade(machine: string): void {
    const e = this.entries.get(machine);
    if (!e || e.status !== "active" || e.downgradeWarned) {
      return;
    }
    e.downgradeWarned = true;
    this.hooks.emit({ kind: "downgrade", machine });
  }

  /** relay 机器快照：为无条目的在线机器发起握手。 */
  onMachines(online: string[]): void {
    for (const m of online) {
      if (this.entries.has(m) || plaintextOpted(m)) {
        continue;
      }
      this.beginHandshake(m);
    }
  }

  /** 收到 e2e_ack：仅 pending 态受理（非恳求 ack / 重复 ack 忽略）。 */
  onAck(ack: E2eAckFrame): void {
    const e = this.entries.get(ack.machine);
    if (!e || e.status !== "pending" || !e.secret) {
      return;
    }
    const outcome = processAck(e.secret, ack);
    if (outcome.kind === "invalid") {
      return; // 丢弃等超时（fail-safe：不因单个坏帧误判支持/不支持）
    }
    clearTimeout(e.timer);
    delete e.timer;
    delete e.secret;
    if (outcome.kind === "alert") {
      e.status = "alert";
      this.hooks.emit({ kind: "alert", machine: ack.machine, reason: outcome.reason });
      return;
    }
    if (outcome.kind === "active") {
      e.status = "active";
      e.key = outcome.key;
      this.hooks.emit({ kind: "active", machine: ack.machine });
      return;
    }
    // tofu：密钥先行持有（网关 ack 后立即加密出站，confirming 态可解），
    // 指纹确认后才 pin + 激活。
    e.status = "confirming";
    e.key = outcome.key;
    e.ik = outcome.ik;
    e.gk = outcome.gk;
    e.gkFp = outcome.gkFp;
    e.idFp = outcome.idFp;
    this.hooks.emit({ kind: "tofu", machine: ack.machine, gkFp: outcome.gkFp, idFp: outcome.idFp });
  }

  /** TOFU 弹层决断：accept → pin + 激活；decline → unsupported（可显式明文）。 */
  resolveTofu(machine: string, accept: boolean): void {
    const e = this.entries.get(machine);
    if (!e || e.status !== "confirming" || !e.ik || !e.gk || !e.key) {
      return;
    }
    if (accept) {
      pinMachine(machine, e.ik, e.gk);
      e.status = "active";
      this.hooks.emit({ kind: "active", machine });
      return;
    }
    delete e.key;
    delete e.ik;
    delete e.gk;
    e.status = "unsupported";
    this.hooks.emit({ kind: "unsupported", machine, reason: "declined" });
  }

  /** 「仍以明文连接」：放行发送 + 持久记录（此后该机器不再尝试握手）。 */
  allowPlaintext(machine: string): void {
    const e = this.entries.get(machine);
    if (e) {
      e.status = "unsupported";
      e.plaintext = true;
      delete e.key;
      delete e.secret;
      if (e.timer) {
        clearTimeout(e.timer);
        delete e.timer;
      }
    }
    persistPlaintextOpt(machine);
    this.hooks.emit({ kind: "plaintext", machine });
  }

  /** pin 重置后重试：丢弃条目（含明文选择）重新握手。 */
  retry(machine: string): void {
    this.dropEntry(machine);
    this.beginHandshake(machine);
  }

  /** 连接断开：清空全部条目与定时器（pin/明文选择持久，不受影响）。 */
  reset(): void {
    for (const e of this.entries.values()) {
      if (e.timer) {
        clearTimeout(e.timer);
      }
    }
    this.entries.clear();
  }

  private beginHandshake(machine: string): void {
    const { secret, hello } = startHandshake(machine, { sid: this.getSid?.(), rng: this.rng });
    const entry: E2eEntry = { status: "pending", secret };
    entry.timer = setTimeout(() => {
      const cur = this.entries.get(machine);
      if (cur !== entry || cur.status !== "pending") {
        return;
      }
      delete cur.secret;
      cur.status = "unsupported";
      this.hooks.emit({ kind: "unsupported", machine, reason: "timeout" });
    }, this.ackTimeoutMs);
    this.entries.set(machine, entry);
    this.hooks.sendHello(hello);
  }

  private dropEntry(machine: string): void {
    const e = this.entries.get(machine);
    if (e?.timer) {
      clearTimeout(e.timer);
    }
    this.entries.delete(machine);
  }
}
