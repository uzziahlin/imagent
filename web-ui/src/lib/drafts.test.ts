// lib/drafts 单测（v1.42 输入草稿：按「机器 + 会话」隔离 + localStorage 防抖
// 落盘）。存储经 createDraftStore({ storage }) 注入内存实现；防抖用假时钟。
// 覆盖：parse / serialize 往返与净化、LRU 淘汰、存储失败静默、防抖与 flush、
// appendText 以 store 值为准、clearAll、启动载入、缺省存储的惰性取用。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  DRAFT_MAX_CHARS,
  DRAFTS_KEY,
  DRAFTS_MAX_ENTRIES,
  createDraftStore,
  draftKey,
  parseDrafts,
  serializeDrafts,
  type StorageLike,
} from "./drafts";

/** 内存存储：记录每次写入（"set:<键>" / "remove:<键>"），可注入抛错。 */
function memoryStorage(seed: Record<string, string> = {}) {
  const store = new Map<string, string>(Object.entries(seed));
  const writes: string[] = [];
  const storage: StorageLike = {
    getItem: (k) => store.get(k) ?? null,
    setItem: (k, v) => {
      writes.push(`set:${k}`);
      store.set(k, v);
    },
    removeItem: (k) => {
      writes.push(`remove:${k}`);
      store.delete(k);
    },
  };
  return { storage, store, writes };
}

const IMG = { name: "a.jpg", mime: "image/jpeg", b64: "QUJD" };
const KEY_A = draftKey("home-mac", "web:a");

