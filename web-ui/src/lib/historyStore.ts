// 消息本地缓存（v1.7.x 刷新恢复）：IndexedDB 持久化每 conv 最近消息，
// App 挂载时 loadAll 预填 spaces（见 restoreHistory），state 变化后经
// debounce 落盘（App 侧 2s，只写 entries 引用有变化的 conv）。
//
// 存储形态：db `imagent-web` v1，objectStore `entries`（无 keyPath，显式
// key = `${machine}||${convId}`——两者均不含 `||`：machine 约束
// [a-zA-Z0-9_-]、conv 为 web:/local: 前缀名，拆解安全），value
// `{entries, savedAt}`。card 存当前最新整卡（含 streaming 中间态，恢复时
// 显示最后快照即可；重连 sync 按 messageId 覆盖兜底终态）。
// v1.8 会话历史重放：value 增加可选 `lastSeq`（网关 outbound_log 该 conv 的
// 本地高水位）——history 帧到达时随 debounce 与 entries 一同落盘；旧记录无
// 该字段读作 undefined（从头拉取）。
//
// 后端注入：jsdom 无 IndexedDB，单测经 setHistoryBackend 注入内存 Map 后端
// （memoryHistoryBackend）；宿主无 IndexedDB（隐私模式等）自动降级 no-op
// ——全部 API 静默空操作，主链路（消息流/连接）完全不受影响。

import {
  CONV_TITLE_MAX_CHARS,
  DEFAULT_CONV,
  type AppState,
  type ConvState,
  type MachineSpace,
  type StreamEntry,
} from "./protocol";

/** 每 conv 持久化容量上限：超出裁掉最早的（内存护栏 MAX_STREAM_ENTRIES=200
 *  更宽，本地缓存收紧到最近 120 条——刷新恢复“最近上下文”够用且控容量）。 */
export const HISTORY_MAX_ENTRIES = 120;

/** 存储键分隔符（machine 与 convId 均不含该序列，拆解无歧义）。 */
const KEY_SEP = "||";

/** 恢复条目的 React key 前缀：恢复条目与新到条目（user:N/text:N 序号键）
 *  的 key 空间隔离，避免恢复后 seq 归零导致键冲突。 */
const RESTORED_KEY_PREFIX = "h:";

const DB_NAME = "imagent-web";
const DB_VERSION = 1;
const STORE = "entries";

/** 每条存储记录：entries（已裁剪至最近 HISTORY_MAX_ENTRIES 条）+ savedAt +
 *  v1.8 起可选 lastSeq（该 conv 的历史重放高水位；旧记录无此字段）。 */
export type StoredConv = { entries: StreamEntry[]; savedAt: number; lastSeq?: number };

/** loadAll 聚合输出的单 conv 记录（savedAt 仅落盘元数据，读出侧不带）。 */
export type RestoredConv = { entries: StreamEntry[]; lastSeq?: number };

/** 存储后端接口（默认 IndexedDB 实现；单测注入内存 Map；不可用降级 no-op）。 */
export interface HistoryBackend {
  get(key: string): Promise<StoredConv | undefined>;
  put(key: string, value: StoredConv): Promise<void>;
  getAll(): Promise<[string, StoredConv][]>;
  clear(): Promise<void>;
}

// ---------------------------------------------------------------------------
// 后端实现
// ---------------------------------------------------------------------------

/** 内存 Map 后端（单测注入用；getAll 按插入序返回）。 */
export function memoryHistoryBackend(): HistoryBackend {
  const map = new Map<string, StoredConv>();
  return {
    async get(key) {
      return map.get(key);
    },
    async put(key, value) {
      map.set(key, value);
    },
    async getAll() {
      return Array.from(map.entries());
    },
    async clear() {
      map.clear();
    },
  };
}

/** no-op 后端（无 IndexedDB 环境降级：静默空操作）。 */
function noopBackend(): HistoryBackend {
  return {
    async get() {
      return undefined;
    },
    async put() {},
    async getAll() {
      return [];
    },
    async clear() {},
  };
}

/** IndexedDB 后端：db 单例惰性打开；每次操作独立事务（低频写，无事务复用
 *  必要）。open 失败（隐私模式等）由调用侧 catch → 已解析的后端仍持有
 *  rejected promise，后续操作全部静默失败——等价 no-op。 */
