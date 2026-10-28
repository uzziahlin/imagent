// 帧协议 v1（事实源：crates/web/src/lib.rs + crates/relay/src/lib.rs）。
// 信封 `{"v":1,"type":...}`，全部 JSON 文本帧。TS 类型与 Rust 侧逐字段对齐：
// - convId 恒为完整 conv 形态（`web:<session>`）；裸 session 名仅网关侧兼容输入，
//   SPA 回执帧（message / approval_response）一律原样回传收到的 convId。
// - CardPayload 可选字段对齐 Rust `skip_serializing_if = "Option::is_none"`
//   （缺席即未序列化）。

export const PROTOCOL_VERSION = 1;

/** 默认会话（网关 normalize_conv(None, "default") 的落点），恒存在。 */
export const DEFAULT_CONV = "web:default";

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
// 出站帧（浏览器 → 网关/relay）
// ---------------------------------------------------------------------------

export type HelloFrame = { v: typeof PROTOCOL_VERSION; type: "hello"; token: string };

export type MessageFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "message";
  /** 完整 conv（如 web:default）；缺省落网关默认会话。 */
  convId?: string;
  text: string;
};

export type ApprovalDecision = "allow" | "deny" | "always";

/** convId / requestId 必须原样回传 approval_request 帧里的值。 */
export type ApprovalResponseFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "approval_response";
  convId: string;
  requestId: string;
  decision: ApprovalDecision;
};

export type ControlFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "control";
  cmd: "stop" | "resume" | "switch";
  args?: string;
};

export type SyncFrame = { v: typeof PROTOCOL_VERSION; type: "sync" };
export type PingFrame = { v: typeof PROTOCOL_VERSION; type: "ping" };

export type OutboundFrame =
  | HelloFrame
  | MessageFrame
  | ApprovalResponseFrame
  | ControlFrame
  | SyncFrame
  | PingFrame;

/** 出站帧序列化（undefined 字段不序列化，与网关入站解析兼容）。 */
export function encodeFrame(frame: OutboundFrame): string {
  return JSON.stringify(frame);
}

// ---------------------------------------------------------------------------
// 入站帧（网关/relay → 浏览器）
// ---------------------------------------------------------------------------

export type HelloAckFrame = { v: typeof PROTOCOL_VERSION; type: "hello_ack"; role: string };

export type MachineFrame = { v: typeof PROTOCOL_VERSION; type: "machine"; online: boolean };

export type ErrorFrame = { v: typeof PROTOCOL_VERSION; type: "error"; code: string };

export type TextFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "text";
  convId: string;
  text: string;
  /** 协议预留位（web 侧恒不发，类型保留）。 */
  hint?: string;
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
};

export type ApprovalRequestFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "approval_request";
  convId: string;
  requestId: string;
  tool: string;
  inputSummary: string;
};

export type ResolvedDecision = ApprovalDecision | "cancelled";

export type ApprovalResolvedFrame = {
  v: typeof PROTOCOL_VERSION;
  type: "approval_resolved";
  convId: string;
  requestId: string;
  decision: ResolvedDecision;
  message?: string;
};

export type PongFrame = { v: typeof PROTOCOL_VERSION; type: "pong" };

export type InboundFrame =
  | HelloAckFrame
  | MachineFrame
  | ErrorFrame
  | TextFrame
  | CardFrame
  | ApprovalRequestFrame
  | ApprovalResolvedFrame
  | PongFrame;

// ---------------------------------------------------------------------------
// parseFrame：v/type 校验 + 每类帧关键字段存在性校验，非法返回 null（不抛）
// ---------------------------------------------------------------------------

type FieldCheck = (o: Record<string, unknown>) => boolean;

const isStr = (o: Record<string, unknown>, k: string) => typeof o[k] === "string";

const cardShapeOk = (o: Record<string, unknown>): boolean =>
  isStr(o, "convId") &&
  isStr(o, "messageId") &&
  typeof o.card === "object" &&
  o.card !== null &&
  isStr(o.card as Record<string, unknown>, "text");

const RESOLVED_DECISIONS: readonly string[] = ["allow", "deny", "always", "cancelled"];

