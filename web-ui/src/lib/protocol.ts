// 帧协议 v1.2（事实源：crates/web/src/lib.rs + crates/relay/src/lib.rs）。
// 信封 `{"v":1,"type":...}`，全部 JSON 文本帧。TS 类型与 Rust 侧逐字段对齐：
// - convId 恒为完整 conv 形态（`web:<session>`）；裸 session 名仅网关侧兼容输入，
//   SPA 回执帧（message / approval_response）一律原样回传收到的 convId。
//   例外即透传：`local:<会话名>` 是网关侧本地终端会话（crates/localterm）的
//   既有 conv 形态——MultiPlatform 镜像路由把 local 会话出站同步给 web 平台，
//   SPA 收到即收集为侧栏会话；对 local 会话的干预消息/审批回执原样携带
//   local: convId（web 出生才缺省 web: 前缀，网关 normalize_conv 对 local:
//   前缀透传放行），SPA 侧不对其做任何前缀改写。
// - CardPayload 可选字段对齐 Rust `skip_serializing_if = "Option::is_none"`
//   （缺席即未序列化）。
// - v1.1 多终端：浏览器→网关帧可选带 `machine`（目标机器）；网关→浏览器帧
//   可选带 `machine`（来源机器，旧网关缺席 → SPA 归 DEFAULT_MACHINE 单机兼容）；
//   relay→浏览器新增 `machines` 帧（全量在线机器快照）。
// - v1.2 会话层：首次登录 hello 带 `token`+可选 `device` → hello_ack 携带
//   `session:{id,token,expiresAt}`（USER_TOKEN 一次性，之后重连 hello 带
//   `session`）；设备管理 `device_list`/`device_revoke` ↔ `devices` 帧。
//   hello_ack 无 session 字段 = 旧 relay → SPA 回退现行 USER_TOKEN 直连模式。
// - v1.3 passkey：HTTP 端点 `/webauthn/{register,login}/{begin,complete}`
//   （lib/webauthn.ts，不走 WS）；凭证管理 `cred_list`/`cred_revoke` ↔
//   `creds` 帧（transport 管理帧，同 devices 族）。
// - v1.4 E2E 中继盲化：浏览器 ↔ 网关逐机器握手（e2e_hello/e2e_ack，X25519 +
//   HKDF-SHA256 + 网关 ed25519 签名，密码学与状态机在 lib/e2e.ts）；握手完成
//   的机器业务帧（message/approval_response/control/sync/ping）包成 enc 帧
//   （XChaCha20-Poly1305，box 内含原帧 JSON，无需 sid）。relay 自产管理帧
//   （hello_ack/machines/machine/devices/creds/error/pong）恒明文。

export const PROTOCOL_VERSION = 1;

/** 默认会话（网关 normalize_conv(None, "default") 的落点），恒存在。 */
export const DEFAULT_CONV = "web:default";

/** 缺省机器 id：帧不带 machine 字段（旧网关）时的归属；也是未收到 machines
 *  帧（旧 relay）单机模式下 currentMachine 的初值。 */
export const DEFAULT_MACHINE = "default";

/** conv 显示名：剥 `web:` 前缀（仅展示用，协议回传仍用完整 conv）。 */
export function convDisplayName(convId: string): string {
  return convId.startsWith("web:") ? convId.slice("web:".length) : convId;
}

// ---------------------------------------------------------------------------
// 卡片载荷（crates/web/src/lib.rs CardPayload）
// ---------------------------------------------------------------------------

export type TodoStatus = "pending" | "in_progress" | "completed";

/** 任务清单项（TodoPayload）。 */
export type TodoItem = {
  id?: string;
  text: string;
  status: TodoStatus;
};

/** 单条工具调用记录（ToolCallPayload）。M3a 保留在 state，M3b 渲染。 */
export type ToolCallPayload = {
  name: string;
  summary: string;
  done: boolean;
  id?: string;
};

export type CardPhase = "thinking" | "tool_running" | "outputting" | "waiting_approval";
export type CardTerminal = "running" | "done" | "error";

export type CardPayload = {
  text: string;
  tool_calls: ToolCallPayload[];
  thoughts: string[];
  todos: TodoItem[];
  phase: CardPhase;
  queued_hint?: string;
  run_secs: number;
  usage_display?: string;
  task_digest?: string;
  terminal: CardTerminal;
  error?: string;
};

/** card / card_patch 帧的 state：streaming = terminal Running，done = 终态。 */
export type CardFrameState = "streaming" | "done";

// ---------------------------------------------------------------------------
// v1.2 会话层
// ---------------------------------------------------------------------------

