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
// - v1.5 准入与管理：re-auth（HTTP /webauthn/reauth/*，adminToken 5 分钟，
//   lib/webauthn.ts + lib/admin.ts）；管理帧（machine_list/machine_add/
//   machine_remove/user_token_rotate ↔ machines_cfg/machine_invalid/
//   user_token_rotated，全部带 adminToken，relay 自产自消恒明文）；新设备
//   配对（hello{pairing} → pairing_pending；已登录侧收 pairing_request →
//   pairing_approve/pairing_deny{adminToken,code}）。
// - v1.6 工作空间：工作目录升格为一等公民。出站 workspace_list /
//   workspace_save / workspace_remove / conv_workdir_set（网关消费的业务
//   帧，恒 e2e 加密）；入站 workspaces 全量快照（default + items + convs，
//   网关处理完任一 workspace 帧必回——SPA 以快照为唯一事实源，乐观更新一律
//   不做）与 conv_workdir 增量（conv_workdir_set 成功后随快照一起回）。
//   失败复用 error 帧（code="workspace_invalid"）。
// - v1.7 媒体上行 + 会话持久化：message 帧可选 media{name,mime,b64}（随帧
//   自动加密，网关落盘 media/up-*，超 8MB 回 error{media_too_large}）；入站
//   workspaces 帧新增可选 sessions[{id,title,updatedAt}] 段（历史会话恢复，
//   旧端三段不动——见 reducer mergePersistedSessions）。
// - v1.8 模型菜单动态化：workspaces 帧新增可选 models{current, cliDefault,
//   options[{label, target}]} 段（网关汇聚：CLI ~/.claude/settings.json 的
//   env.ANTHROPIC_DEFAULT_* 映射优先，config official_models 官方列表回退；
//   空 options 整段缺席）。reducer 落 workspaces[machine].models，命令面板
//   /model chips 与当前模型文本据此渲染（lib/commands modelChips）。
// - v1.8 会话历史重放：网关为每 conv 落盘出站消息日志（outbound_log，per
//   conv 单调 seq），浏览器出站 history_query{convId, since, machine}（业务
//   帧，E2E 加密）按本地高水位增量拉取；网关回 history{machine, convId,
//   entries, lastSeq, trimmed}——entries 元素与 StreamEntry 对齐（key 前缀
//   `h:` 与实时条目键空间区分，card 恒 state="done" 终态快照）。合并语义见
//   mergeHistory：重放不计未读、概要仅空 conv 刷新、card 同 messageId 本地
//   优先；高水位（lastSeq）由 App 层维护（historyStore 随缓存落盘）。

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
// 会话自动命名（纯 SPA 侧，协议与后端零改动）：「会话 ID」与「显示名」解耦。
// 留空名字创建新会话时 SPA 生成短随机 ID（s-xxxx）走既有 /switch 流程；显示名
// 由 reducer 从该会话首条消息派生（ConvState.title，见 appendEntry）。
// ---------------------------------------------------------------------------

/** 自动会话完整形态：web:s-<4 位小写 hex>（newAutoConvName + web: 前缀）。 */
export const AUTO_CONV_RE = /^web:s-[0-9a-f]{4}$/;

/** 是否机器生成的自动会话（web:s-xxxx）。自动会话与 default 会话的显示名
 *  采用自动 title（见 convTitle）；手动名是用户输入（convId 即名字），恒不
 *  被自动 title 覆盖；local: 会话走剥前缀显示。 */
export function isAutoConv(convId: string): boolean {
  return AUTO_CONV_RE.test(convId);
}

/** 生成自动会话短名：s-<4 位小写 hex 随机>。同机重复概率 1/65536 极小，且
 *  ensureConv 按 convId 幂等（真撞上即并入同一会话，无碍）。 */
export function newAutoConvName(): string {
  const b = new Uint8Array(2);
  crypto.getRandomValues(b);
  return `s-${Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("")}`;
}

/** conv 显示名（用户可见，手动名 > 自动 title > 剥前缀兜底）：自动会话
 *  （web:s-xxxx）与 default 会话优先首条消息派生的 title——default 恒为固定
 *  入口，未发消息（title 未派生）时回退 convDisplayName 显示 "default"；
 *  其余（手动名 / local）恒 convDisplayName 剥前缀。title 派生见
 *  ConvState.title。 */
