// 输入草稿存储（v1.42）：驾驶舱的草稿按「机器 + 会话」隔离。
//
// 此前输入框文本 / 待发图片是 Composer 的两个纯组件内 state，而 Composer 全局
// 只有一个实例——刷新即丢；切换会话时草稿原样带到另一个会话（文字和已选图片
// 都会），容易发错会话。
//
// 归属与持久化的取舍：
// - 归属键 = 机器 + 会话（draftKey，与 historyStore 的存储键同形）。
// - 文字草稿持久化到 localStorage（刷新 / iOS 杀进程后可恢复），防抖落盘。
// - 待发图片只在内存里按会话隔离，**不**进 localStorage：压缩后的图片是
//   数百 KB 级的 base64 大对象，落盘既占配额又拖慢每次写入；刷新丢图的代价
//   可接受（图片是「选了还没发」的瞬时附件，重选成本远低于重打一段文字）。
// - 存储不可用（隐私模式 / 配额超限 / storage 被禁用）一律静默降级：内存草稿
//   仍可用，只是不再跨刷新恢复。
//
// 职责边界：输入框的文本仍是 Composer 的局部 state（不提升到 App——那会让
// 每个按键重渲染整个 App，含消息列表）；本模块只做「按会话存取 + 落盘」，
// 不依赖 React。不变量：**store 永远是当前会话的最新值**（Composer 每次写入
// 先落 store 再同步自己的 state），所以切换会话时不需要「先把旧的存回去」。

import type { MediaPayload } from "./protocol";

/** localStorage 键（JSON 对象：草稿键 → 文本）。 */
export const DRAFTS_KEY = "imagent.drafts";

/** 单条草稿上限（按 Unicode 码点截断，中文 / emoji 不劈半）。输入框本身不限
 *  长度，这里只是防御性的存储护栏：一条超长草稿不应挤占 localStorage 配额。 */
export const DRAFT_MAX_CHARS = 20_000;

/** 最多保留的会话数（LRU，淘汰最久未写入的）。 */
export const DRAFTS_MAX_ENTRIES = 100;

/** 防抖落盘间隔（ms）：停止输入后这么久才写盘——连续打字不逐键写 localStorage。 */
const PERSIST_DELAY_MS = 500;

/** 草稿集合（键 → 文本；对象键序即 LRU 序，最久未使用在前）。 */
export type DraftTexts = Record<string, string>;

/** localStorage 的最小读写面（便于测试注入内存实现）。 */
export type StorageLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** 草稿归属键：`${machine}||${convId}`（与 historyStore 的存储键同形；两者都
 *  不含 `||`——machine 约束为 [A-Za-z0-9_-]，convId 是 `<前缀>:<名>`）。 */
export function draftKey(machine: string, convId: string): string {
  return `${machine}||${convId}`;
}

/** 按码点截断到 DRAFT_MAX_CHARS。长度未超时原样返回：String.length 是 UTF-16
 *  单元数、是码点数的上界，单元数不超则码点数必不超，无需展开。 */
function clipDraft(text: string): string {
  if (text.length <= DRAFT_MAX_CHARS) {
    return text;
  }
  return Array.from(text).slice(0, DRAFT_MAX_CHARS).join("");
}

/** 解析存储里的草稿集合。存储内容不可信（手改 / 旧版本 / 损坏），逐项净化：
 *  畸形 JSON / 非对象 → {}；非字符串值、空串条目丢弃；超长按码点截断。 */
export function parseDrafts(raw: string | null): DraftTexts {
  if (!raw) {
    return {};
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return {};
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return {};
  }
  const out: DraftTexts = {};
  for (const [key, value] of Object.entries(parsed as Record<string, unknown>)) {
    if (typeof value !== "string" || value === "") {
      continue;
    }
    out[key] = clipDraft(value);
  }
  return out;
}

/** 序列化草稿集合：空集合 → null（调用方据此删除存储键，不留空对象占位）；
 *  空串条目不落盘。键序保持插入序（即 LRU 序）。 */
export function serializeDrafts(drafts: DraftTexts): string | null {
  const entries = Object.entries(drafts).filter(([, value]) => value !== "");
  if (entries.length === 0) {
    return null;
  }
  return JSON.stringify(Object.fromEntries(entries));
}