/** relay 签发的会话凭证（hello_ack.session）：30 天有效期，重连 hello 用
 *  session.token 替代 USER_TOKEN。expiresAt 约定毫秒 epoch（秒级经
 *  normalizeExpiryMs 容错放大）。 */
export type SessionInfo = { id: string; token: string; expiresAt: number };

/** expiresAt 容错归一毫秒 epoch：秒级时间戳（< 1e12）自动 ×1000。 */
export function normalizeExpiryMs(expiresAt: number): number {
  return expiresAt < 1e12 ? expiresAt * 1000 : expiresAt;
}

/** UA → 设备短名（登录缺省设备名，如「iPhone/Safari」「Mac/Chrome」）。
 *  纯函数便于单测：OS 取 iPhone/iPad/Android/Mac/Windows/Linux 首个命中；
 *  浏览器判定顺序敏感——Edge/Opera 的 UA 同时含 Chrome/ 与 Safari/，须先
 *  判衍生再判本体；两者均未命中给兜底短名。 */
export function deviceSummary(ua: string): string {
  const os = ua.includes("iPhone")
    ? "iPhone"
    : ua.includes("iPad")
      ? "iPad"
      : ua.includes("Android")
        ? "Android"
        : ua.includes("Macintosh") || ua.includes("Mac OS X")
          ? "Mac"
          : ua.includes("Windows")
            ? "Windows"
            : ua.includes("Linux")
              ? "Linux"
              : undefined;
  const browser = /Edg(e)?\//.test(ua)
    ? "Edge"
    : ua.includes("OPR/") || ua.includes("Opera")
      ? "Opera"
      : ua.includes("Firefox/")
        ? "Firefox"
        : ua.includes("Chrome/")
          ? "Chrome"
          : ua.includes("Safari/")
            ? "Safari"
            : undefined;
  if (os && browser) {
    return `${os}/${browser}`;
  }
  return os ?? browser ?? "未知设备";
}

// ---------------------------------------------------------------------------
// 出站帧（浏览器 → 网关/relay）
// ---------------------------------------------------------------------------

/** hello 双形态（v1.2）：首次登录带 token（+可选 device）；重连带 session
 *  （session 由新 relay 签发，只会发往新 relay；旧 relay 持有 session 场景
 *  不存在——token 缺失直接 1008 回登录页）。 */
export type HelloFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "hello";
  /** 首次登录：USER_TOKEN（一次性，签发 session 后 SPA 即删除）。 */
  token?: string;
  /** 重连：会话凭证。 */
  session?: string;
  /** 首次登录可选设备名（缺省 deviceSummary(navigator.userAgent)）。 */
  device?: string;
};

export type MessageFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "message";
  /** 完整 conv（如 web:default）；缺省落网关默认会话。 */
  convId?: string;
  text: string;
  /** 目标机器（v1.1）：多机在线时未指定会被 relay 回 ambiguous_machine，
   *  故多机模式下 SPA 必须恒带当前选中机器；单机模式（旧 relay）不带。 */
  machine?: string;
};

export type ApprovalDecision = "allow" | "deny" | "always";

/** convId / requestId 必须原样回传 approval_request 帧里的值。 */
export type ApprovalResponseFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "approval_response";
  convId: string;
  requestId: string;
  decision: ApprovalDecision;
  /** 目标机器（v1.1）：审批来自哪台机器就回哪台（非当前选中机器的审批
   *  也要能批）。 */
  machine?: string;
};

export type ControlFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "control";
  cmd: "stop" | "resume" | "switch";
  args?: string;
  machine?: string;
};

export type SyncFrame = { v: typeof PROTOCOL_VERSION; type: "sync"; machine?: string };
export type PingFrame = { v: typeof PROTOCOL_VERSION; type: "ping"; machine?: string };

/** v1.2 设备管理：请求已登录设备列表。 */
export type DeviceListFrame = { v: typeof PROTOCOL_VERSION; type: "device_list" };

/** v1.2 设备管理：吊销设备（deviceId = devices 帧里的 id）。 */
export type DeviceRevokeFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "device_revoke";
  deviceId: string;
};

/** v1.3 通行钥匙管理：请求已注册 passkey 列表。 */
export type CredListFrame = { v: typeof PROTOCOL_VERSION; type: "cred_list" };

/** v1.3 通行钥匙管理：删除（credId = creds 帧里的 id）。 */
export type CredRevokeFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "cred_revoke";
  credId: string;
};

// ---------------------------------------------------------------------------
// v1.4 E2E 中继盲化（密码学构造与编码规约见 lib/e2e.ts 头注）
// ---------------------------------------------------------------------------