export function convTitle(conv: ConvState | undefined, convId: string): string {
  return (isAutoConv(convId) || convId === DEFAULT_CONV) && conv?.title
    ? conv.title
    : convDisplayName(convId);
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

/** hello 双形态（v1.2）+ 配对形态（v1.5）：首次登录带 token（+可选 device）；
 * 重连带 session（session 由新 relay 签发，只会发往新 relay；旧 relay 持有
 * session 场景不存在——token 缺失直接 1008 回登录页）。 */
export type HelloFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "hello";
  /** 首次登录：USER_TOKEN（一次性，签发 session 后 SPA 即删除）。 */
  token?: string;
  /** 重连：会话凭证。 */
  session?: string;
  /** 首次登录可选设备名（缺省 deviceSummary(navigator.userAgent)）。 */
  device?: string;
  /** v1.5 新设备配对：无凭证 hello，relay 创建 pending 并回 pairing_pending；
   * 该连接保持未认证等待态，被批准后 relay 直接换发 hello_ack.session。 */
  pairing?: boolean;
};

/** v1.7 媒体上行载荷（契约 A 段，字段名与 Rust MediaPayload 逐字一致）：
 * name ≤64 字符、mime 须 image/*、b64 单行无换行且解码后 ≤8MB（网关超限回
 * error{media_too_large}；SPA 侧经 canvas 预压缩规避，见 lib/imageCompress）。 */
export type MediaPayload = { name: string; mime: string; b64: string };

export type MessageFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "message";
  /** 完整 conv（如 web:default）；缺省落网关默认会话。 */
  convId?: string;
  /** 可选说明（纯图可为空串——text 与 media 至少其一非空）。 */
  text: string;
  /** 目标机器（v1.1）：多机在线时未指定会被 relay 回 ambiguous_machine，
   *  故多机模式下 SPA 必须恒带当前选中机器；单机模式（旧 relay）不带。 */
  machine?: string;
  /** v1.7 可选图片附件（message 已在 E2E 业务帧集合，media 随帧自动加密）。 */
  media?: MediaPayload;
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
// v1.5 管理帧（浏览器 → relay 自产自消；全部带 adminToken，缺失/无效回
// error{admin_auth_required}。adminToken 来自 re-auth（lib/webauthn.ts），
// 5 分钟有效，内存态（lib/admin.ts）。管理帧恒明文（relay 消费，不经网关）。
// ---------------------------------------------------------------------------

/** 机器名约束（relay 侧同规）：1..=32 位 [a-zA-Z0-9_-]。 */
export const MACHINE_NAME_RE = /^[a-zA-Z0-9_-]{1,32}$/;

/** 网关 ed25519 公钥约束：64 位 hex（大小写宽容，提交前归一小写）。 */
export const PUBKEY_HEX_RE = /^[0-9a-fA-F]{64}$/;

export function machineNameValid(name: string): boolean {
  return MACHINE_NAME_RE.test(name);
}

export function machinePubkeyValid(pubkey: string): boolean {
  return PUBKEY_HEX_RE.test(pubkey);
}

export type MachineListFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "machine_list";
  adminToken: string;
};

export type MachineAddFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "machine_add";
  adminToken: string;
  name: string;
  /** 64 hex 网关公钥。 */
  pubkey: string;
};

export type MachineRemoveFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "machine_remove";
  adminToken: string;
  name: string;
};

export type UserTokenRotateFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "user_token_rotate";
  adminToken: string;
};

/** v1.5 配对批准（管理级，须 re-auth）：code = pairing_request 帧的 6 位码。 */
export type PairingApproveFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "pairing_approve";
  adminToken: string;
  code: string;
};

export type PairingDenyFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "pairing_deny";
  adminToken: string;
  code: string;
};

// ---------------------------------------------------------------------------
// v1.6 工作空间帧（浏览器 → 网关，E2E 业务帧）
// ---------------------------------------------------------------------------

/** 工作空间名约束（网关侧同规）：1..=32 位非空（trim 后）。 */
export const WORKSPACE_NAME_MAX = 32;

export function workspaceNameValid(name: string): boolean {
  const n = name.trim();
  return n.length >= 1 && n.length <= WORKSPACE_NAME_MAX;
}

/** v1.6 请求工作空间全量快照（网关回 workspaces 帧；连接 ready / 切机后发）。 */
export type WorkspaceListFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "workspace_list";
  /** 目标机器（v1.1 路由约定，同 message 族：多机模式必须带）。 */
  machine?: string;
};

/** v1.6 保存工作空间（name 1..=32 非空 trim 后；path 绝对路径）。 */
export type WorkspaceSaveFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "workspace_save";
  name: string;
  path: string;
  machine?: string;
};