/** 草稿存储（Composer 的写入都经它；App 层创建一份、整棵树共用）。 */
export interface DraftStore {
  /** 某会话的文字草稿（无则空串）。 */
  text(key: string): string;
  /** 某会话的待发图片（无则 null；仅内存）。 */
  media(key: string): MediaPayload | null;
  /** 写入文字草稿。空串 = 删除该条；按码点截断到 DRAFT_MAX_CHARS；标记为
   *  最近使用；调度一次防抖落盘。 */
  setText(key: string, text: string): void;
  /** 以 store 里的当前值为准做函数式追加。语音识别结果等异步回调拿不到最新
   *  的组件 state（闭包里是旧值），追加必须以 store 为准，否则会覆盖掉期间
   *  手动输入的内容。返回空串同样 = 删除该条。 */
  appendText(key: string, fn: (prev: string) => string): void;
  /** 写入 / 清除待发图片（仅内存，不落盘）。 */
  setMedia(key: string, media: MediaPayload | null): void;
  /** 清除某会话的草稿（发送成功后 / 会话被删除时）：文字与图片一并清。 */
  clear(key: string): void;
  /** 立即落盘（取消未到点的防抖，同步写一次）。 */
  flush(): void;
  /** 清空内存与 storage（登出 / 认证失败：与消息缓存同步清本机痕迹）。 */
  clearAll(): void;
}

/**
 * 创建草稿存储。
 * - opts.storage：注入存储实现；传 null = 无持久化；**不传**则每次访问时才
 *   惰性取全局 localStorage（不存在 / 访问即抛 → 视为 null），便于测试注入。
 * - opts.persistDelayMs：防抖间隔（测试注入 0 以外的值来验证防抖）。
 * 所有存储读写都 try/catch：存储失败不影响内存草稿。
 */
export function createDraftStore(
  opts?: { storage?: StorageLike | null; persistDelayMs?: number },
): DraftStore {
  const delay = opts?.persistDelayMs ?? PERSIST_DELAY_MS;

  const storageOf = (): StorageLike | null => {
    if (opts?.storage !== undefined) {
      return opts.storage;
    }
    try {
      return typeof localStorage === "undefined" ? null : localStorage;
    } catch {
      return null; // 访问 localStorage 属性本身抛错（浏览器禁用了存储）
    }
  };

  /** 文字草稿（Map 插入序即 LRU 序：末尾 = 最近写入）。 */
  const texts = new Map<string, string>();
  /** 待发图片（仅内存）。 */
  const medias = new Map<string, MediaPayload>();

  /** LRU 淘汰：超过 DRAFTS_MAX_ENTRIES 时从最久未写入（Map 首个键）起删。 */
  const evict = (): void => {
    while (texts.size > DRAFTS_MAX_ENTRIES) {
      const oldest = texts.keys().next().value;
      if (oldest === undefined) {
        break;
      }
      texts.delete(oldest);
    }
  };

  // 启动时读一次既有草稿（单写者假设：同一页面生命周期内不再重读）。
  try {
    const storage = storageOf();
    const loaded = storage ? parseDrafts(storage.getItem(DRAFTS_KEY)) : {};
    for (const [key, value] of Object.entries(loaded)) {
      texts.set(key, value);
    }
  } catch {
    // 读失败（存储被禁用等）按空草稿起步
  }
  evict();

  let timer: ReturnType<typeof setTimeout> | null = null;
  const cancelTimer = (): void => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };
  /** 同步写一次（空集合 = 删除存储键）。失败静默——内存草稿仍可用。 */
  const writeNow = (): void => {
    try {
      const storage = storageOf();
      if (!storage) {
        return;
      }
      const json = serializeDrafts(Object.fromEntries(texts));
      if (json === null) {
        storage.removeItem(DRAFTS_KEY);
      } else {
        storage.setItem(DRAFTS_KEY, json);
      }
    } catch {
      // 隐私模式 / 配额超限 / 存储被禁用：静默降级
    }
  };
  /** 防抖：每次写入重新计时，停止写入 delay 毫秒后落盘。 */
  const schedulePersist = (): void => {
    cancelTimer();
    timer = setTimeout(() => {
      timer = null;
      writeNow();
    }, delay);
  };

  const setText = (key: string, text: string): void => {
    const clipped = clipDraft(text);
    // 先删再插：把该键移到 Map 末尾（最近使用）
    texts.delete(key);
    if (clipped !== "") {
      texts.set(key, clipped);
    }
    evict();
    schedulePersist();
  };

  return {
    text: (key) => texts.get(key) ?? "",
    media: (key) => medias.get(key) ?? null,
    setText,
    appendText: (key, fn) => setText(key, fn(texts.get(key) ?? "")),
    setMedia: (key, media) => {
      if (media === null) {
        medias.delete(key);
      } else {
        medias.set(key, media);
      }
    },
    clear: (key) => {
      medias.delete(key);
      if (texts.delete(key)) {
        schedulePersist();
      }
    },
    flush: () => {
      cancelTimer();
      writeNow();
    },
    clearAll: () => {
      cancelTimer();
      texts.clear();
      medias.clear();
      try {
        storageOf()?.removeItem(DRAFTS_KEY);
      } catch {
        // 同上：存储失败静默
      }
    },
  };
}