/** 握手第一步（浏览器 → 网关，经 relay 明文透传）：ek = 浏览器临时 X25519
 *  公钥（32B hex），n1 = 浏览器随机数（32B hex）。sid = 会话 id
 *  （hello_ack.session.id）：携带即定向加密形态（网关 enc{sid} 帧只投对应
 *  连接）；缺席 = 广播兼容模式（多浏览器同 token 时最后握手者有效）。 */
export type E2eHelloFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "e2e_hello";
  machine: string;
  ek: string;
  n1: string;
  sid?: string;
};

/** 加密业务帧（双向）：box = base64(nonce24 || XChaCha20-Poly1305(原帧 JSON))，
 *  machine = 会话所属机器（解密方据此选会话密钥；无需 sid——浏览器单会话）。 */
export type EncFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "enc";
  machine: string;
  box: string;
};

export type OutboundFrame =
  | HelloFrame
  | MessageFrame
  | ApprovalResponseFrame
  | ControlFrame
  | SyncFrame
  | PingFrame
  | DeviceListFrame
  | DeviceRevokeFrame
  | CredListFrame
  | CredRevokeFrame
  | E2eHelloFrame
  | EncFrame;

/** 出站帧序列化（undefined 字段不序列化，与网关入站解析兼容）。 */
export function encodeFrame(frame: OutboundFrame): string {
  return JSON.stringify(frame);
}

// ---------------------------------------------------------------------------
// 入站帧（网关/relay → 浏览器）
// ---------------------------------------------------------------------------

/** v1.2：hello_ack 可选携带 session（旧 relay 缺席 → SPA 回退 USER_TOKEN
 *  直连模式，行为与 v1.1 完全一致）。 */
export type HelloAckFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "hello_ack";
  role: string;
  session?: SessionInfo;
};

export type MachineFrame = { v: typeof PROTOCOL_VERSION; type: "machine"; online: boolean };

/** v1.1：relay→浏览器全量在线机器快照（连接后推一次，此后任一网关上下线再推）。 */
export type MachinesFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "machines";
  online: string[];
};

export type ErrorFrame = { v: typeof PROTOCOL_VERSION; type: "error"; code: string };

export type TextFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "text";
  convId: string;
  text: string;
  /** 协议预留位（web 侧恒不发，类型保留）。 */
  hint?: string;
  /** 来源机器（v1.1）；旧网关缺席 → 归 DEFAULT_MACHINE。 */
  machine?: string;
};

/** card（新建/sync 重发）与 card_patch（更新）同形，仅 type 不同。 */
export type CardFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "card" | "card_patch";
  convId: string;
  /** 同 conv 内卡片的幂等 key：同 messageId 覆盖（整卡替换语义，非 diff）。 */
  messageId: string;
  card: CardPayload;
  state: CardFrameState;
  machine?: string;
};

export type ApprovalRequestFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "approval_request";
  convId: string;
  requestId: string;
  tool: string;
  inputSummary: string;
  machine?: string;
};

export type ResolvedDecision = ApprovalDecision | "cancelled";

export type ApprovalResolvedFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "approval_resolved";
  convId: string;
  requestId: string;
  decision: ResolvedDecision;
  message?: string;
  machine?: string;
};

export type PongFrame = { v: typeof PROTOCOL_VERSION; type: "pong" };

/** v1.2 已登录设备项（devices 帧）。lastSeen/created 容错为可选（缺席仅
 *  影响展示，不影响协议可用性）。 */
export type DeviceInfo = {
  id: string;
  name: string;
  connected: boolean;
  lastSeen?: number;
  created?: number;
};

/** v1.2：relay→浏览器设备列表快照（device_list 请求 / device_revoke 后回）。 */
export type DevicesFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "devices";
  devices: DeviceInfo[];
};

/** v1.3 已注册通行钥匙项（creds 帧）。created 为 unix 秒（缺席仅影响展示）。 */
export type PasskeyInfo = { id: string; label: string; created?: number };

/** v1.3：relay→浏览器通行钥匙列表快照（cred_list 请求 / cred_revoke 后回）。 */
export type CredsFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "creds";
  credentials: PasskeyInfo[];
};

/** v1.4 握手第二步（网关 → 浏览器，经 relay 明文透传）：gk = 网关 X25519
 *  公钥（32B hex），n2 = 网关随机数（32B hex），sig = 网关 ed25519 对
 *  bytes(ek)||bytes(gk)||bytes(n1)||bytes(n2) 的签名（64B hex），ik = 网关
 *  ed25519 身份公钥（32B hex——首次连接（未 pin）无信任锚，必须随帧携带供
 *  TOFU 确认；id/pk 为宽容别名，解析侧兼容）。 */