/** v1.6 移除工作空间（按 name；成功/失败均回 workspaces 快照）。 */
export type WorkspaceRemoveFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "workspace_remove";
  name: string;
  machine?: string;
};

/** v1.6 绑定会话工作目录（convId 恒为完整 conv 形态；path 绝对路径；成功回
 *  conv_workdir 增量 + workspaces 快照）。 */
export type ConvWorkdirSetFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "conv_workdir_set";
  convId: string;
  path: string;
  machine?: string;
};

// ---------------------------------------------------------------------------
// v1.8 会话历史重放（浏览器 → 网关，E2E 业务帧）
// ---------------------------------------------------------------------------

/** v1.8 增量拉取会话出站日志：convId 恒为完整 conv 形态；since = 本地高水位
 *  （该 conv 已见的最大 seq，0 = 从头拉）；machine 同 message 族路由约定
 *  （多机模式必须带当前机器）。网关回 history 帧。 */
export type HistoryQueryFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "history_query";
  convId: string;
  since: number;
  machine?: string;
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
  | EncFrame
  | MachineListFrame
  | MachineAddFrame
  | MachineRemoveFrame
  | UserTokenRotateFrame
  | PairingApproveFrame
  | PairingDenyFrame
  | WorkspaceListFrame
  | WorkspaceSaveFrame
  | WorkspaceRemoveFrame
  | ConvWorkdirSetFrame
  | HistoryQueryFrame;

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

/** v1.5 起可带 message（machine_invalid 等 relay 自产错误的说明文字）。 */
export type ErrorFrame = { v: typeof PROTOCOL_VERSION; type: "error"; code: string; message?: string };

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

// ---------------------------------------------------------------------------
// v1.5 管理/配对入站帧（relay 自产，恒明文）
// ---------------------------------------------------------------------------

/** machines_cfg 机器条目：pubkey_fp = 网关公钥 SHA-256 指纹前 16 hex（relay
 *  侧计算）；created = unix 秒（容错可选，缺席仅影响展示）。 */
export type MachineCfg = { name: string; pubkey_fp: string; created?: number };

/** v1.5：机器配置列表快照（machine_list 请求 / machine_add / machine_remove
 *  成功后回；管理页机器区数据源）。 */
export type MachinesCfgFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "machines_cfg";
  machines: MachineCfg[];
};

/** v1.5：USER_TOKEN 轮换结果。token 是新值明文——**仅此一次返回**，SPA 以
 *  结果弹层展示一次（复制落 ~/.imagent/web-relay.env），不落任何存储。 */
export type UserTokenRotatedFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "user_token_rotated";
  token: string;
};

/** v1.5：relay 向全部已认证会话广播的配对请求（新设备在登录页发起
 *  hello{pairing} 时产生）。expiresAt unix 秒（容错可选）。 */
export type PairingRequestFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "pairing_request";
  code: string;
  device: string;
  expiresAt?: number;
};

/** v1.5：配对等待确认（发往配对连接本身）：6 位数字码，5 分钟有效。 */
export type PairingPendingFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "pairing_pending";
  code: string;
};

// ---------------------------------------------------------------------------
// v1.6 工作空间帧（网关 → 浏览器，E2E 业务帧，经 enc 解密后二次分发进 reducer）
// ---------------------------------------------------------------------------

/** v1.6 工作空间条目（workspaces 帧 items 元素）。lastUsedAt unix 秒，可选
 *  （对齐 Rust skip_serializing_if——缺席即未序列化）。 */
export type WorkspaceInfo = { name: string; path: string; lastUsedAt?: number };

/** v1.7 会话持久化条目（workspaces 帧可选 sessions 段元素，契约 B 段）：
 * id = 完整 conv（store sessions 表 web: 前缀行）、title = 显示名（v1.9 三
 * 档：手动名 > 语义标题 > first_prompt 按 Unicode 码点截 24 字符）、
 * updatedAt = unix 秒。 */
export type SessionSummary = { id: string; title: string; updatedAt: number };

/** v1.8 模型菜单项（workspaces 帧可选 models 段 options 元素）：label = 菜单
 *  显示名（别名 sonnet/opus/haiku 或官方模型名）；target = 第三方映射的
 *  实际模型 id（无映射为 null——网关按可空字符串序列化，非缺席）。 */
export type ModelOption = { label: string; target: string | null };