function idbBackend(): HistoryBackend {
  const dbPromise: Promise<IDBDatabase> = new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(STORE)) {
        req.result.createObjectStore(STORE); // 无 keyPath：put 显式 key
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  const withStore = <T>(
    mode: IDBTransactionMode,
    run: (store: IDBObjectStore) => IDBRequest<T> | undefined,
  ): Promise<T | undefined> =>
    dbPromise.then(
      (db) =>
        new Promise<T | undefined>((resolve, reject) => {
          const tx = db.transaction(STORE, mode);
          const req = run(tx.objectStore(STORE));
          tx.oncomplete = () => resolve(req?.result);
          tx.onabort = () => reject(tx.error);
          tx.onerror = () => reject(tx.error);
        }),
    );
  return {
    get: (key) => withStore("readonly", (s) => s.get(key) as IDBRequest<StoredConv | undefined>),
    put: async (key, value) => {
      await withStore("readwrite", (s) => {
        s.put(value, key);
        return undefined;
      });
    },
    getAll: async () => {
      const [values, keys] = await Promise.all([
        withStore("readonly", (s) => s.getAll() as IDBRequest<StoredConv[]>),
        withStore("readonly", (s) => s.getAllKeys() as IDBRequest<IDBValidKey[]>),
      ]);
      const out: [string, StoredConv][] = [];
      (keys ?? []).forEach((k, i) => {
        if (typeof k === "string" && values?.[i] !== undefined) {
          out.push([k, values[i]]);
        }
      });
      return out;
    },
    clear: async () => {
      await withStore("readwrite", (s) => {
        s.clear();
        return undefined;
      });
    },
  };
}

// 后端解析：pinned（测试注入）优先；否则按环境一次性解析（jsdom 无
// indexedDB → no-op，浏览器 → IndexedDB）。
let pinned: HistoryBackend | null = null;
let resolved: HistoryBackend | null = null;

function backend(): HistoryBackend {
  if (pinned) {
    return pinned;
  }
  resolved ??= typeof indexedDB === "undefined" ? noopBackend() : idbBackend();
  return resolved;
}

/** 注入/还原存储后端（单测）：传 null 还原默认解析（jsdom 下即 no-op）。 */
export function setHistoryBackend(b: HistoryBackend | null): void {
  pinned = b;
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

/** 容量裁剪：超出 HISTORY_MAX_ENTRIES 丢弃最早的（引用不变时原样返回）。 */
function trimEntries(entries: StreamEntry[]): StreamEntry[] {
  return entries.length > HISTORY_MAX_ENTRIES
    ? entries.slice(entries.length - HISTORY_MAX_ENTRIES)
    : entries;
}

function convKey(machine: string, convId: string): string {
  return `${machine}${KEY_SEP}${convId}`;
}

/** 消息本地缓存门面。全部方法静默容错（存储失败不影响主链路）。 */
export const historyStore = {
  /** 全量读取并按 machine/conv 聚合（key 拆解；畸形 key 跳过）。v1.8 起每条
   *  记录带 lastSeq（旧记录无该字段 → undefined）。 */
  async loadAll(): Promise<Record<string, Record<string, RestoredConv>>> {
    try {
      const pairs = await backend().getAll();
      const out: Record<string, Record<string, RestoredConv>> = {};
      for (const [key, val] of pairs) {
        const sep = key.indexOf(KEY_SEP);
        const machine = sep > 0 ? key.slice(0, sep) : "";
        const convId = sep > 0 ? key.slice(sep + KEY_SEP.length) : "";
        if (!machine || !convId) {
          continue;
        }
        (out[machine] ??= {})[convId] = {
          entries: val?.entries ?? [],
          lastSeq: typeof val?.lastSeq === "number" ? val.lastSeq : undefined,
        };
      }
      return out;
    } catch {
      return {};
    }
  },

  /** 保存单 conv（裁剪至最近 HISTORY_MAX_ENTRIES 条；不可变引用不重复写）。
   *  v1.8：lastSeq 为该 conv 的历史重放高水位——undefined 不落键（清回旧
   *  形态），number 落键覆盖。 */
  async saveConv(
    machine: string,
    convId: string,
    entries: StreamEntry[],
    lastSeq?: number,
  ): Promise<void> {
    try {
      await backend().put(convKey(machine, convId), {
        entries: trimEntries(entries),
        savedAt: Date.now(),
        ...(lastSeq !== undefined ? { lastSeq } : {}),
      });
    } catch {
      // 静默：配额/隐私模式下落盘失败不影响内存消息流
    }
  },

  /** 清空全部本地缓存（退出登录清本机痕迹；普通刷新不触发）。 */
  async clear(): Promise<void> {
    try {
      await backend().clear();
    } catch {
      // 静默
    }
  },
};

// ---------------------------------------------------------------------------
// 恢复预填（loadAll 输出 → AppState.spaces；纯函数，reducer 既有语义不动，
// App 经扩展 action 调用）
// ---------------------------------------------------------------------------

/** 条目 → 侧栏单行预览（与 protocol.ts previewOf 同语义的本地实现）。 */
function previewOfEntry(entry: StreamEntry): string {
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

/** 自动命名摘要（与 protocol.ts titleSummary 同语义）：首条 user/text 的
 *  首行按码点截 CONV_TITLE_MAX_CHARS，空摘要返回 undefined（title 不定）。 */
function titleOfEntries(entries: StreamEntry[]): string | undefined {
  for (const e of entries) {
    if (e.kind === "user" || e.kind === "text") {
      const s = Array.from(firstLine(e.text)).slice(0, CONV_TITLE_MAX_CHARS).join("");
      return s.length > 0 ? s : undefined;
    }
  }
  return undefined;
}

/** 末条时间戳：从尾部找最近带 at 的条目（user/text 盖戳；全无则 0）。 */
function lastAtOfEntries(entries: StreamEntry[]): number {
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i];
    if ((e.kind === "user" || e.kind === "text") && typeof e.at === "number") {
      return e.at;
    }
  }
  return 0;
}