export type E2eAckFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "e2e_ack";
  machine: string;
  gk: string;
  n2: string;
  sig: string;
  ik?: string;
  /** 宽容别名（跨实现字段命名差异）：ik/id/pk 任一存在即接受，e2e 层归一。 */
  id?: string;
  pk?: string;
};

export type InboundFrame =
  | HelloAckFrame
  | MachineFrame
  | MachinesFrame
  | ErrorFrame
  | TextFrame
  | CardFrame
  | ApprovalRequestFrame
  | ApprovalResolvedFrame
  | PongFrame
  | DevicesFrame
  | CredsFrame
  | E2eAckFrame
  | EncFrame;

// ---------------------------------------------------------------------------
// parseFrame：v/type 校验 + 每类帧关键字段存在性校验，非法返回 null（不抛）
// ---------------------------------------------------------------------------

type FieldCheck = (o: Record<string, unknown>) => boolean;

const isStr = (o: Record<string, unknown>, k: string) => typeof o[k] === "string";

/** v1.1 可选 machine 字段：缺席或字符串才合法。 */
const machineOk = (o: Record<string, unknown>): boolean =>
  o.machine === undefined || typeof o.machine === "string";

const cardShapeOk = (o: Record<string, unknown>): boolean =>
  isStr(o, "convId") &&
  isStr(o, "messageId") &&
  typeof o.card === "object" &&
  o.card !== null &&
  isStr(o.card as Record<string, unknown>, "text");

/** v1.2 hello_ack.session：缺席（旧 relay）或 {id,token,expiresAt} 形状齐备。 */
const sessionOk = (o: Record<string, unknown>): boolean => {
  const s = o.session;
  if (s === undefined) {
    return true;
  }
  if (typeof s !== "object" || s === null) {
    return false;
  }
  const r = s as Record<string, unknown>;
  return isStr(r, "id") && isStr(r, "token") && typeof r.expiresAt === "number";
};

/** v1.2 devices 帧元素：id/name 字符串、connected 布尔；lastSeen/created
 *  可选但出现即须为数字（容错展示字段）。 */
const deviceOk = (d: unknown): boolean => {
  if (typeof d !== "object" || d === null) {
    return false;
  }
  const r = d as Record<string, unknown>;
  return (
    isStr(r, "id") &&
    isStr(r, "name") &&
    typeof r.connected === "boolean" &&
    (r.lastSeen === undefined || typeof r.lastSeen === "number") &&
    (r.created === undefined || typeof r.created === "number")
  );
};

/** v1.3 creds 帧元素：id/label 字符串；created 可选但出现即须为数字。 */
const passkeyOk = (p: unknown): boolean => {
  if (typeof p !== "object" || p === null) {
    return false;
  }
  const r = p as Record<string, unknown>;
  return isStr(r, "id") && isStr(r, "label") && (r.created === undefined || typeof r.created === "number");
};

const RESOLVED_DECISIONS: readonly string[] = ["allow", "deny", "always", "cancelled"];

/** v1.4 e2e_ack 的网关身份公钥字段：ik（规范名）/ id / pk（宽容别名）任一
 *  为字符串即可（首次连接必须携带，否则 TOFU 无从展示指纹）。 */
const ackIdentityOk = (o: Record<string, unknown>): boolean =>
  isStr(o, "ik") || isStr(o, "id") || isStr(o, "pk");

/** 入站帧类型表：type → 关键字段校验（前向兼容靠“未知 type → null”由调用方忽略）。 */
const INBOUND_CHECKS: Record<string, FieldCheck> = {
  machine: (o) => typeof o.online === "boolean",
  machines: (o) =>
    Array.isArray(o.online) && o.online.every((m) => typeof m === "string"),
  error: (o) => isStr(o, "code"),
  text: (o) => isStr(o, "convId") && isStr(o, "text") && machineOk(o),
  card: (o) => cardShapeOk(o) && machineOk(o),
  card_patch: (o) => cardShapeOk(o) && machineOk(o),
  approval_request: (o) =>
    isStr(o, "convId") && isStr(o, "requestId") && isStr(o, "tool") && isStr(o, "inputSummary") && machineOk(o),
  approval_resolved: (o) =>
    isStr(o, "convId") &&
    isStr(o, "requestId") &&
    typeof o.decision === "string" &&
    RESOLVED_DECISIONS.includes(o.decision) &&
    machineOk(o),
  pong: () => true,
  hello_ack: (o) => isStr(o, "role") && sessionOk(o),
  devices: (o) => Array.isArray(o.devices) && o.devices.every(deviceOk),
  creds: (o) => Array.isArray(o.credentials) && o.credentials.every(passkeyOk),
  // v1.4：字段级形状校验（编码/长度/签名校验在 lib/e2e.ts 深层完成）
  e2e_ack: (o) =>
    isStr(o, "machine") && isStr(o, "gk") && isStr(o, "n2") && isStr(o, "sig") && ackIdentityOk(o),
  enc: (o) => isStr(o, "machine") && isStr(o, "box"),
};