/** v1.8 workspaces 帧可选 models 段（模型菜单动态化，网关侧汇聚）：current =
 *  /model 命令热设值（null = 未设置，跟随 CLI 默认）；cliDefault =
 *  settings.json env.ANTHROPIC_MODEL（CLI 侧默认模型，无则 null）；
 *  options = 菜单项（网关侧空 options 整段不下发）。 */
export type ModelsInfo = {
  current: string | null;
  cliDefault: string | null;
  options: ModelOption[];
};

/** v1.6 工作空间全量快照（唯一事实源）：default = 该机器 default_workdir
 *  绝对路径；convs = convId → workdir 绝对路径（含 local: 前缀会话）。machine
 *  = 来源机器（旧网关缺席 → 归 DEFAULT_MACHINE）。v1.7 起可选携带 sessions
 *  （该机器持久化的历史会话列表，按 updatedAt 降序；旧网关缺席 → 不合并）；
 *  v1.8 起可选携带 models（/model 菜单数据，旧网关缺席 → SPA 内置兜底）。 */
export type WorkspacesFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "workspaces";
  default: string;
  workspaces: WorkspaceInfo[];
  convs: Record<string, string>;
  machine?: string;
  sessions?: SessionSummary[];
  models?: ModelsInfo;
};

/** v1.6 会话工作目录增量（conv_workdir_set 成功后随 workspaces 快照一起回）。 */
export type ConvWorkdirFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "conv_workdir";
  convId: string;
  workdir: string;
  machine?: string;
};

// ---------------------------------------------------------------------------
// v1.8 会话历史重放（网关 → 浏览器，E2E 业务帧，经 enc 解密后二次分发进 reducer）
// ---------------------------------------------------------------------------

/** v1.8 history 帧 entries 元素（与 StreamEntry 对齐的子集）：text 带 at
 *  （网关出站日志的 unix 毫秒）；card 恒为终态快照（state="done"）。key 前缀
 *  `h:` + seq（如 "h:42"）——与实时条目（text:N / card:m:c:msg）及本地恢复
 *  条目（h:text:N）键空间区分。 */
export type HistoryEntry =
  | { kind: "text"; key: string; text: string; at: number }
  | { kind: "card"; key: string; messageId: string; card: CardPayload; state: "done" };

/** v1.8 会话历史重放响应：entries = seq > since 的行（升序，≤500；空数组 =
 *  无增量，仅更新水位）；lastSeq = 该 conv 当前最大 seq（无任何记录时 0）；
 *  trimmed = 因 GC 返回集不含 since 之前的完整历史（客户端应丢弃本地该 conv
 *  缓存、以返回集重建）。machine = 来源机器（缺席归 DEFAULT_MACHINE）。 */
export type HistoryFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "history";
  convId: string;
  entries: HistoryEntry[];
  lastSeq: number;
  trimmed: boolean;
  machine?: string;
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
  | EncFrame
  | MachinesCfgFrame
  | UserTokenRotatedFrame
  | PairingRequestFrame
  | PairingPendingFrame
  | WorkspacesFrame
  | ConvWorkdirFrame
  | HistoryFrame;

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

/** v1.5 machines_cfg 帧元素：name/pubkey_fp 字符串；created 可选但出现即须
 *  为数字（容错展示字段）。 */
const machineCfgOk = (m: unknown): boolean => {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const r = m as Record<string, unknown>;
  return isStr(r, "name") && isStr(r, "pubkey_fp") && (r.created === undefined || typeof r.created === "number");
};

/** v1.6 workspaces 帧元素：name/path 字符串；lastUsedAt 可选但出现即须为
 *  数字（对齐 Rust skip_serializing_if）。 */
const workspaceOk = (w: unknown): boolean => {
  if (typeof w !== "object" || w === null) {
    return false;
  }
  const r = w as Record<string, unknown>;
  return (
    isStr(r, "name") &&
    isStr(r, "path") &&
    (r.lastUsedAt === undefined || typeof r.lastUsedAt === "number")
  );
};

/** v1.6 workspaces.convs：记录（convId → workdir 绝对路径字符串）。 */
const convDirsOk = (c: unknown): boolean => {
  if (typeof c !== "object" || c === null || Array.isArray(c)) {
    return false;
  }
  return Object.values(c as Record<string, unknown>).every((v) => typeof v === "string");
};

/** v1.7 workspaces.sessions 元素（可选段）：id/title 字符串、updatedAt 数字；
 *  段本身缺席（旧网关）合法。 */