/** 入站帧类型表：type → 关键字段校验（前向兼容靠“未知 type → null”由调用方忽略）。 */
const INBOUND_CHECKS: Record<string, FieldCheck> = {
  hello_ack: (o) => isStr(o, "role"),
  machine: (o) => typeof o.online === "boolean",
  error: (o) => isStr(o, "code"),
  text: (o) => isStr(o, "convId") && isStr(o, "text"),
  card: cardShapeOk,
  card_patch: cardShapeOk,
  approval_request: (o) =>
    isStr(o, "convId") && isStr(o, "requestId") && isStr(o, "tool") && isStr(o, "inputSummary"),
  approval_resolved: (o) =>
    isStr(o, "convId") &&
    isStr(o, "requestId") &&
    typeof o.decision === "string" &&
    RESOLVED_DECISIONS.includes(o.decision),
  pong: () => true,
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

/** 消息流条目：按到达顺序渲染，三类 + 审批终态记录。 */
export type StreamEntry =
  | { kind: "user"; key: string; text: string }
  | { kind: "text"; key: string; text: string; hint?: string }
  | { kind: "card"; key: string; messageId: string; card: CardPayload; state: CardFrameState }
  | { kind: "approval_note"; key: string; text: string; message?: string };

export type CardEntry = Extract<StreamEntry, { kind: "card" }>;

/** pending 审批（弹层渲染；requestId 为幂等 key）。 */
export type PendingApproval = {
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
};

/** 消息条目内存护栏：单 conv 超过上限时丢弃最早的本地用户回显（ephemeral）；
 *  服务端条目（text/card）不丢——卡片终态可由 sync 重发兜底。 */
export const MAX_STREAM_ENTRIES = 200;

export type AppState = {
  /** key 生成序号（纯函数内单调递增）。 */
  seq: number;
  convs: Record<string, ConvState>;
  /** conv 集合（插入序，web:default 恒为首项）。 */
  convOrder: string[];
  machineOnline: boolean;
  lastError: { code: string; at: number } | null;
};

export function initialState(): AppState {
  return {
    seq: 0,
    convs: { [DEFAULT_CONV]: { entries: [], pending: [], lastPreview: "", lastAt: 0 } },
    convOrder: [DEFAULT_CONV],
    machineOnline: false,
    lastError: null,
  };
}

export type Action =
  | { type: "frame"; frame: InboundFrame }
  | { type: "send_echo"; convId: string; text: string }
  | { type: "approval_sent"; convId: string; requestId: string }
  | { type: "clear_error" };

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

/** 确保 conv 存在（不可变），返回新 convs/convOrder。 */
function ensureConv(
  state: AppState,
  convId: string,
): { convs: Record<string, ConvState>; convOrder: string[] } {
  if (state.convs[convId]) {
    return { convs: state.convs, convOrder: state.convOrder };
  }
  return {
    convs: { ...state.convs, [convId]: { entries: [], pending: [], lastPreview: "", lastAt: 0 } },
    convOrder: [...state.convOrder, convId],
  };
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

function appendEntry(state: AppState, convId: string, entry: StreamEntry): AppState {
  const { convs, convOrder } = ensureConv(state, convId);
  const conv = convs[convId];
  return {
    ...state,
    seq: state.seq + 1,
    convs: {
      ...convs,
      [convId]: {
        ...conv,
        entries: trimEntries([...conv.entries, entry]),
        lastPreview: previewOf(entry),
        lastAt: Date.now(),
      },
    },
    convOrder,
  };
}

/** card / card_patch：同 conv 内同 messageId 整卡替换（保留原位），否则追加。
 *  概要（lastPreview/lastAt）仅在卡片是会话最新条目时更新——补丁旧卡片不动概要。 */
function upsertCard(state: AppState, frame: CardFrame): AppState {
  const { convs, convOrder } = ensureConv(state, frame.convId);
  const conv = convs[frame.convId];
  const idx = conv.entries.findIndex(
    (e) => e.kind === "card" && e.messageId === frame.messageId,
  );
  const entry: CardEntry = {
    kind: "card",
    key: `card:${frame.convId}:${frame.messageId}`,
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
  const nextConv: ConvState = isLatest
    ? { ...conv, entries, lastPreview: previewOf(entry), lastAt: Date.now() }
    : { ...conv, entries };
  return {
    ...state,
    seq: idx >= 0 ? state.seq : state.seq + 1,
    convs: { ...convs, [frame.convId]: nextConv },
    convOrder,
  };
}

function addApproval(state: AppState, frame: ApprovalRequestFrame): AppState {
  const { convs, convOrder } = ensureConv(state, frame.convId);
  const conv = convs[frame.convId];
  if (conv.pending.some((p) => p.requestId === frame.requestId)) {
    return state; // 同 requestId 幂等
  }
  return {
    ...state,
    convs: {
      ...convs,
      [frame.convId]: {
        ...conv,
        pending: [
          ...conv.pending,
          {
            convId: frame.convId,
            requestId: frame.requestId,
            tool: frame.tool,
            inputSummary: frame.inputSummary,
          },
        ],
      },
    },
    convOrder,
  };
}

/** 移出 pending 并在流里留终态记录；无对应 pending 则忽略（防 sync 噪声）。 */
function resolveApproval(state: AppState, frame: ApprovalResolvedFrame): AppState {
  const conv = state.convs[frame.convId];
  if (!conv) {
    return state;
  }
  const pending = conv.pending.filter((p) => p.requestId !== frame.requestId);
  if (pending.length === conv.pending.length) {
    return state;
  }
  return appendEntry(
    { ...state, convs: { ...state.convs, [frame.convId]: { ...conv, pending } } },
    frame.convId,
    {
      kind: "approval_note",
      key: `note:${state.seq + 1}`,
      text: resolvedLabel(frame.decision),
      message: frame.message,
    },
  );
}

function reduceFrame(state: AppState, frame: InboundFrame): AppState {
  switch (frame.type) {
    case "hello_ack":
    case "pong":
      return state; // 连接管理层（ws.ts）消费，不进消息流
    case "machine":
      return { ...state, machineOnline: frame.online };
    case "error":
      return { ...state, lastError: { code: frame.code, at: Date.now() } };
    case "text":
      return appendEntry(state, frame.convId, {
        kind: "text",
        key: `text:${state.seq + 1}`,
        text: frame.text,
        hint: frame.hint,
      });
    case "card":
    case "card_patch":
      return upsertCard(state, frame);
    case "approval_request":
      return addApproval(state, frame);
    case "approval_resolved":
      return resolveApproval(state, frame);
  }
}

export function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case "frame":
      return reduceFrame(state, action.frame);
    case "send_echo":
      return appendEntry(state, action.convId, {
        kind: "user",
        key: `user:${state.seq + 1}`,
        text: action.text,
      });
    case "approval_sent": {
      const conv = state.convs[action.convId];
      if (!conv) {
        return state;
      }
      const pending = conv.pending.filter((p) => p.requestId !== action.requestId);
      if (pending.length === conv.pending.length) {
        return state;
      }
      return {
        ...state,
        convs: { ...state.convs, [action.convId]: { ...conv, pending } },
      };
    }
    case "clear_error":
      return state.lastError ? { ...state, lastError: null } : state;
  }
}