/** 解析一条入站文本帧：信封 v=1 + 已知 type + 关键字段齐备才放行，否则 null。 */
export function parseFrame(raw: string): InboundFrame | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) {
    return null;
  }
  const o = parsed as Record<string, unknown>;
  if (o.v !== PROTOCOL_VERSION || typeof o.type !== "string") {
    return null;
  }
  const check = INBOUND_CHECKS[o.type];
  if (!check || !check(o)) {
    return null;
  }
  return parsed as InboundFrame;
}

// ---------------------------------------------------------------------------
// 应用状态（reducer 归约，纯函数）
// ---------------------------------------------------------------------------

/** 消息流条目：按到达顺序渲染，三类 + 审批终态记录。user/text 带 at（到达
 *  时间戳，ms epoch，UI 显示 HH:MM；reducer 追加时统一盖戳）。 */
export type StreamEntry =
  | { kind: "user"; key: string; text: string; at?: number }
  | { kind: "text"; key: string; text: string; hint?: string; at?: number }
  | { kind: "card"; key: string; messageId: string; card: CardPayload; state: CardFrameState }
  | { kind: "approval_note"; key: string; text: string; message?: string };

export type CardEntry = Extract<StreamEntry, { kind: "card" }>;

/** pending 审批（弹层渲染；requestId 为幂等 key）。machine = 审批来源机器，
 *  回传 approval_response 时必须带同一 machine（relay 按机器路由）。 */
export type PendingApproval = {
  machine: string;
  convId: string;
  requestId: string;
  tool: string;
  inputSummary: string;
};

export type ConvState = {
  entries: StreamEntry[];
  pending: PendingApproval[];
  /** 侧边栏会话项预览：该会话最后一条消息的单行摘要（user/text 原文首行、card 正文首行）。 */
  lastPreview: string;
  /** 最后一条消息到达时间（Date.now() 毫秒，仅 UI 展示用）。 */
  lastAt: number;
  /** 未读计数（侧栏徽标）：非当前 conv 收到新消息（text 追加 / 已存在 conv 的新
   *  card 插入）+1，select_conv 点选清零。card_patch 替换与首次物化的 card 插入
   *  （sync 重放 / 新会话首卡）不计——防刷新/重连/切机后全部会话被误点亮。 */
  unread: number;
};

/** 单台机器的会话空间：会话表 + conv 插入序（web:default 恒为首项）。 */
export type MachineSpace = {
  convs: Record<string, ConvState>;
  convOrder: string[];
};

/** 消息条目内存护栏：单 conv 超过上限时丢弃最早的本地用户回显（ephemeral）；
 *  服务端条目（text/card）不丢——卡片终态可由 sync 重发兜底。 */
export const MAX_STREAM_ENTRIES = 200;

/** 一次性通知（机器自动切换等）：reducer 产出结构化事件，App 映射文案渲染
 *  toast 后发 clear_notice 清除。 */
export type MachineNotice =
  /** 当前机器不在 machines 列表 → 自动切到 to。 */
  | { kind: "switched"; from: string; to: string; at: number }
  /** 当前机器掉线且无任何在线机器可切。 */
  | { kind: "offline"; machine: string; at: number };

export type AppState = {
  /** key 生成序号（纯函数内单调递增）。 */
  seq: number;
  /** 每台机器一个会话空间（key = machine id；恒含 DEFAULT_MACHINE）。 */
  spaces: Record<string, MachineSpace>;
  /** 在线机器全量列表（machines 帧覆盖式维护）。 */
  machines: string[];
  /** 收到过 machines 帧 = 新 relay 多机模式；false = 旧 relay 单机兼容
   *  （出站帧不带 machine，不显示机器切换器）。 */
  machinesKnown: boolean;
  /** 当前选中机器（侧栏机器切换器 / machines 帧自动掉线切换维护）。 */
  currentMachine: string;
  /** 当前选中会话（侧栏点选 / 新会话创建维护，恒为完整 conv 形态）。未读计数
   *  的「当前 conv」判定依据（收到的帧落在当前 conv 不计未读）。 */
  currentConv: string;
  /** 旧网关 machine 帧（单机在线态）；多机模式以 machines 列表为准。 */
  machineOnline: boolean;
  /** v1.2 已登录设备列表（devices 帧）；null = 尚未拉取（设备面板未打开）。 */
  devices: DeviceInfo[] | null;
  /** v1.3 已注册通行钥匙列表（creds 帧）；null = 尚未拉取。 */
  passkeys: PasskeyInfo[] | null;
  lastError: { code: string; at: number } | null;
  lastNotice: MachineNotice | null;
};

