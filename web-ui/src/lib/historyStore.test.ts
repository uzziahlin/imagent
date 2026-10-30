// 消息本地缓存（IndexedDB 门面 + 后端注入 + 恢复预填映射）：
// jsdom 无 IndexedDB，全部经 setHistoryBackend 注入内存 Map 后端；降级
// no-op 路径在 jsdom 下即默认后端（typeof indexedDB === "undefined"）。
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  HISTORY_MAX_ENTRIES,
  historyStore,
  memoryHistoryBackend,
  restoreHistory,
  setHistoryBackend,
  type HistoryBackend,
} from "./historyStore";
import { DEFAULT_CONV, DEFAULT_MACHINE, initialState, spaceOf, type StreamEntry } from "./protocol";

/** 最小 card 载荷（preview 派生只读 text/task_digest）。 */
const cardOf = (text: string): StreamEntry => ({
  kind: "card",
  key: "card:m:c:msg1",
  messageId: "msg1",
  card: {
    text,
    tool_calls: [],
    thoughts: [],
    todos: [],
    phase: "outputting",
    run_secs: 5,
    terminal: "running",
  },
  state: "streaming",
});

const userOf = (text: string, at: number): StreamEntry => ({
  kind: "user",
  key: `user:${at}`,
  text,
  at,
});

const textOf = (text: string, at: number): StreamEntry => ({
  kind: "text",
  key: `text:${at}`,
  text,
  at,
});

describe("historyStore（注入内存后端）", () => {
  let backend: HistoryBackend;

  beforeEach(() => {
    backend = memoryHistoryBackend();
    setHistoryBackend(backend);
  });

  afterEach(() => {
    setHistoryBackend(null);
  });

  it("saveConv/loadAll 往返：key 按 machine||conv 拆解聚合", async () => {
    await historyStore.saveConv("home-mac", "web:default", [userOf("你好", 100)]);
    await historyStore.saveConv("home-mac", "web:s-a", [textOf("回复", 200)]);
    await historyStore.saveConv("work-mac", "web:default", [userOf("工作机", 300)]);
    const all = await historyStore.loadAll();
    expect(all).toEqual({
      "home-mac": { "web:default": [userOf("你好", 100)], "web:s-a": [textOf("回复", 200)] },
      "work-mac": { "web:default": [userOf("工作机", 300)] },
    });
  });

  it("每 conv 只保留最近 120 条（更早的裁掉）", async () => {
    const entries = Array.from({ length: 130 }, (_, i) => userOf(`消息${i}`, i + 1));
    await historyStore.saveConv("home-mac", "web:default", entries);
    const stored = await backend.get("home-mac||web:default");
    expect(stored?.entries).toHaveLength(HISTORY_MAX_ENTRIES);
    expect(stored?.entries[0]).toMatchObject({ text: "消息10" }); // 0..9 被裁
    expect(stored?.entries.at(-1)).toMatchObject({ text: "消息129" });
    // 容量内原样保存（引用内容不变）
    await historyStore.saveConv("home-mac", "web:work", entries.slice(0, 5));
    expect((await backend.get("home-mac||web:work"))?.entries).toHaveLength(5);
  });

  it("clear 清空全部；get 不存在的 key 返回 undefined", async () => {
    await historyStore.saveConv("home-mac", "web:default", [userOf("x", 1)]);
    await historyStore.clear();
    expect(await historyStore.loadAll()).toEqual({});
    expect(await backend.get("home-mac||web:default")).toBeUndefined();
  });

  it("畸形 key（无分隔符 / 空段）在 loadAll 聚合时跳过，不抛错", async () => {
    await backend.put("no-sep-key", { entries: [userOf("x", 1)], savedAt: 1 });
    await backend.put("||web:default", { entries: [userOf("x", 1)], savedAt: 1 });
    await backend.put("home-mac||", { entries: [userOf("x", 1)], savedAt: 1 });
    await historyStore.saveConv("home-mac", "web:ok", [userOf("好", 1)]);
    expect(await historyStore.loadAll()).toEqual({ "home-mac": { "web:ok": [userOf("好", 1)] } });
  });

  it("降级 no-op（jsdom 默认后端）：全部 API 静默空操作不抛错", async () => {
    setHistoryBackend(null); // jsdom 无 indexedDB → no-op
    expect(await historyStore.loadAll()).toEqual({});
    await expect(
      historyStore.saveConv("home-mac", "web:default", [userOf("x", 1)]),
    ).resolves.toBeUndefined();
    await expect(historyStore.clear()).resolves.toBeUndefined();
  });
});