const sessionSummaryOk = (s: unknown): boolean => {
  if (typeof s !== "object" || s === null) {
    return false;
  }
  const r = s as Record<string, unknown>;
  return isStr(r, "id") && isStr(r, "title") && typeof r.updatedAt === "number";
};

/** v1.8 workspaces.models.options 元素：label 字符串；target 字符串或 null
 *  （无映射的 null 形态——契约非缺席）。 */
const modelOptionOk = (o: unknown): boolean => {
  if (typeof o !== "object" || o === null) {
    return false;
  }
  const r = o as Record<string, unknown>;
  return isStr(r, "label") && (r.target === null || typeof r.target === "string");
};

/** v1.8 workspaces.models 段（可选）：current/cliDefault 字符串或 null、
 *  options 为全合法元素数组；段本身缺席（旧网关）合法。 */
const modelsOk = (m: unknown): boolean => {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const r = m as Record<string, unknown>;
  return (
    (r.current === null || typeof r.current === "string") &&
    (r.cliDefault === null || typeof r.cliDefault === "string") &&
    Array.isArray(r.options) &&
    r.options.every(modelOptionOk)
  );
};

/** v1.8 history 帧 entries 元素：kind 二选一——text 需 key/text；card 需
 *  key/messageId + card 对象（载荷完整性由网关保证，card 深层字段不作逐项
 *  校验——与 StreamEntry 渲染路径一致，畸形载荷不致崩溃）。 */
const historyEntryOk = (e: unknown): boolean => {
  if (typeof e !== "object" || e === null) {
    return false;
  }
  const r = e as Record<string, unknown>;
  if (r.kind === "text") {
    return isStr(r, "key") && isStr(r, "text");
  }
  if (r.kind === "card") {
    return (
      isStr(r, "key") && isStr(r, "messageId") && typeof r.card === "object" && r.card !== null
    );
  }
  return false;
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
  error: (o) =>
    isStr(o, "code") && (o.message === undefined || typeof o.message === "string"),
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
  // v1.5 管理/配对帧：字段级形状校验
  machines_cfg: (o) => Array.isArray(o.machines) && o.machines.every(machineCfgOk),
  user_token_rotated: (o) => isStr(o, "token"),
  pairing_request: (o) =>
    isStr(o, "code") && isStr(o, "device") && (o.expiresAt === undefined || typeof o.expiresAt === "number"),
  pairing_pending: (o) => isStr(o, "code"),
  // v1.4：字段级形状校验（编码/长度/签名校验在 lib/e2e.ts 深层完成）
  e2e_ack: (o) =>
    isStr(o, "machine") && isStr(o, "gk") && isStr(o, "n2") && isStr(o, "sig") && ackIdentityOk(o),
  enc: (o) => isStr(o, "machine") && isStr(o, "box"),
  // v1.6 工作空间帧：逐字段形状校验（machine 可选——旧网关缺席归 DEFAULT_MACHINE）；
  // v1.7 sessions 可选段 / v1.8 models 可选段：出现即须为全合法元素
  workspaces: (o) =>
    isStr(o, "default") &&
    Array.isArray(o.workspaces) &&
    o.workspaces.every(workspaceOk) &&
    convDirsOk(o.convs) &&
    (o.sessions === undefined || (Array.isArray(o.sessions) && o.sessions.every(sessionSummaryOk))) &&
    (o.models === undefined || modelsOk(o.models)) &&
    machineOk(o),
  conv_workdir: (o) => isStr(o, "convId") && isStr(o, "workdir") && machineOk(o),
  // v1.8 会话历史重放：entries 逐元素形状 + lastSeq 非负整数 + trimmed 布尔
  //（machine 可选——旧网关缺席归 DEFAULT_MACHINE）
  history: (o) =>
    isStr(o, "convId") &&
    Array.isArray(o.entries) &&
    o.entries.every(historyEntryOk) &&
    typeof o.lastSeq === "number" &&
    Number.isInteger(o.lastSeq) &&
    o.lastSeq >= 0 &&
    typeof o.trimmed === "boolean" &&
    machineOk(o),
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
  /** 自动显示名（会话自动命名）：该会话首条 user/text 消息的单行摘要（按字符
   *  截 CONV_TITLE_MAX_CHARS，中文不劈半），一旦写入不再变——首条消息即定调。
   *  所有会话一律派生，但仅自动形态（web:s-xxxx，见 isAutoConv）与 default
   *  会话的显示名采用（convTitle）；手动名 / local 会话派生而不用。 */
  title?: string;
};