/** 只读兜底空空间（未收到任何帧的机器），绝不写回 state。 */
const EMPTY_SPACE: MachineSpace = { convs: {}, convOrder: [] };

/** 只读访问某机器会话空间（不存在时返回空空间引用，不落 state）。 */
export function spaceOf(state: AppState, machine: string): MachineSpace {
  return state.spaces[machine] ?? EMPTY_SPACE;
}

/** 新机器空间：预置默认会话（与单机模式 initialState 行为一致）。 */
function newSpace(): MachineSpace {
  return {
    convs: { [DEFAULT_CONV]: { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 } },
    convOrder: [DEFAULT_CONV],
  };
}

export function initialState(): AppState {
  return {
    seq: 0,
    spaces: { [DEFAULT_MACHINE]: newSpace() },
    machines: [],
    machinesKnown: false,
    currentMachine: DEFAULT_MACHINE,
    currentConv: DEFAULT_CONV,
    machineOnline: false,
    devices: null,
    passkeys: null,
    lastError: null,
    lastNotice: null,
  };
}

export type Action =
  | { type: "frame"; frame: InboundFrame }
  | { type: "send_echo"; convId: string; text: string; machine?: string }
  | { type: "approval_sent"; convId: string; requestId: string; machine?: string }
  /** 用户在机器切换器点选机器（currentConv 同时回到该机器默认会话）。 */
  | { type: "select_machine"; machine: string }
  /** 用户点选会话（仅切换 SPA 本地当前 conv，不发包；清该 conv 未读）。 */
  | { type: "select_conv"; convId: string }
  | { type: "clear_error" }
  | { type: "clear_notice" };

function resolvedLabel(decision: ResolvedDecision): string {
  switch (decision) {
    case "allow":
      return "已批准";
    case "deny":
      return "已拒绝";
    case "always":
      return "已批准（始终允许）";
    case "cancelled":
      return "已取消";
  }
}

/** 确保 machine 空间 + conv 存在（不可变），返回新 spaces；均已存在则原样返回。 */
function ensureConv(
  state: AppState,
  machine: string,
  convId: string,
): Record<string, MachineSpace> {
  const hadSpace = state.spaces[machine] !== undefined;
  const space = hadSpace ? state.spaces[machine] : newSpace();
  if (hadSpace && space.convs[convId]) {
    return state.spaces;
  }
  const nextSpace: MachineSpace = space.convs[convId]
    ? space
    : {
        convs: {
          ...space.convs,
          [convId]: { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 },
        },
        convOrder: [...space.convOrder, convId],
      };
  return { ...state.spaces, [machine]: nextSpace };
}

/** 条目 → 侧边栏单行预览（首行、去首尾空白）。 */
function previewOf(entry: StreamEntry): string {
  switch (entry.kind) {
    case "user":
    case "text":
    case "approval_note":
      return firstLine(entry.text);
    case "card": {
      const t = firstLine(entry.card.text);
      return t || entry.card.task_digest || "";
    }
  }
}

function firstLine(s: string): string {
  const i = s.indexOf("\n");
  return (i === -1 ? s : s.slice(0, i)).trim();
}

/** 内存护栏：超上限丢最早的 user 回显；无 user 可丢（纯服务端条目）即停。 */
function trimEntries(entries: StreamEntry[]): StreamEntry[] {
  let trimmed = entries;
  while (trimmed.length > MAX_STREAM_ENTRIES) {
    const i = trimmed.findIndex((e) => e.kind === "user");
    if (i === -1) {
      break;
    }
    trimmed = trimmed.filter((_, idx) => idx !== i);
  }
  return trimmed;
}

function appendEntry(
  state: AppState,
  machine: string,
  convId: string,
  entry: StreamEntry,
): AppState {
  const spaces = ensureConv(state, machine, convId);
  const space = spaces[machine];
  const conv = space.convs[convId];
  // user/text 盖到达时间戳（approval_note 不展示时间）
  const stamped: StreamEntry =
    entry.kind === "user" || entry.kind === "text" ? { ...entry, at: Date.now() } : entry;
  // 未读：非当前 conv 的服务端条目（text / approval_note）+1；用户回显恒落
  // 当前 conv（send_echo 由 App 发送路径产生），不计。
  const bumpUnread = stamped.kind !== "user" && !(machine === state.currentMachine && convId === state.currentConv);
  return {
    ...state,
    seq: state.seq + 1,
    spaces: {
      ...spaces,
      [machine]: {
        ...space,
        convs: {
          ...space.convs,
          [convId]: {
            ...conv,
            entries: trimEntries([...conv.entries, stamped]),
            lastPreview: previewOf(stamped),
            lastAt: Date.now(),
            unread: bumpUnread ? conv.unread + 1 : conv.unread,
          },
        },
      },
    },
  };
}