const emptyConvState = (): ConvState => ({
  entries: [],
  pending: [],
  lastPreview: "",
  lastAt: 0,
  unread: 0,
});

/** 恢复条目改写 key（前缀隔离）——浅拷贝条目，数组/卡片载荷引用共享
 *  （恢复后即被 state 持有，无二次修改）。 */
const withRestoredKey = (entries: StreamEntry[]): StreamEntry[] =>
  entries.map((e) => ({ ...e, key: `${RESTORED_KEY_PREFIX}${e.key}` }));

/**
 * 本地缓存恢复 → spaces 预填（纯函数）：
 * - 空 entries 的记录跳过；
 * - 本地不存在的 conv：建条目（entries/lastPreview=末条 preview/lastAt=末条
 *   at/unread=0/title 按首条 user/text 派生），convOrder 按 lastAt 降序插
 *   default 之后（与 v1.7 mergePersistedSessions 同序策略）；
 * - 本地已存在的 conv：恢复条目（更旧）垫在既有条目之前，概要/未读以本地
 *   （更新）为准，title 本地未定时由恢复首条补派生——恢复与 WS 帧竞争到达
 *   时（loadAll 异步完成前 hello 流已进）两端消息都保留；
 * - 未知机器先建默认空间再合并；state 无任何改动时原样返回（引用不变）。
 * - v1.8：入参为 loadAll 输出形态（每 conv {entries, lastSeq?}），本函数只
 *   消费 entries——lastSeq 由 App 层读出另行维护（lastSeqRef）。
 */
export function restoreHistory(
  state: AppState,
  restored: Record<string, Record<string, RestoredConv>>,
): AppState {
  const spaces: Record<string, MachineSpace> = { ...state.spaces };
  let changed = false;
  for (const [machine, convs] of Object.entries(restored)) {
    const space = spaces[machine] ?? {
      convs: { [DEFAULT_CONV]: emptyConvState() },
      convOrder: [DEFAULT_CONV],
    };
    const nextConvs: Record<string, ConvState> = { ...space.convs };
    const fresh: { convId: string; lastAt: number }[] = [];
    let machineChanged = false;
    for (const [convId, rec] of Object.entries(convs)) {
      if (rec.entries.length === 0) {
        continue;
      }
      const entries = withRestoredKey(rec.entries);
      const existing = nextConvs[convId];
      const derivedTitle = titleOfEntries(entries);
      const lastAt = lastAtOfEntries(entries);
      machineChanged = true;
      if (!existing) {
        nextConvs[convId] = {
          entries,
          pending: [],
          lastPreview: previewOfEntry(entries[entries.length - 1]),
          lastAt,
          unread: 0,
          ...(derivedTitle !== undefined ? { title: derivedTitle } : {}),
        };
        fresh.push({ convId, lastAt });
      } else if (existing.entries.length === 0) {
        // 空壳（initialState 预置的 default / 快照合并的离线会话）：整体填入
        nextConvs[convId] = {
          ...existing,
          entries,
          lastPreview: previewOfEntry(entries[entries.length - 1]),
          lastAt,
          ...(existing.title === undefined && derivedTitle !== undefined
            ? { title: derivedTitle }
            : {}),
        };
      } else {
        // 帧先到：恢复条目垫前，概要/未读/位置以本地为准，时间取较新侧
        nextConvs[convId] = {
          ...existing,
          entries: [...entries, ...existing.entries],
          lastPreview: existing.lastPreview || previewOfEntry(entries[entries.length - 1]),
          lastAt: Math.max(existing.lastAt, lastAt),
          ...(existing.title === undefined && derivedTitle !== undefined
            ? { title: derivedTitle }
            : {}),
        };
      }
    }
    if (!machineChanged) {
      continue;
    }
    const convOrder =
      fresh.length > 0
        ? (() => {
            const di = space.convOrder.indexOf(DEFAULT_CONV);
            const at = di >= 0 ? di + 1 : 0;
            const ids = fresh.sort((a, b) => b.lastAt - a.lastAt).map((f) => f.convId);
            return [...space.convOrder.slice(0, at), ...ids, ...space.convOrder.slice(at)];
          })()
        : space.convOrder;
    spaces[machine] = { convs: nextConvs, convOrder };
    changed = true;
  }
  return changed ? { ...state, spaces } : state;
}