/** 单台机器的会话空间：会话表 + conv 插入序（web:default 恒为首项）。 */
export type MachineSpace = {
  convs: Record<string, ConvState>;
  convOrder: string[];
};

/** 自动显示名（title）最大字符数：按 Unicode 码点截断（中文/emoji 不劈半）。 */
export const CONV_TITLE_MAX_CHARS = 24;

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
  /** v1.5 机器配置列表（machines_cfg 帧）；null = 尚未拉取（管理页未打开
   *  或 re-auth 未完成）。 */
  machinesCfg: MachineCfg[] | null;
  /** v1.5 待决配对请求（pairing_request 帧按 code 幂等追加；允许/拒绝帧
   *  发出后经 pairing_resolved 移除）。 */
  pairingRequests: PairingRequest[];
  /** v1.5 USER_TOKEN 轮换结果（仅显示一次的明文新值；clear_rotated_token
   *  关闭弹层即永失——不落任何存储）。 */
  rotatedToken: string | null;
  /** v1.6 逐机器工作空间快照（workspaces 帧按来源机器落表；空 = 尚未收到
   *  快照（旧网关），目录条与目录选择不渲染）。 */
  workspaces: Record<string, MachineWorkspaces>;
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

/** v1.5 待决配对请求条目（pairing_request 帧载荷 + 到达时间）。 */
export type PairingRequest = {
  code: string;
  device: string;
  /** unix 秒（容错可选；倒计时用）。 */
  expiresAt?: number;
  /** 到达时间（ms，UI 展示）。 */
  at: number;
};

/** v1.6 单台机器的工作空间快照（workspaces 帧按 frame.machine 归入）：
 *  default = 该机器 default_workdir；items = 已存空间；convs = convId →
 *  当前 workdir。SPA 以快照为唯一事实源（乐观更新一律不做）。v1.8 起可含
 *  models（/model 菜单数据；undefined = 网关未下发——空 options 或旧网关，
 *  chips 回退内置）。 */
export type MachineWorkspaces = {
  default: string;
  items: WorkspaceInfo[];
  convs: Record<string, string>;
  models?: ModelsInfo;
};

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
    machinesCfg: null,
    pairingRequests: [],
    rotatedToken: null,
    workspaces: {},
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
  /** v1.5 关闭轮换结果弹层（一次性明文 token 就此作废）。 */
  | { type: "clear_rotated_token" }
  /** v1.5 配对请求已应答（允许/拒绝帧发出后移除对应卡片）。 */
  | { type: "pairing_resolved"; code: string }
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

/** 自动命名摘要：首行（firstLine 已去空白）按码点截 CONV_TITLE_MAX_CHARS；
 *  截后为空串返回 null（不写 title，留待后续消息定调）。 */
function titleSummary(text: string): string | null {
  const s = Array.from(firstLine(text)).slice(0, CONV_TITLE_MAX_CHARS).join("");
  return s.length > 0 ? s : null;
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
  // 自动命名：title 未定时由首条 user/text 消息定调（观看端无 user 回显时首条
  // text 同样派生；card / approval_note 不定调）。一旦写入不再变；空摘要不写。
  const derivedTitle =
    conv.title === undefined && (stamped.kind === "user" || stamped.kind === "text")
      ? titleSummary(stamped.text)
      : undefined;  return {
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
            ...(derivedTitle != null ? { title: derivedTitle } : {}),
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
type MachineBearingFrame =
  | TextFrame
  | CardFrame
  | ApprovalRequestFrame
  | ApprovalResolvedFrame
  | WorkspacesFrame
  | ConvWorkdirFrame
  | HistoryFrame;

function frameMachine(frame: MachineBearingFrame): string {
  return frame.machine ?? DEFAULT_MACHINE;
}

/** v1.6 conv_workdir 增量：合并进对应机器快照的 convs。该机器尚无快照时忽略
 *  ——按契约增量帧总与全量快照成对到达，无快照即无从归属（也不造无 default
 *  的残缺条目：目录条渲染以快照存在为前提）。 */
function mergeConvWorkdir(state: AppState, frame: ConvWorkdirFrame): AppState {
  const machine = frameMachine(frame);
  const cur = state.workspaces[machine];
  if (!cur) {
    return state;
  }
  return {
    ...state,
    workspaces: {
      ...state.workspaces,
      [machine]: { ...cur, convs: { ...cur.convs, [frame.convId]: frame.workdir } },
    },
  };
}

/** 重放集末条时间戳：从尾部找最近带 at 的 text（重放条目无 user、card 无 at），
 *  全无则 0（与 historyStore lastAtOfEntries 同语义，UI 展示用）。 */
function lastAtOfHistory(entries: StreamEntry[]): number {
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i];
    if (e.kind === "text" && typeof e.at === "number") {
      return e.at;
    }
  }
  return 0;
}