/** card / card_patch：同机器同 conv 内同 messageId 整卡替换（保留原位），否则追加。
 *  概要（lastPreview/lastAt）仅在卡片是会话最新条目时更新——补丁旧卡片不动概要。
 *  未读：仅「已存在的 conv」的新卡片插入计数（非当前 conv）；替换（card_patch /
 *  sync 重放同 messageId）与 conv 首次物化（刷新/切机后的 sync 恢复、新会话首卡）
 *  不计——刷新重连后不会把全部会话误点亮。 */
function upsertCard(state: AppState, machine: string, frame: CardFrame): AppState {
  const convExisted = state.spaces[machine]?.convs[frame.convId] !== undefined;
  const spaces = ensureConv(state, machine, frame.convId);
  const space = spaces[machine];
  const conv = space.convs[frame.convId];
  const idx = conv.entries.findIndex(
    (e) => e.kind === "card" && e.messageId === frame.messageId,
  );
  const entry: CardEntry = {
    kind: "card",
    key: `card:${machine}:${frame.convId}:${frame.messageId}`,
    messageId: frame.messageId,
    card: frame.card,
    state: frame.state,
  };
  let entries: StreamEntry[];
  if (idx >= 0) {
    entries = conv.entries.slice();
    entries[idx] = entry;
  } else {
    entries = [...conv.entries, entry];
  }
  entries = trimEntries(entries);
  const isLatest = idx === -1 || idx === conv.entries.length - 1;
  const bumpUnread =
    idx === -1 && convExisted && !(machine === state.currentMachine && frame.convId === state.currentConv);
  const nextConv: ConvState = {
    ...conv,
    entries,
    unread: bumpUnread ? conv.unread + 1 : conv.unread,
    ...(isLatest ? { lastPreview: previewOf(entry), lastAt: Date.now() } : {}),
  };
  return {
    ...state,
    seq: idx >= 0 ? state.seq : state.seq + 1,
    spaces: {
      ...spaces,
      [machine]: { ...space, convs: { ...space.convs, [frame.convId]: nextConv } },
    },
  };
}

/** 清某机器某 conv 的未读计数（点选会话 / 机器切换落在默认会话）；无变化返回原 state。 */
function clearUnread(state: AppState, machine: string, convId: string): AppState {
  const space = state.spaces[machine];
  const conv = space?.convs[convId];
  if (!space || !conv || conv.unread === 0) {
    return state;
  }
  return {
    ...state,
    spaces: {
      ...state.spaces,
      [machine]: {
        ...space,
        convs: { ...space.convs, [convId]: { ...conv, unread: 0 } },
      },
    },
  };
}

function addApproval(state: AppState, machine: string, frame: ApprovalRequestFrame): AppState {
  const spaces = ensureConv(state, machine, frame.convId);
  const space = spaces[machine];
  const conv = space.convs[frame.convId];
  if (conv.pending.some((p) => p.requestId === frame.requestId)) {
    return state; // 同 requestId 幂等
  }
  return {
    ...state,
    spaces: {
      ...spaces,
      [machine]: {
        ...space,
        convs: {
          ...space.convs,
          [frame.convId]: {
            ...conv,
            pending: [
              ...conv.pending,
              {
                machine,
                convId: frame.convId,
                requestId: frame.requestId,
                tool: frame.tool,
                inputSummary: frame.inputSummary,
              },
            ],
          },
        },
      },
    },
  };
}

/** 移出 pending 并在流里留终态记录；无对应 pending 则忽略（防 sync 噪声）。 */
function resolveApproval(
  state: AppState,
  machine: string,
  frame: ApprovalResolvedFrame,
): AppState {
  const conv = spaceOf(state, machine).convs[frame.convId];
  if (!conv) {
    return state;
  }
  const pending = conv.pending.filter((p) => p.requestId !== frame.requestId);
  if (pending.length === conv.pending.length) {
    return state;
  }
  const stripped: AppState = {
    ...state,
    spaces: {
      ...state.spaces,
      [machine]: {
        ...state.spaces[machine],
        convs: { ...state.spaces[machine].convs, [frame.convId]: { ...conv, pending } },
      },
    },
  };
  return appendEntry(stripped, machine, frame.convId, {
    kind: "approval_note",
    key: `note:${state.seq + 1}`,
    text: resolvedLabel(frame.decision),
    message: frame.message,
  });
}