describe("restoreHistory（恢复预填映射）", () => {
  it("新 conv：entries/preview=末条/lastAt=末条 at/unread=0，convOrder 按 lastAt 降序插 default 之后", () => {
    const s = restoreHistory(initialState(), {
      "home-mac": {
        "web:s-b": [userOf("任务B首条", 2000), textOf("任务B回复", 2100)],
        "web:s-a": [userOf("任务A首条", 3000)],
        "web:manual": [textOf("手动会话的恢复消息", 1000)],
      },
    });
    const sp = spaceOf(s, "home-mac");
    expect(sp.convOrder).toEqual([DEFAULT_CONV, "web:s-a", "web:s-b", "web:manual"]);
    expect(sp.convs["web:s-a"]).toMatchObject({
      lastPreview: "任务A首条",
      lastAt: 3000,
      unread: 0,
      title: "任务A首条",
    });
    // 末条 preview：user+text 两条取 text
    expect(sp.convs["web:s-b"]?.lastPreview).toBe("任务B回复");
    expect(sp.convs["web:s-b"]?.lastAt).toBe(2100);
    // entries 全量保留 + key 加恢复前缀（与新到条目键空间隔离）
    expect(sp.convs["web:s-b"]?.entries.map((e) => e.key)).toEqual(["h:user:2000", "h:text:2100"]);
  });

  it("default 会话（本地空壳）：整体填入并按首条派生 title；convOrder 位置不动", () => {
    const s = restoreHistory(initialState(), {
      [DEFAULT_MACHINE]: { [DEFAULT_CONV]: [userOf("默认会话首条消息", 900), cardOf("卡片正文")] },
    });
    const sp = spaceOf(s, DEFAULT_MACHINE);
    expect(sp.convOrder).toEqual([DEFAULT_CONV]);
    const conv = sp.convs[DEFAULT_CONV];
    expect(conv?.entries).toHaveLength(2);
    expect(conv?.title).toBe("默认会话首条消息");
    expect(conv?.lastPreview).toBe("卡片正文"); // 末条是 card：取卡片正文
    expect(conv?.lastAt).toBe(900); // card 无 at，取末条带 at 的（首条 user）
  });

  it("帧先到（本地已有条目）：恢复条目垫前、概要/未读以本地为准、lastAt 取较新侧、title 补派生", () => {
    let s = initialState();
    // 模拟恢复完成前 WS 已进一帧（无 user/text → title 未定）
    s = {
      ...s,
      spaces: {
        ...s.spaces,
        "home-mac": {
          convs: {
            [DEFAULT_CONV]: {
              entries: [],
              pending: [],
              lastPreview: "",
              lastAt: 0,
              unread: 0,
            },
            "web:s-a": {
              entries: [cardOf("新到的卡片")],
              pending: [],
              lastPreview: "新到的卡片",
              lastAt: 5000,
              unread: 2,
            },
          },
          convOrder: [DEFAULT_CONV, "web:s-a"],
        },
      },
    };
    const out = restoreHistory(s, {
      "home-mac": { "web:s-a": [userOf("更早的首条消息", 1000)] },
    });
    const conv = spaceOf(out, "home-mac").convs["web:s-a"];
    // 恢复（旧）在前、本地（新）在后，两端都保留
    expect(conv?.entries.map((e) => e.kind)).toEqual(["user", "card"]);
    expect(conv?.title).toBe("更早的首条消息"); // 本地未定 → 由恢复首条补派生
    expect(conv?.lastPreview).toBe("新到的卡片"); // 本地概要（更新）优先
    expect(conv?.unread).toBe(2); // 未读不动
    expect(conv?.lastAt).toBe(5000); // 较新侧
    expect(spaceOf(out, "home-mac").convOrder).toEqual([DEFAULT_CONV, "web:s-a"]); // 位置不动
  });

  it("本地已有 title / 空恢复 / 空记录：不改动（title 本地优先、无变化原引用返回）", () => {
    // 空恢复 → 原引用
    const base = initialState();
    expect(restoreHistory(base, {})).toBe(base);
    // 空记录跳过（不清空、不建壳）
    expect(restoreHistory(base, { "home-mac": { "web:x": [] } }).spaces).toBe(base.spaces);
    // 本地已定 title 不被恢复 title 覆盖（首条定调不可变）
    let s = base;
    s = {
      ...s,
      spaces: {
        ...s.spaces,
        "home-mac": {
          convs: {
            [DEFAULT_CONV]: { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 },
            "web:s-a": {
              entries: [cardOf("卡片")],
              pending: [],
              lastPreview: "",
              lastAt: 0,
              unread: 0,
              title: "本地已定",
            },
          },
          convOrder: [DEFAULT_CONV, "web:s-a"],
        },
      },
    };
    const out = restoreHistory(s, { "home-mac": { "web:s-a": [userOf("恢复首条", 1)] } });
    expect(spaceOf(out, "home-mac").convs["web:s-a"]?.title).toBe("本地已定");
  });

  it("未知机器：先建默认空间（预置 default conv）再合并", () => {
    const s = restoreHistory(initialState(), {
      "work-mac": { "web:s-x": [userOf("未知机器会话", 42)] },
    });
    const sp = spaceOf(s, "work-mac");
    expect(sp.convOrder).toEqual([DEFAULT_CONV, "web:s-x"]);
    expect(sp.convs[DEFAULT_CONV]).toBeTruthy(); // 新空间预置 default
    expect(sp.convs["web:s-x"]?.lastAt).toBe(42);
  });
});