/** v1.8 history 帧（会话历史重放）合并：重放条目**不进** appendEntry 消息流
 *  ——不计未读、不派生本地序号 key（key 即网关 seq 幂等键）。概要
 *  （lastPreview/lastAt）与 title 仅在该 conv 以重放集为全部内容时刷新——
 *  trimmed 整体替换（先清本地即空）或本地空 conv 首次物化；增量合并（本地
 *  非空）不动概要，本地优先。去重：本地已有同 key 跳过；card 同 messageId
 *  本地优先（本地可能持有等价/更新终态或流式中间态，重放恒为终态快照，覆盖
 *  无增益——与既有 sync/快照的本地优先语义一致）。entries 空（仅水位）reducer
 *  无事可做，高水位由 App 层 lastSeqRef 维护。 */
function mergeHistory(state: AppState, frame: HistoryFrame): AppState {
  if (frame.entries.length === 0) {
    return state;
  }
  const machine = frameMachine(frame);
  const replay: StreamEntry[] = frame.entries;
  const spaces = ensureConv(state, machine, frame.convId);
  const space = spaces[machine];
  const conv = space.convs[frame.convId];
  // trimmed：丢弃本地该 conv 缓存以重放集重建（契约）；否则本地条目保留在前，
  // 重放增量按 key / card messageId 去重 append 在后。
  const local = frame.trimmed ? [] : conv.entries;
  const seenKeys = new Set(local.map((e) => e.key));
  const seenCardIds = new Set(
    local.filter((e): e is CardEntry => e.kind === "card").map((e) => e.messageId),
  );
  const merged = local.slice();
  for (const e of replay) {
    if (seenKeys.has(e.key)) {
      continue;
    }
    if (e.kind === "card" && seenCardIds.has(e.messageId)) {
      continue;
    }
    merged.push(e);
    seenKeys.add(e.key);
    if (e.kind === "card") {
      seenCardIds.add(e.messageId);
    }
  }
  if (merged.length === local.length) {
    return state; // 全部命中去重（重复重放/本地已含）——幂等无改动
  }
  // 概要刷新仅限「本地为空」：重放集即该 conv 全部内容（与 restoreHistory 对
  // 新 conv 的概要派生同语义）；title 本地已定不覆盖（一旦写入不变），未定时
  // 由重放首条 text 补派生。
  const fresh = local.length === 0;
  const derivedTitle =
    fresh && conv.title === undefined
      ? titleSummary(replay.find((e): e is Extract<StreamEntry, { kind: "text" }> => e.kind === "text")?.text ?? "")
      : undefined;
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
            entries: trimEntries(merged),
            ...(fresh
              ? { lastPreview: previewOf(merged[merged.length - 1]), lastAt: lastAtOfHistory(merged) }
              : {}),
            ...(derivedTitle != null ? { title: derivedTitle } : {}),
          },
        },
      },
    },
  };
}

/** v1.7 会话持久化合并（契约 B 段，workspaces 快照可选 sessions 段）：
 *  sessions 里**尚不存在于本地 spaces** 的 conv 建空壳条目（entries 空、
 *  lastPreview = title、lastAt = updatedAt*1000、unread 0——离线历史不点亮
 *  未读；v1.9 起非空 title 同时落 ConvState.title），convOrder 按 updatedAt
 *  降序插入到 default 之后。
 *
 *  v1.9 语义标题放宽：本地**已存在**的 conv 不再全不动——快照 title 非空
 *  且与本地不同时**以快照为准更新 title**（网关权威：机械摘要 → 语义标题
 *  的升级靠它，快照 title = 手动名 > 语义标题 > 摘要三档汇聚后的结果）；
 *  本地 entries / 未读 / 概要 / 排序仍不动。空快照 title 不覆盖（网关无
 *  任何可用名时本地派生 title 更有价值，且避免掐断 appendEntry 的首条
 *  消息定调）。手动名会话不受影响（其快照 title = 手动名，而 convTitle
 *  对手动名恒走 convDisplayName，title 字段不参与显示）。convTitle 显示
 *  规则不变（自动形态 / default 用 title）。幂等：重复相同快照无效果。 */