/** machines 帧：全量覆盖在线列表；当前机器不在列表时自动切第一台在线并出
 *  notice（多台网关掉线重排时保证 SPA 仍指向一台在线机器，出站帧不致
 *  machine_offline）。全部离线无可切时保持当前机器并出 offline notice。 */
function reduceMachines(state: AppState, frame: MachinesFrame): AppState {
  const online = frame.online;
  let { currentMachine } = state;
  let lastNotice = state.lastNotice;
  if (!online.includes(currentMachine)) {
    if (online.length > 0) {
      lastNotice = { kind: "switched", from: currentMachine, to: online[0], at: Date.now() };
      currentMachine = online[0];
    } else if (state.machinesKnown) {
      lastNotice = { kind: "offline", machine: currentMachine, at: Date.now() };
    }
  }
  return {
    ...state,
    machines: online,
    machinesKnown: true,
    currentMachine,
    lastNotice,
  };
}

/** 入站帧归属机器：带 machine 字段归该机器，缺省归 DEFAULT_MACHINE（旧网关兼容）。 */
type MachineBearingFrame = TextFrame | CardFrame | ApprovalRequestFrame | ApprovalResolvedFrame;

function frameMachine(frame: MachineBearingFrame): string {
  return frame.machine ?? DEFAULT_MACHINE;
}

function reduceFrame(state: AppState, frame: InboundFrame): AppState {
  switch (frame.type) {
    case "hello_ack":
    case "pong":
      return state; // 连接管理层（ws.ts）消费，不进消息流
    case "e2e_ack":
    case "enc":
      // v1.4：ws 层 e2e 协调器消费（enc 解密后的内帧才二次分发进来）；
      // reducer 收到即说明上游漏拦，静默忽略。
      return state;
    case "machines":
      return reduceMachines(state, frame);
    case "devices":
      // v1.2 设备列表快照：全量覆盖（device_list 请求 / device_revoke 后回）
      return { ...state, devices: frame.devices };
    case "creds":
      // v1.3 通行钥匙列表快照：全量覆盖（cred_list 请求 / cred_revoke 后回）
      return { ...state, passkeys: frame.credentials };
    case "machine":
      return { ...state, machineOnline: frame.online };
    case "error":
      return { ...state, lastError: { code: frame.code, at: Date.now() } };
    case "text":
      return appendEntry(state, frameMachine(frame), frame.convId, {
        kind: "text",
        key: `text:${state.seq + 1}`,
        text: frame.text,
        hint: frame.hint,
      });
    case "card":
    case "card_patch":
      return upsertCard(state, frameMachine(frame), frame);
    case "approval_request":
      return addApproval(state, frameMachine(frame), frame);
    case "approval_resolved":
      return resolveApproval(state, frameMachine(frame), frame);
  }
}

export function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case "frame":
      return reduceFrame(state, action.frame);
    case "send_echo":
      // machine 缺省落 default 空间（既有单机测试路径兼容）
      return appendEntry(state, action.machine ?? DEFAULT_MACHINE, action.convId, {
        kind: "user",
        key: `user:${state.seq + 1}`,
        text: action.text,
      });
    case "approval_sent": {
      const machine = action.machine ?? DEFAULT_MACHINE;
      const space = state.spaces[machine];
      const conv = space?.convs[action.convId];
      if (!conv) {
        return state;
      }
      const pending = conv.pending.filter((p) => p.requestId !== action.requestId);
      if (pending.length === conv.pending.length) {
        return state;
      }
      return {
        ...state,
        spaces: {
          ...state.spaces,
          [machine]: {
            ...space,
            convs: { ...space.convs, [action.convId]: { ...conv, pending } },
          },
        },
      };
    }
    case "select_machine":
      if (state.currentMachine === action.machine) {
        return state;
      }
      // 切机即回到该机器默认会话（视为「已读」落点，清其未读）
      return clearUnread(
        { ...state, currentMachine: action.machine, currentConv: DEFAULT_CONV },
        action.machine,
        DEFAULT_CONV,
      );
    case "select_conv": {
      // 点选会话：切当前 conv 并清未读；同 conv 重复点选仅承担清零
      const changed = clearUnread(state, state.currentMachine, action.convId);
      return changed.currentConv === action.convId ? changed : { ...changed, currentConv: action.convId };
    }
    case "clear_error":
      return state.lastError ? { ...state, lastError: null } : state;
    case "clear_notice":
      return state.lastNotice ? { ...state, lastNotice: null } : state;
  }
}