beforeEach(() => {
  vi.useFakeTimers();
  localStorage.clear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("draftKey / parseDrafts / serializeDrafts（纯函数）", () => {
  it("draftKey：`${machine}||${convId}`，与 historyStore 的存储键同形", () => {
    expect(draftKey("home-mac", "web:s-ab12")).toBe("home-mac||web:s-ab12");
    expect(draftKey("home-mac", "peer:win-a")).toBe("home-mac||peer:win-a");
    // 同名会话在不同机器上是两个键（归属 = 机器 + 会话）
    expect(draftKey("home-mac", "web:work")).not.toBe(draftKey("work-mac", "web:work"));
  });

  it("parse ↔ serialize 往返：多条草稿原样恢复，键序保持", () => {
    const raw = serializeDrafts({ [KEY_A]: "你好", "work-mac||web:b": "second line\n" });
    expect(raw).not.toBeNull();
    expect(parseDrafts(raw)).toEqual({ [KEY_A]: "你好", "work-mac||web:b": "second line\n" });
    expect(Object.keys(parseDrafts(raw!))).toEqual([KEY_A, "work-mac||web:b"]);
  });

  it("畸形 JSON → {}：非 JSON 文本 / JSON 标量 / 数组 / null / 空串", () => {
    expect(parseDrafts(null)).toEqual({});
    expect(parseDrafts("")).toEqual({});
    expect(parseDrafts("{not json")).toEqual({});
    expect(parseDrafts("42")).toEqual({});
    expect(parseDrafts('"text"')).toEqual({});
    expect(parseDrafts("[]")).toEqual({});
    expect(parseDrafts("null")).toEqual({});
    expect(parseDrafts("true")).toEqual({});
  });

  it("非字符串值丢弃（数字 / 对象 / null / 布尔），字符串条目保留", () => {
    expect(
      parseDrafts(
        JSON.stringify({ a: "ok", b: 1, c: { x: 1 }, d: null, e: true, f: ["x"] }),
      ),
    ).toEqual({ a: "ok" });
  });

  it("空串条目丢弃（parse 侧与 serialize 侧都一样，不落盘）", () => {
    expect(parseDrafts('{"a":"","b":"x"}')).toEqual({ b: "x" });
    expect(serializeDrafts({ a: "", b: "" })).toBeNull();
    // 只有空串条目 → null（调用方据此删存储键）
    expect(parseDrafts(JSON.stringify({ a: "" }))).toEqual({});
  });

  it("serialize：空集合 → null；非空 → JSON 字符串", () => {
    expect(serializeDrafts({})).toBeNull();
    expect(serializeDrafts({ k: "v" })).toBe('{"k":"v"}');
  });

  it("超长草稿按码点截断到 DRAFT_MAX_CHARS（代理对不被劈半）", () => {
    // 20001 个 BMP 字符
    const long = "字".repeat(DRAFT_MAX_CHARS + 1);
    const clipped = parseDrafts(JSON.stringify({ a: long })).a;
    expect(Array.from(clipped)).toHaveLength(DRAFT_MAX_CHARS);
    expect(clipped.length).toBe(DRAFT_MAX_CHARS);
    // emoji（代理对）：按码点截断，末尾不会留下半个代理对
    const emoji = "😀".repeat(DRAFT_MAX_CHARS + 5);
    const clippedEmoji = parseDrafts(JSON.stringify({ a: emoji })).a;
    expect(Array.from(clippedEmoji)).toHaveLength(DRAFT_MAX_CHARS);
    expect(clippedEmoji.endsWith("😀")).toBe(true);
    expect(clippedEmoji.length).toBe(DRAFT_MAX_CHARS * 2);
    // 恰到上限不截断；上限内含 emoji 的混合文本原样
    expect(parseDrafts(JSON.stringify({ a: "字".repeat(DRAFT_MAX_CHARS) })).a).toHaveLength(DRAFT_MAX_CHARS);
    const mixed = "a😀中".repeat(100);
    expect(parseDrafts(JSON.stringify({ a: mixed })).a).toBe(mixed);
  });
});

describe("createDraftStore：按会话隔离的读写", () => {
  it("text / setText：各会话独立；空串 = 删除该条", () => {
    const store = createDraftStore({ storage: null });
    expect(store.text(KEY_A)).toBe("");
    store.setText(KEY_A, "草稿 A");
    store.setText("work-mac||web:work", "草稿 B");
    expect(store.text(KEY_A)).toBe("草稿 A");
    expect(store.text("work-mac||web:work")).toBe("草稿 B");
    store.setText(KEY_A, "");
    expect(store.text(KEY_A)).toBe("");
    expect(store.text("work-mac||web:work")).toBe("草稿 B");
  });

  it("setText 按码点截断到上限；截断后以截断值为准", () => {
    const store = createDraftStore({ storage: null });
    store.setText(KEY_A, "😀".repeat(DRAFT_MAX_CHARS + 3));
    expect(Array.from(store.text(KEY_A))).toHaveLength(DRAFT_MAX_CHARS);
  });

  it("appendText 以 store 里的当前值为准（不被闭包里的旧 state 覆盖）", () => {
    const store = createDraftStore({ storage: null });
    store.setText(KEY_A, "已输入");
    // 模拟语音识别回调：它拿到的「prev」只能是 store 里的值
    store.appendText(KEY_A, (prev) => (prev ? `${prev} 识别结果` : "识别结果"));
    expect(store.text(KEY_A)).toBe("已输入 识别结果");
    // 期间用户又手动输入了一段：下一次追加仍接在最新的后面，不丢字
    store.setText(KEY_A, `${store.text(KEY_A)} 补充`);
    store.appendText(KEY_A, (prev) => `${prev}!`);
    expect(store.text(KEY_A)).toBe("已输入 识别结果 补充!");
    // 空草稿上追加（prev 为空串）
    store.appendText("m||web:empty", (prev) => (prev ? `${prev} x` : "x"));
    expect(store.text("m||web:empty")).toBe("x");
    // fn 返回空串 = 删除该条
    store.appendText("m||web:empty", () => "");
    expect(store.text("m||web:empty")).toBe("");
  });

  it("media：仅内存、按会话隔离；null = 清除；不进 storage", () => {
    const { storage, store, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    expect(s.media(KEY_A)).toBeNull();
    s.setMedia(KEY_A, IMG);
    expect(s.media(KEY_A)).toBe(IMG);
    expect(s.media("work-mac||web:work")).toBeNull();
    s.setMedia(KEY_A, null);
    expect(s.media(KEY_A)).toBeNull();
    s.setMedia("work-mac||web:work", IMG);
    s.setText(KEY_A, "x");
    vi.advanceTimersByTime(500);
    expect(writes).toEqual([`set:${DRAFTS_KEY}`]);
    expect(JSON.parse(store.get(DRAFTS_KEY)!)).toEqual({ [KEY_A]: "x" }); // 图片不在里面
  });

  it("clear(key)：文字与图片一并清，别的会话不动，落盘后存储里不再有该键", () => {
    const { storage, store, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    s.setText(KEY_A, "A");
    s.setText("m||web:b", "B");
    s.setMedia(KEY_A, IMG);
    s.flush();
    s.clear(KEY_A);
    expect(s.text(KEY_A)).toBe("");
    expect(s.media(KEY_A)).toBeNull();
    expect(s.text("m||web:b")).toBe("B");
    vi.advanceTimersByTime(500);
    expect(JSON.parse(store.get(DRAFTS_KEY)!)).toEqual({ "m||web:b": "B" });
    // clear 一个不存在的文字条目：不调度无谓的落盘
    writes.length = 0;
    s.clear("m||web:none");
    vi.advanceTimersByTime(500);
    expect(writes).toEqual([]);
  });

  it("空串删除条目后：再 clear 该键不触发无谓落盘（条目已不存在）", () => {
    const { storage, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    s.setText("m||web:x", "a");
    s.setText("m||web:x", ""); // 删除条目
    vi.advanceTimersByTime(500); // 这次删除的落盘完成
    writes.length = 0;
    s.clear("m||web:x"); // 条目已不在表里：不应再调度一次落盘
    vi.advanceTimersByTime(500);
    expect(writes).toEqual([]);
  });

  it("启动时载入 storage 里的既有草稿（刷新 / 杀进程后恢复的来源）", () => {
    const { storage } = memoryStorage({ [DRAFTS_KEY]: JSON.stringify({ [KEY_A]: "刷新前的草稿" }) });
    const s = createDraftStore({ storage });
    expect(s.text(KEY_A)).toBe("刷新前的草稿");
    // 图片不在 storage 里：恢复不出来（取舍见文件头注）
    expect(s.media(KEY_A)).toBeNull();
  });

  it("缺省 storage：惰性取全局 localStorage（创建时不读，首次访问时才取）", () => {
    const s = createDraftStore();
    expect(s.text(KEY_A)).toBe(""); // 创建时 storage 里还没有
    localStorage.setItem(DRAFTS_KEY, JSON.stringify({ [KEY_A]: "后写入的" }));
    // 未传 storage → 每次访问时取全局：写入会与既有内容合并覆盖
    s.setText("m||web:new", "新草稿");
    vi.advanceTimersByTime(500);
    // 内存里没有 KEY_A（创建时已读过一次、为空），落盘的是内存态
    expect(JSON.parse(localStorage.getItem(DRAFTS_KEY)!)).toEqual({ "m||web:new": "新草稿" });
  });

  it("缺省 storage 且全局无 localStorage：视同无持久化，内存草稿照常可用", () => {
    vi.stubGlobal("localStorage", undefined);
    try {
      const s = createDraftStore();
      s.setText(KEY_A, "仅内存");
      expect(s.text(KEY_A)).toBe("仅内存");
      expect(() => vi.advanceTimersByTime(500)).not.toThrow();
      expect(() => s.flush()).not.toThrow();
      expect(() => s.clearAll()).not.toThrow();
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

describe("createDraftStore：防抖落盘 / flush / clearAll", () => {
  it("防抖：写入后 500ms 内不落盘；停止写入满 500ms 才写一次", () => {
    const { storage, store, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    s.setText(KEY_A, "a");
    vi.advanceTimersByTime(499);
    expect(writes).toEqual([]);
    expect(store.has(DRAFTS_KEY)).toBe(false);
    vi.advanceTimersByTime(1);
    expect(writes).toEqual([`set:${DRAFTS_KEY}`]);
    expect(JSON.parse(store.get(DRAFTS_KEY)!)).toEqual({ [KEY_A]: "a" });
  });

  it("防抖：连续写入重新计时——打字期间不落盘，停手后只写最后一次", () => {
    const { storage, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    s.setText(KEY_A, "a");
    for (let i = 0; i < 6; i++) {
      vi.advanceTimersByTime(300);
      s.setText(KEY_A, `${i}`); // 每 300ms 一次按键：始终不满 500ms
    }
    expect(writes).toEqual([]);
    vi.advanceTimersByTime(500);
    expect(writes).toEqual([`set:${DRAFTS_KEY}`]);
    expect(s.text(KEY_A)).toBe("5");
  });

  it("persistDelayMs 可注入：按注入值防抖", () => {
    const { storage, writes } = memoryStorage();
    const s = createDraftStore({ storage, persistDelayMs: 2_000 });
    s.setText(KEY_A, "a");
    vi.advanceTimersByTime(1_999);
    expect(writes).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(writes).toEqual([`set:${DRAFTS_KEY}`]);
  });

  it("flush 立即落盘，并取消未到点的防抖（之后不会重复写）", () => {
    const { storage, store, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    s.setText(KEY_A, "a");
    s.flush();
    expect(writes).toEqual([`set:${DRAFTS_KEY}`]);
    expect(JSON.parse(store.get(DRAFTS_KEY)!)).toEqual({ [KEY_A]: "a" });
    vi.advanceTimersByTime(5_000);
    expect(writes).toEqual([`set:${DRAFTS_KEY}`]); // 防抖定时器已被取消
  });

  it("最后一条草稿被删空 → 落盘时删除存储键（不留空对象）", () => {
    const { storage, store, writes } = memoryStorage();
    const s = createDraftStore({ storage });
    s.setText(KEY_A, "a");
    s.flush();
    expect(store.has(DRAFTS_KEY)).toBe(true);
    writes.length = 0;
    s.setText(KEY_A, "");
    vi.advanceTimersByTime(500);
    expect(writes).toEqual([`remove:${DRAFTS_KEY}`]);
    expect(store.has(DRAFTS_KEY)).toBe(false);
  });

  it("clearAll：清空内存草稿与图片、删存储键、取消未到点的防抖；之后内存为空", () => {
    const { storage, store, writes } = memoryStorage({ [DRAFTS_KEY]: JSON.stringify({ [KEY_A]: "旧草稿" }) });
    const s = createDraftStore({ storage });
    s.setText("m||web:b", "b");
    s.setMedia(KEY_A, IMG);
    s.clearAll();
    expect(store.has(DRAFTS_KEY)).toBe(false);
    expect(s.text(KEY_A)).toBe("");
    expect(s.text("m||web:b")).toBe("");
    expect(s.media(KEY_A)).toBeNull();
    writes.length = 0;
    vi.advanceTimersByTime(5_000);
    expect(writes).toEqual([]); // 不会有迟到的落盘把已清空的写回
    // 清空后仍可继续使用
    s.setText(KEY_A, "重新开始");
    expect(s.text(KEY_A)).toBe("重新开始");
  });

  it("clearAll 后 storage 里预置的其它键不受影响（只删草稿键）", () => {
    const { storage, store } = memoryStorage({ other: "keep", [DRAFTS_KEY]: "{}" });
    createDraftStore({ storage }).clearAll();
    expect(store.get("other")).toBe("keep");
    expect(store.has(DRAFTS_KEY)).toBe(false);
  });
});

describe("createDraftStore：LRU 淘汰", () => {
  it("超过 DRAFTS_MAX_ENTRIES 个会话时淘汰最久未写入的", () => {
    const s = createDraftStore({ storage: null });
    for (let i = 0; i < DRAFTS_MAX_ENTRIES; i++) {
      s.setText(`m||web:c${i}`, `t${i}`);
    }
    expect(s.text("m||web:c0")).toBe("t0");
    s.setText("m||web:new", "新"); // 第 101 个：淘汰最久的 c0
    expect(s.text("m||web:c0")).toBe("");
    expect(s.text("m||web:c1")).toBe("t1");
    expect(s.text("m||web:new")).toBe("新");
  });

  it("重新写入即「最近使用」：刚写过的会话不会被淘汰", () => {
    const s = createDraftStore({ storage: null });
    for (let i = 0; i < DRAFTS_MAX_ENTRIES; i++) {
      s.setText(`m||web:c${i}`, `t${i}`);
    }
    s.setText("m||web:c0", "t0 更新"); // c0 移到最近
    s.setText("m||web:new", "新");
    expect(s.text("m||web:c0")).toBe("t0 更新"); // 保住
    expect(s.text("m||web:c1")).toBe(""); // 淘汰的是次久的 c1
  });

  it("appendText 同样算一次写入（刷新 LRU 序）", () => {
    const s = createDraftStore({ storage: null });
    for (let i = 0; i < DRAFTS_MAX_ENTRIES; i++) {
      s.setText(`m||web:c${i}`, `t${i}`);
    }
    s.appendText("m||web:c0", (prev) => `${prev}+`);
    s.setText("m||web:new", "新");
    expect(s.text("m||web:c0")).toBe("t0+");
    expect(s.text("m||web:c1")).toBe("");
  });

  // 空串 = 删除条目，而不是在表里留一个空串占位。text() 的返回值区分不出这
  // 两种实现（都是空串），落盘内容也区分不出（serialize 会滤掉空串）——唯一
  // 的可观察差异是 LRU 名额：占位会占名额，把真正有草稿的会话挤出去。
  it("空串入参不占 LRU 名额：对没有草稿的会话写入空串，不会挤掉别的会话草稿", () => {
    const s = createDraftStore({ storage: null });
    for (let i = 0; i < DRAFTS_MAX_ENTRIES; i++) {
      s.setText(`m||web:c${i}`, `t${i}`);
    }
    // 真实路径：纯图片消息的会话本就没有文字草稿，发送成功后 submit 仍会写入空串
    s.setText("m||web:image-only", "");
    expect(s.text("m||web:image-only")).toBe("");
    // 空串不入表：名额仍是 100，最旧的 c0 没有被挤掉
    expect(s.text("m||web:c0")).toBe("t0");
    expect(s.text(`m||web:c${DRAFTS_MAX_ENTRIES - 1}`)).toBe(`t${DRAFTS_MAX_ENTRIES - 1}`);
  });

  it("清空已有草稿即释放 LRU 名额：之后再写新草稿不会挤掉别的会话", () => {
    const s = createDraftStore({ storage: null });
    for (let i = 0; i < DRAFTS_MAX_ENTRIES; i++) {
      s.setText(`m||web:c${i}`, `t${i}`);
    }
    s.setText("m||web:c0", ""); // 清空最旧的会话
    expect(s.text("m||web:c0")).toBe("");
    s.setText("m||web:new", "新");
    expect(s.text("m||web:c1")).toBe("t1"); // 名额已释放：c1 不被淘汰
    expect(s.text("m||web:new")).toBe("新");
  });

  it("启动载入时存储里的条目数超限 → 载入后即淘汰到上限（按存储键序，最久在前）", () => {
    const seed: Record<string, string> = {};
    for (let i = 0; i < DRAFTS_MAX_ENTRIES + 20; i++) {
      seed[`m||web:c${i}`] = `t${i}`;
    }
    const { storage } = memoryStorage({ [DRAFTS_KEY]: JSON.stringify(seed) });
    const s = createDraftStore({ storage });
    // 淘汰最旧的 20 条（c0–c19），保留 c20–c119 共 100 条
    expect(s.text("m||web:c0")).toBe("");
    expect(s.text("m||web:c19")).toBe("");
    expect(s.text("m||web:c20")).toBe("t20");
    expect(s.text(`m||web:c${DRAFTS_MAX_ENTRIES + 19}`)).toBe(`t${DRAFTS_MAX_ENTRIES + 19}`);
  });

  it("淘汰后的落盘只含留下的条目", () => {
    const { storage, store } = memoryStorage();
    const s = createDraftStore({ storage });
    for (let i = 0; i <= DRAFTS_MAX_ENTRIES; i++) {
      s.setText(`m||web:c${i}`, `t${i}`);
    }
    vi.advanceTimersByTime(500);
    const saved = JSON.parse(store.get(DRAFTS_KEY)!) as Record<string, string>;
    expect(Object.keys(saved)).toHaveLength(DRAFTS_MAX_ENTRIES);
    expect(saved["m||web:c0"]).toBeUndefined();
  });
});

describe("createDraftStore：存储失败静默降级", () => {
  /** 各方法一调用即抛的 storage。 */
  function throwingStorage(): StorageLike {
    const boom = (): never => {
      throw new Error("QuotaExceededError");
    };
    return { getItem: boom, setItem: boom, removeItem: boom };
  }

  it("读抛错（启动载入）：按空草稿起步，不上抛", () => {
    const s = createDraftStore({ storage: throwingStorage() });
    expect(s.text(KEY_A)).toBe("");
    s.setText(KEY_A, "内存可用");
    expect(s.text(KEY_A)).toBe("内存可用");
  });

  it("写 / 删抛错：防抖到点与 flush 都不上抛，内存草稿仍在", () => {
    const s = createDraftStore({ storage: throwingStorage() });
    s.setText(KEY_A, "a");
    expect(() => vi.advanceTimersByTime(500)).not.toThrow(); // 防抖到点的 setItem 抛错
    expect(() => s.flush()).not.toThrow();
    expect(s.text(KEY_A)).toBe("a"); // 存储失败不影响内存草稿
    s.setText(KEY_A, "");
    expect(() => vi.advanceTimersByTime(500)).not.toThrow(); // removeItem 抛错
    expect(s.text(KEY_A)).toBe("");
    expect(() => s.clearAll()).not.toThrow();
  });

  it("getItem 返回 null（首次使用）：空草稿起步，不误判为畸形", () => {
    const { storage, store } = memoryStorage();
    const s = createDraftStore({ storage });
    expect(s.text(KEY_A)).toBe("");
    s.setText(KEY_A, "a");
    vi.advanceTimersByTime(500);
    expect(JSON.parse(store.get(DRAFTS_KEY)!)).toEqual({ [KEY_A]: "a" });
  });

  it("访问全局 localStorage 属性本身抛错（存储被禁用）：视同无持久化", () => {
    vi.stubGlobal("localStorage", undefined);
    Object.defineProperty(globalThis, "localStorage", {
      get(): Storage {
        throw new DOMException("The operation is insecure.", "SecurityError");
      },
      configurable: true,
    });
    try {
      const s = createDraftStore();
      s.setText(KEY_A, "仅内存");
      expect(s.text(KEY_A)).toBe("仅内存");
      expect(() => vi.advanceTimersByTime(500)).not.toThrow();
    } finally {
      delete (globalThis as { localStorage?: unknown }).localStorage;
      vi.unstubAllGlobals();
    }
  });
});