function mergePersistedSessions(
  state: AppState,
  machine: string,
  sessions: SessionSummary[],
): Record<string, MachineSpace> {
  const hadSpace = state.spaces[machine] !== undefined;
  const space = hadSpace ? state.spaces[machine] : newSpace();
  // v1.9：已存在 conv 的 title 升级（快照权威），其余字段不动。
  let convs = space.convs;
  let upgraded = false;
  for (const s of sessions) {
    const conv = convs[s.id];
    if (conv !== undefined && s.title !== "" && conv.title !== s.title) {
      convs = { ...convs, [s.id]: { ...conv, title: s.title } };
      upgraded = true;
    }
  }
  const fresh = sessions
    .filter((s) => space.convs[s.id] === undefined)
    .sort((a, b) => b.updatedAt - a.updatedAt);
  if (fresh.length === 0 && !upgraded) {
    return hadSpace ? state.spaces : { ...state.spaces, [machine]: space };
  }
  convs = { ...convs }; // 未发生升级时仍是原 state 对象，填充前须拷贝（不可变纪律）
  for (const s of fresh) {
    convs[s.id] = {
      entries: [],
      pending: [],
      lastPreview: s.title,
      lastAt: s.updatedAt * 1000,
      unread: 0,
      // 与升级循环同语义：快照 title 即权威显示名（空则不置，留待本地
      // 首条消息派生——appendEntry 只在 title 未定时定调）。
      ...(s.title !== "" ? { title: s.title } : {}),
    };
  }
  const di = space.convOrder.indexOf(DEFAULT_CONV);
  const convOrder = [
    ...space.convOrder.slice(0, di + 1),
    ...fresh.map((s) => s.id),
    ...space.convOrder.slice(di + 1),
  ];
  return { ...state.spaces, [machine]: { convs, convOrder } };
}

function reduceFrame(state: AppState, frame: InboundFrame): AppState {
  switch (frame.type) {
    case "hello_ack":
    case "pong":
      return state; // 连接管理层（ws.ts）消费，不进消息流
    case "pairing_pending":
      // v1.5：配对等待码由 ws 层 onPairingPending 消费，不进应用状态
      return state;
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
    case "machines_cfg":
      // v1.5 机器配置快照：全量覆盖（machine_list / machine_add / machine_remove 后回）
      return { ...state, machinesCfg: frame.machines };
    case "user_token_rotated":
      // v1.5 轮换结果：一次性明文 token 存 reducer 供弹层展示（关闭即清）
      return { ...state, rotatedToken: frame.token };
    case "pairing_request":
      // v1.5 配对广播：按 code 幂等追加（relay 广播重放/多端同码不重复弹卡）
      if (state.pairingRequests.some((p) => p.code === frame.code)) {
        return state;
      }
      return {
        ...state,
        pairingRequests: [
          ...state.pairingRequests,
          { code: frame.code, device: frame.device, expiresAt: frame.expiresAt, at: Date.now() },
        ],
      };
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
    case "workspaces": {
      // v1.6 全量快照：按帧来源机器整体落表（唯一事实源；失败回 error 帧且
      // 不再回快照——SPA 不做乐观更新，失败即维持上一份快照）。
      // v1.7：快照可选 sessions 段 → 先合并历史会话（仅补本地不存在的 conv），
      // 再落快照表。v1.8：快照可选 models 段随快照整体覆盖（缺席即清除——
      // 网关撤下 CLI 映射/配空 official 后下一条快照自然回落内置 chips）。
      const machine = frameMachine(frame);
      const spaces = frame.sessions
        ? mergePersistedSessions(state, machine, frame.sessions)
        : state.spaces;
      return {
        ...state,
        spaces,
        workspaces: {
          ...state.workspaces,
          [machine]: {
            default: frame.default,
            items: frame.workspaces,
            convs: frame.convs,
            models: frame.models,
          },
        },
      };
    }
    case "conv_workdir":
      // v1.6 增量：合并进对应机器快照 convs（机器取帧的 machine 字段）
      return mergeConvWorkdir(state, frame);
    case "history":
      // v1.8 会话历史重放：重放条目合并不走消息流（不计未读），见 mergeHistory
      return mergeHistory(state, frame);
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
    case "clear_rotated_token":
      return state.rotatedToken ? { ...state, rotatedToken: null } : state;
    case "pairing_resolved": {
      const left = state.pairingRequests.filter((p) => p.code !== action.code);
      return left.length === state.pairingRequests.length ? state : { ...state, pairingRequests: left };
    }
  }
}
