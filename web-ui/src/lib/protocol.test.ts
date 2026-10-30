import { describe, expect, it } from "vitest";
import {
  CONV_TITLE_MAX_CHARS,
  DEFAULT_CONV,
  DEFAULT_MACHINE,
  convTitle,
  deviceSummary,
  encodeFrame,
  initialState,
  isAutoConv,
  machineNameValid,
  machinePubkeyValid,
  newAutoConvName,
  normalizeExpiryMs,
  parseFrame,
  reducer,
  spaceOf,
  workspaceNameValid,
  type Action,
  type AppState,
  type InboundFrame,
  type WorkspacesFrame,
} from "./protocol";

function run(state: AppState, ...actions: Action[]): AppState {
  return actions.reduce(reducer, state);
}

function frame(raw: string): InboundFrame {
  const f = parseFrame(raw);
  if (!f) {
    throw new Error(`测试夹具帧解析失败：${raw}`);
  }
  return f;
}

/** 单机/默认机器空间访问器（v1.1 起 convs 收进 spaces[machine]）。 */
const convsOf = (s: AppState, machine = DEFAULT_MACHINE) => spaceOf(s, machine).convs;
const orderOf = (s: AppState, machine = DEFAULT_MACHINE) => spaceOf(s, machine).convOrder;

const TEXT_FRAME = (text: string, convId = DEFAULT_CONV) =>
  `{"v":1,"type":"text","convId":"${convId}","text":"${text}"}`;

const CARD_FRAME = (opts: {
  type?: "card" | "card_patch";
  messageId?: string;
  text: string;
  terminal?: string;
  convId?: string;
}) => {
  const { type = "card", messageId = "m1", text, terminal = "running", convId = DEFAULT_CONV } = opts;
  return `{"v":1,"type":"${type}","convId":"${convId}","messageId":"${messageId}","state":"${
    terminal === "running" ? "streaming" : "done"
  }","card":{"text":"${text}","tool_calls":[],"thoughts":[],"todos":[],"phase":"outputting","run_secs":5,"terminal":"${terminal}"}}`;
};

describe("parseFrame", () => {
  it("合法帧各类型解析", () => {
    expect(parseFrame('{"v":1,"type":"hello_ack","role":"browser"}')?.type).toBe("hello_ack");
    expect(parseFrame('{"v":1,"type":"machine","online":true}')?.type).toBe("machine");
    expect(parseFrame('{"v":1,"type":"error","code":"gateway_offline"}')?.type).toBe("error");
    expect(parseFrame('{"v":1,"type":"text","convId":"web:default","text":"hi"}')?.type).toBe("text");
    expect(parseFrame(CARD_FRAME({ text: "c" }))?.type).toBe("card");
    expect(
      parseFrame(
        '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls"}',
      )?.type,
    ).toBe("approval_request");
    expect(
      parseFrame('{"v":1,"type":"approval_resolved","convId":"web:default","requestId":"r1","decision":"cancelled"}')
        ?.type,
    ).toBe("approval_resolved");
    expect(parseFrame('{"v":1,"type":"pong"}')?.type).toBe("pong");
  });

  it("缺 v / 版本不为 1 / 缺 type → null", () => {
    expect(parseFrame('{"type":"text","convId":"web:default","text":"x"}')).toBeNull();
    expect(parseFrame('{"v":2,"type":"text","convId":"web:default","text":"x"}')).toBeNull();
    expect(parseFrame('{"v":"1","type":"pong"}')).toBeNull();
    expect(parseFrame('{"v":1}')).toBeNull();
  });

  it("未知 type → null（不抛）", () => {
    expect(parseFrame('{"v":1,"type":"future-thing","data":1}')).toBeNull();
  });

  it("畸形 JSON / 非对象 → null 不抛", () => {
    expect(parseFrame("not json")).toBeNull();
    expect(parseFrame('"string"')).toBeNull();
    expect(parseFrame("42")).toBeNull();
    expect(parseFrame("null")).toBeNull();
  });

  it("关键字段缺失/类型错误 → null", () => {
    // text 缺 convId
    expect(parseFrame('{"v":1,"type":"text","text":"x"}')).toBeNull();
    // machine.online 非布尔
    expect(parseFrame('{"v":1,"type":"machine","online":"yes"}')).toBeNull();
    // card 缺 card 载荷
    expect(parseFrame('{"v":1,"type":"card","convId":"web:default","messageId":"m"}')).toBeNull();
    // card.text 非字符串
    expect(
      parseFrame('{"v":1,"type":"card","convId":"web:default","messageId":"m","card":{"text":1}}'),
    ).toBeNull();
    // approval_resolved decision 非法
    expect(
      parseFrame('{"v":1,"type":"approval_resolved","convId":"c","requestId":"r","decision":"maybe"}'),
    ).toBeNull();
  });

  it("v1.1 machines 帧解析：字符串数组合法，空数组亦合法", () => {
    expect(
      parseFrame('{"v":1,"type":"machines","online":["home-mac","work-mac"]}'),
    ).toMatchObject({ type: "machines", online: ["home-mac", "work-mac"] });
    expect(parseFrame('{"v":1,"type":"machines","online":[]}')).toMatchObject({
      type: "machines",
      online: [],
    });
  });

  it("v1.1 网关帧可选 machine 字段：缺席合法、携带字符串合法、非字符串 → null", () => {
    // 携带 machine：解析后原样保留
    expect(
      parseFrame('{"v":1,"type":"text","convId":"web:default","text":"hi","machine":"home-mac"}'),
    ).toMatchObject({ type: "text", machine: "home-mac" });
    expect(
      parseFrame(
        '{"v":1,"type":"card","convId":"web:default","messageId":"m","card":{"text":"t"},"machine":"home-mac"}',
      ),
    ).toMatchObject({ type: "card", machine: "home-mac" });
    expect(
      parseFrame(
        '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r","tool":"Bash","inputSummary":"ls","machine":"work-mac"}',
      ),
    ).toMatchObject({ type: "approval_request", machine: "work-mac" });
    // machine 类型错误
    expect(parseFrame('{"v":1,"type":"text","convId":"web:default","text":"hi","machine":7}')).toBeNull();
    expect(
      parseFrame(
        '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r","tool":"Bash","inputSummary":"x","machine":[]}',
      ),
    ).toBeNull();
    // machines.online 非字符串数组
    expect(parseFrame('{"v":1,"type":"machines","online":"home-mac"}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"machines","online":[1]}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"machines"}')).toBeNull();
  });

  it("v1.2 hello_ack 可选 session：缺席合法（旧 relay）、形状齐备合法、畸形 → null", () => {
    // 缺席（旧 relay）：仍合法
    expect(parseFrame('{"v":1,"type":"hello_ack","role":"browser"}')).toMatchObject({
      type: "hello_ack",
      role: "browser",
    });
    // 携带合法 session：原样保留
    expect(
      parseFrame(
        '{"v":1,"type":"hello_ack","role":"browser","session":{"id":"sid-1","token":"st-1","expiresAt":1900000000000}}',
      ),
    ).toMatchObject({
      type: "hello_ack",
      session: { id: "sid-1", token: "st-1", expiresAt: 1_900_000_000_000 },
    });
    // session 形状错误
    expect(parseFrame('{"v":1,"type":"hello_ack","role":"browser","session":"st-1"}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"hello_ack","role":"browser","session":{"id":"sid-1"}}')).toBeNull();
    expect(
      parseFrame(
        '{"v":1,"type":"hello_ack","role":"browser","session":{"id":"sid-1","token":"st-1","expiresAt":"tomorrow"}}',
      ),
    ).toBeNull();
  });

  it("v1.2 devices 帧解析：元素形状校验、lastSeen/created 可选、非法 → null", () => {
    const ok =
      '{"v":1,"type":"devices","devices":[{"id":"d1","name":"Mac/Chrome","connected":true,"lastSeen":1700000000000,"created":1699000000000},{"id":"d2","name":"iPhone/Safari","connected":false}]}';
    const f = parseFrame(ok);
    expect(f).toMatchObject({ type: "devices" });
    expect((f as { devices: unknown[] }).devices).toHaveLength(2);
    // 元素非法：缺 id / connected 非布尔 / lastSeen 非数字
    expect(parseFrame('{"v":1,"type":"devices","devices":[{"name":"x","connected":true}]}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"devices","devices":[{"id":"d","name":"x","connected":"yes"}]}')).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"devices","devices":[{"id":"d","name":"x","connected":true,"lastSeen":"y"}]}'),
    ).toBeNull();
    // devices 非数组
    expect(parseFrame('{"v":1,"type":"devices","devices":"all"}')).toBeNull();
  });
});

describe("v1.2 会话层纯函数", () => {
  it("deviceSummary：常见 UA → OS/浏览器 短名", () => {
    expect(
      deviceSummary(
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
      ),
    ).toBe("iPhone/Safari");
    expect(
      deviceSummary(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
      ),
    ).toBe("Mac/Chrome");
    // Edge/Opera UA 同时含 Chrome/ 与 Safari/：必须判衍生浏览器
    expect(
      deviceSummary(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0",
      ),
    ).toBe("Windows/Edge");
    expect(
      deviceSummary(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_4) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 OPR/112.0.0.0",
      ),
    ).toBe("Mac/Opera");
    expect(
      deviceSummary(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.4; rv:127.0) Gecko/20100101 Firefox/127.0",
      ),
    ).toBe("Mac/Firefox");
    expect(
      deviceSummary(
        "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
      ),
    ).toBe("Android/Chrome");
    expect(
      deviceSummary(
        "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1",
      ),
    ).toBe("iPad/Safari");
  });

  it("deviceSummary：单边命中与全未命中兜底", () => {
    expect(deviceSummary("Mozilla/5.0 (Macintosh) Firefox/127.0 and-nothing-else")).toBe("Mac/Firefox");
    // 只有 OS 信号
    expect(deviceSummary("some-app Windows x64 runtime")).toBe("Windows");
    // 只有浏览器信号
    expect(deviceSummary("curl-like agent Chrome/126")).toBe("Chrome");
    // 全未命中
    expect(deviceSummary("unknown-client/1.0")).toBe("未知设备");
    expect(deviceSummary("")).toBe("未知设备");
  });

  it("normalizeExpiryMs：毫秒 epoch 原样、秒级自动放大、边界不误伤", () => {
    expect(normalizeExpiryMs(1_900_000_000_000)).toBe(1_900_000_000_000); // 毫秒（2020+）
    expect(normalizeExpiryMs(1_900_000_000)).toBe(1_900_000_000_000); // 秒级 → ×1000
    expect(normalizeExpiryMs(0)).toBe(0);
  });
});

describe("reducer", () => {
  it("初始状态含默认 conv，无消息", () => {
    const s = initialState();
    expect(orderOf(s)).toEqual([DEFAULT_CONV]);
    expect(convsOf(s)[DEFAULT_CONV]).toEqual({
      entries: [],
      pending: [],
      lastPreview: "",
      lastAt: 0,
      unread: 0,
    });
    expect(s.machineOnline).toBe(false);
    expect(s.currentConv).toBe(DEFAULT_CONV);
  });

  it("text 帧追加一条助手消息", () => {
    const s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("你好")) });
    const entries = convsOf(s)[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "text", text: "你好" });
  });

  it("card 帧新建卡片条目", () => {
    const s = run(initialState(), {
      type: "frame",
      frame: frame(CARD_FRAME({ messageId: "m1", text: "流式中" })),
    });
    const entries = convsOf(s)[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({
      kind: "card",
      messageId: "m1",
      state: "streaming",
    });
    expect((entries[0] as { card: { text: string } }).card.text).toBe("流式中");
  });

  it("card_patch 同 messageId 整卡替换且保留原位", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("前言")) },
      { type: "frame", frame: frame(CARD_FRAME({ messageId: "m1", text: "第一版" })) },
      { type: "frame", frame: frame(TEXT_FRAME("插入")) },
      { type: "frame", frame: frame(CARD_FRAME({ type: "card_patch", messageId: "m1", text: "第二版", terminal: "done" })) },
    );
    const entries = convsOf(s)[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(3);
    expect(entries[1]).toMatchObject({ kind: "card", messageId: "m1", state: "done" });
    expect((entries[1] as { card: { text: string } }).card.text).toBe("第二版");
    // 卡片仍在原位（第 2 项），插入的 text 未被顶掉
    expect(entries[0]).toMatchObject({ kind: "text", text: "前言" });
    expect(entries[2]).toMatchObject({ kind: "text", text: "插入" });
  });

  it("sync 重发同 messageId card 幂等（同 messageId 覆盖不新增）", () => {
    const base = run(
      initialState(),
      { type: "frame", frame: frame(CARD_FRAME({ messageId: "m1", text: "流式", terminal: "running" })) },
    );
    // 断线重连 → sync → 网关以 card 帧重发同 messageId 终态
    const resynced = run(base, {
      type: "frame",
      frame: frame(CARD_FRAME({ messageId: "m1", text: "终稿", terminal: "done" })),
    });
    const entries = convsOf(resynced)[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "card", messageId: "m1", state: "done" });
    expect((entries[0] as { card: { text: string } }).card.text).toBe("终稿");
  });

  it("approval_request 入 pending；同 requestId 幂等", () => {
    const req = '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"rm -rf /tmp/x"}';
    let s = run(initialState(), { type: "frame", frame: frame(req) });
    expect(convsOf(s)[DEFAULT_CONV].pending).toHaveLength(1);
    expect(convsOf(s)[DEFAULT_CONV].pending[0]).toMatchObject({ requestId: "r1", tool: "Bash" });
    s = run(s, { type: "frame", frame: frame(req) });
    expect(convsOf(s)[DEFAULT_CONV].pending).toHaveLength(1);
  });

  it("approval_resolved 移出 pending 并留终态记录", () => {
    let s = run(
      initialState(),
      {
        type: "frame",
        frame: frame('{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls"}'),
      },
    );
    s = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"approval_resolved","convId":"web:default","requestId":"r1","decision":"cancelled","message":"/stop 顶替"}'),
    });
    expect(convsOf(s)[DEFAULT_CONV].pending).toHaveLength(0);
    const entries = convsOf(s)[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "approval_note", text: "已取消", message: "/stop 顶替" });
    // 无对应 pending 的 resolved 被忽略（防噪声）
    const again = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"approval_resolved","convId":"web:default","requestId":"rX","decision":"allow"}'),
    });
    expect(convsOf(again)[DEFAULT_CONV].entries).toHaveLength(1);
  });

  it("approval_sent 本地乐观移除", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame('{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls"}'),
    });
    s = run(s, { type: "approval_sent", convId: DEFAULT_CONV, requestId: "r1" });
    expect(convsOf(s)[DEFAULT_CONV].pending).toHaveLength(0);
  });

  it("machine 帧更新在线态", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame('{"v":1,"type":"machine","online":true}') },
    );
    expect(s.machineOnline).toBe(true);
    const off = run(s, { type: "frame", frame: frame('{"v":1,"type":"machine","online":false}') });
    expect(off.machineOnline).toBe(false);
  });

  it("error 帧记入 lastError；clear_error 清除", () => {
    const s = run(initialState(), {
      type: "frame",
      frame: frame('{"v":1,"type":"error","code":"gateway_offline"}'),
    });
    expect(s.lastError?.code).toBe("gateway_offline");
    expect(run(s, { type: "clear_error" }).lastError).toBeNull();
  });

  it("hello_ack / pong 不改变消息流状态", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame('{"v":1,"type":"hello_ack","role":"browser"}') },
      { type: "frame", frame: frame('{"v":1,"type":"pong"}') },
    );
    expect(convsOf(s)[DEFAULT_CONV].entries).toHaveLength(0);
  });

  it("conv 集合按帧收集，web:default 恒在首位", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("a", "web:work")) },
      { type: "frame", frame: frame(TEXT_FRAME("b", "web:proj-x")) },
      { type: "frame", frame: frame(TEXT_FRAME("c")) },
    );
    expect(orderOf(s)).toEqual(["web:default", "web:work", "web:proj-x"]);
    expect(convsOf(s)["web:work"].entries).toHaveLength(1);
    expect(convsOf(s)["web:proj-x"].entries).toHaveLength(1);
    expect(convsOf(s)[DEFAULT_CONV].entries).toHaveLength(1);
  });

  // MultiPlatform 镜像路由把网关侧 local 终端会话（conv 形态 local:<名>）的
  // 出站帧同步给 web 平台——SPA 收到 local:xxx 帧即按同机制收集为侧栏会话；
  // 用户点选后的干预回执（send_echo 落同 conv，出站 message 帧的 convId 由
  // ws 层原样携带，见 ws.test.ts）不被改写，端到端回到 local 会话。
  it("local:xxx conv（镜像的本地终端会话）按帧收集，干预回执落同 conv", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("终端输出", "local:work")) },
    );
    expect(orderOf(s)).toEqual([DEFAULT_CONV, "local:work"]);
    expect(convsOf(s)["local:work"].entries).toHaveLength(1);
    expect(convsOf(s)["local:work"].entries[0]).toMatchObject({
      kind: "text",
      text: "终端输出",
    });
    // 点选后干预：用户回显追加到同一 local conv（convId 不被加 web: 前缀）。
    const after = run(s, { type: "send_echo", convId: "local:work", text: "继续" });
    const entries = convsOf(after)["local:work"].entries;
    expect(entries).toHaveLength(2);
    expect(entries[1]).toMatchObject({ kind: "user", text: "继续" });
  });

  it("send_echo 追加用户回显到指定 conv", () => {
    const s = run(initialState(), { type: "send_echo", convId: "web:work", text: "列出目录" });
    expect(orderOf(s)).toContain("web:work");
    expect(convsOf(s)["web:work"].entries).toHaveLength(1);
    expect(convsOf(s)["web:work"].entries[0]).toMatchObject({ kind: "user", text: "列出目录" });
  });

  it("会话概要：lastPreview/lastAt 随最后一条消息更新（user/text/card 各形态）", () => {
    let s = initialState();
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("");
    expect(convsOf(s)[DEFAULT_CONV].lastAt).toBe(0);
    // text：取首行（夹具为 JSON 原文，换行需转义）
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("你好\\n第二行")) });
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("你好");
    expect(convsOf(s)[DEFAULT_CONV].lastAt).toBeGreaterThan(0);
    // user 回显覆盖概要
    s = run(s, { type: "send_echo", convId: DEFAULT_CONV, text: "用户提问" });
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("用户提问");
    // card：正文首行（夹具为 JSON 原文，换行需转义）
    s = run(s, {
      type: "frame",
      frame: frame(CARD_FRAME({ messageId: "m1", text: "卡片正文首行\\n次行" })),
    });
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("卡片正文首行");
    // 空正文卡片回退 task_digest
    s = run(s, {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"card","convId":"web:default","messageId":"m2","state":"streaming","card":{"text":"","tool_calls":[],"thoughts":[],"todos":[],"phase":"thinking","run_secs":1,"task_digest":"部署中","terminal":"running"}}',
      ),
    });
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("部署中");
  });

  it("会话概要：card_patch 补丁旧卡片（非最新条目）不覆盖概要", () => {
    let s = run(
      initialState(),
      { type: "frame", frame: frame(CARD_FRAME({ messageId: "m1", text: "旧版" })) },
      { type: "frame", frame: frame(TEXT_FRAME("后到的文本")) },
    );
    s = run(s, {
      type: "frame",
      frame: frame(CARD_FRAME({ type: "card_patch", messageId: "m1", text: "新正文" })),
    });
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("后到的文本");
  });

  it("会话概要：各 conv 独立维护", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("默认会话消息")) },
      { type: "frame", frame: frame(TEXT_FRAME("工作会话消息", "web:work")) },
    );
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("默认会话消息");
    expect(convsOf(s)["web:work"].lastPreview).toBe("工作会话消息");
  });

  it("内存护栏：超 200 条目丢弃最早的用户回显，服务端条目保留", () => {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("服务端消息")) });
    const echoes: Action[] = Array.from({ length: 205 }, (_, i) => ({
      type: "send_echo",
      convId: DEFAULT_CONV,
      text: `msg${i}`,
    }));
    s = run(s, ...echoes);
    const entries = convsOf(s)[DEFAULT_CONV].entries;
    // 206 条 → 丢 6 条最早 user 回显 → 恰好 200
    expect(entries).toHaveLength(200);
    // 服务端 text 条目不丢（卡片由 sync 兜底，text/user 回显属 ephemeral）
    expect(entries[0]).toMatchObject({ kind: "text", text: "服务端消息" });
    expect(entries.some((e) => e.kind === "user" && e.text === "msg0")).toBe(false);
    expect(entries.some((e) => e.kind === "user" && e.text === "msg5")).toBe(false);
    expect(entries.some((e) => e.kind === "user" && e.text === "msg6")).toBe(true);
    expect(entries.some((e) => e.kind === "user" && e.text === "msg204")).toBe(true);
    // 概要仍是最后一条
    expect(convsOf(s)[DEFAULT_CONV].lastPreview).toBe("msg204");
  });

  it("内存护栏：无本地回显可丢时（纯服务端条目）不强截断", () => {
    const actions: Action[] = Array.from({ length: 205 }, (_, i) => ({
      type: "frame",
      frame: frame(TEXT_FRAME(`t${i}`)),
    }));
    const s = run(initialState(), ...actions);
    expect(convsOf(s)[DEFAULT_CONV].entries).toHaveLength(205);
  });
});

describe("reducer 未读计数（侧栏徽标数据源）", () => {
  it("非当前 conv 收到 text → unread 累加；当前 conv 与用户回显不计", () => {
    let s = initialState();
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("当前会话消息")) });
    expect(convsOf(s)[DEFAULT_CONV].unread).toBe(0);
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("后台消息", "web:side")) });
    expect(convsOf(s)["web:side"].unread).toBe(1);
    // 用户回显（即使落非当前 conv——现实中不会发生）也不计
    s = run(s, { type: "send_echo", convId: "web:side", text: "用户回显不计数" });
    expect(convsOf(s)["web:side"].unread).toBe(1);
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("又一条", "web:side")) });
    expect(convsOf(s)["web:side"].unread).toBe(2);
  });

  it("select_conv 切当前会话并清零未读（同 conv 重复点选幂等）", () => {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("后台消息", "web:side")) });
    s = run(s, { type: "select_conv", convId: "web:side" });
    expect(s.currentConv).toBe("web:side");
    expect(convsOf(s)["web:side"].unread).toBe(0);
    // 切走后新消息重新计未读
    s = run(s, { type: "select_conv", convId: DEFAULT_CONV });
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("再来", "web:side")) });
    expect(convsOf(s)["web:side"].unread).toBe(1);
    s = run(s, { type: "select_conv", convId: "web:side" });
    expect(convsOf(s)["web:side"].unread).toBe(0);
  });

  it("card：已存在 conv 的新卡片插入（非当前）计未读；card_patch / sync 重放替换不计", () => {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("开场", "web:side")) });
    s = run(
      s,
      { type: "frame", frame: frame(CARD_FRAME({ convId: "web:side", messageId: "m1", text: "任务卡" })) },
    );
    expect(convsOf(s)["web:side"].unread).toBe(2); // text + 新卡片插入
    // card_patch 同 messageId 替换：不追加
    s = run(s, {
      type: "frame",
      frame: frame(
        CARD_FRAME({ type: "card_patch", convId: "web:side", messageId: "m1", text: "完成", terminal: "done" }),
      ),
    });
    expect(convsOf(s)["web:side"].unread).toBe(2);
    // 重连 sync 重放（card 类型、同 messageId）→ 覆盖语义：不追加
    s = run(s, {
      type: "frame",
      frame: frame(
        CARD_FRAME({ type: "card", convId: "web:side", messageId: "m1", text: "完成", terminal: "done" }),
      ),
    });
    expect(convsOf(s)["web:side"].unread).toBe(2);
  });

  it("card 首次物化 conv（刷新后 sync 恢复 / 新会话首卡）不计未读——刷新不点亮全部会话", () => {
    let s = initialState();
    s = run(
      s,
      { type: "frame", frame: frame(CARD_FRAME({ convId: "web:synced", messageId: "m9", text: "恢复的卡片", terminal: "done" })) },
    );
    expect(convsOf(s)["web:synced"].unread).toBe(0);
    // conv 已存在后的新卡片才计数
    s = run(
      s,
      { type: "frame", frame: frame(CARD_FRAME({ convId: "web:synced", messageId: "m10", text: "新任务", terminal: "done" })) },
    );
    expect(convsOf(s)["web:synced"].unread).toBe(1);
  });

  it("select_machine：currentConv 回该机器默认会话并清其未读", () => {
    let s = run(
      initialState(),
      { type: "frame", frame: frame(`{"v":1,"type":"machines","online":["home-mac","work-mac"]}`) },
    );
    // 首帧 machines 自动选中 home-mac；给 work-mac 的默认会话攒一条未读
    s = run(s, {
      type: "frame",
      frame: frame(`{"v":1,"type":"text","convId":"web:default","text":"hi","machine":"work-mac"}`),
    });
    expect(convsOf(s, "work-mac")[DEFAULT_CONV].unread).toBe(1);
    s = run(s, { type: "select_machine", machine: "work-mac" });
    expect(s.currentMachine).toBe("work-mac");
    expect(s.currentConv).toBe(DEFAULT_CONV);
    expect(convsOf(s, "work-mac")[DEFAULT_CONV].unread).toBe(0);
  });
});

describe("reducer 多机（v1.1）", () => {
  const MACHINES_FRAME = (online: string[]) =>
    `{"v":1,"type":"machines","online":${JSON.stringify(online)}}`;
  const TEXT_ON = (machine: string, text: string, convId = DEFAULT_CONV) =>
    `{"v":1,"type":"text","convId":"${convId}","text":"${text}","machine":"${machine}"}`;

  it("初始状态：单机模式（无 machines 帧、currentMachine=default）", () => {
    const s = initialState();
    expect(s.machines).toEqual([]);
    expect(s.machinesKnown).toBe(false);
    expect(s.currentMachine).toBe(DEFAULT_MACHINE);
    expect(spaceOf(s, DEFAULT_MACHINE).convOrder).toEqual([DEFAULT_CONV]);
  });

  it("machines 帧全量覆盖在线列表并置 machinesKnown；上下线增删即时反映", () => {
    let s = run(initialState(), { type: "frame", frame: frame(MACHINES_FRAME(["home-mac"])) });
    expect(s.machinesKnown).toBe(true);
    expect(s.machines).toEqual(["home-mac"]);
    // 引导切换（default 不在列表）→ currentMachine 变为唯一在线机器
    expect(s.currentMachine).toBe("home-mac");
    // 全量覆盖：新列表不含旧机器
    s = run(s, { type: "frame", frame: frame(MACHINES_FRAME(["home-mac", "work-mac"])) });
    expect(s.machines).toEqual(["home-mac", "work-mac"]);
    expect(s.currentMachine).toBe("home-mac"); // 仍在线不切
    s = run(s, { type: "frame", frame: frame(MACHINES_FRAME(["work-mac"])) });
    expect(s.machines).toEqual(["work-mac"]);
  });

  it("首帧 machines 引导切换：default 不在列表 → 切第一台在线并出 notice", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(MACHINES_FRAME(["work-mac", "home-mac"])) },
    );
    expect(s.currentMachine).toBe("work-mac"); // 列表第一台
    expect(s.lastNotice).toMatchObject({ kind: "switched", from: DEFAULT_MACHINE, to: "work-mac" });
    // clear_notice 清除
    expect(run(s, { type: "clear_notice" }).lastNotice).toBeNull();
  });

  it("当前机器掉线：自动切第一台在线并出 switched notice", () => {
    let s = run(
      initialState(),
      { type: "frame", frame: frame(MACHINES_FRAME(["home-mac", "work-mac"])) },
    );
    expect(s.currentMachine).toBe("home-mac");
    s = run(s, { type: "frame", frame: frame(MACHINES_FRAME(["work-mac"])) });
    expect(s.currentMachine).toBe("work-mac");
    expect(s.lastNotice).toMatchObject({ kind: "switched", from: "home-mac", to: "work-mac" });
  });

  it("全部离线：无可切机器，保持 currentMachine 并出 offline notice", () => {
    let s = run(
      initialState(),
      { type: "frame", frame: frame(MACHINES_FRAME(["home-mac"])) },
    );
    s = run(s, { type: "frame", frame: frame(MACHINES_FRAME([])) });
    expect(s.machines).toEqual([]);
    expect(s.currentMachine).toBe("home-mac");
    expect(s.lastNotice).toMatchObject({ kind: "offline", machine: "home-mac" });
  });

  it("当前机器仍在线：machines 帧不切换、不出 notice", () => {
    // 先引导到已知状态并清掉引导 notice，再验证「仍在线」的后续帧是静默的
    let s = run(
      initialState(),
      { type: "frame", frame: frame(MACHINES_FRAME(["home-mac", "work-mac"])) },
      { type: "clear_notice" },
    );
    expect(s.currentMachine).toBe("home-mac");
    expect(s.lastNotice).toBeNull();
    s = run(s, { type: "frame", frame: frame(MACHINES_FRAME(["home-mac", "work-mac", "x-mac"])) });
    expect(s.currentMachine).toBe("home-mac");
    expect(s.lastNotice).toBeNull();
  });

  it("入站帧带 machine 归对应机器空间；不带归 default（旧网关兼容）", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_ON("home-mac", "家里")) },
      { type: "frame", frame: frame(TEXT_ON("work-mac", "公司")) },
      { type: "frame", frame: frame(TEXT_FRAME("无机器字段")) },
    );
    expect(spaceOf(s, "home-mac").convs[DEFAULT_CONV].entries).toHaveLength(1);
    expect(spaceOf(s, "work-mac").convs[DEFAULT_CONV].entries).toHaveLength(1);
    expect(spaceOf(s, DEFAULT_MACHINE).convs[DEFAULT_CONV].entries).toHaveLength(1);
    expect(
      (spaceOf(s, "home-mac").convs[DEFAULT_CONV].entries[0] as { text: string }).text,
    ).toBe("家里");
  });

  it("会话空间 per-machine 隔离：同 convId 两台机器互不干扰", () => {
    let s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_ON("home-mac", "home 消息", "web:work")) },
      { type: "frame", frame: frame(TEXT_ON("work-mac", "work 消息", "web:work")) },
      { type: "send_echo", convId: "web:work", text: "home 回显", machine: "home-mac" },
    );
    const home = spaceOf(s, "home-mac").convs["web:work"];
    const work = spaceOf(s, "work-mac").convs["web:work"];
    expect(home.entries).toHaveLength(2); // text + user 回显
    expect(work.entries).toHaveLength(1);
    expect(home.lastPreview).toBe("home 回显");
    expect(work.lastPreview).toBe("work 消息");
    // 卡片按 machine+convId+messageId 幂等：两台机器同 messageId 不串卡
    s = run(
      s,
      {
        type: "frame",
        frame: frame(
          '{"v":1,"type":"card","convId":"web:work","messageId":"m1","state":"streaming","machine":"home-mac","card":{"text":"home 卡","tool_calls":[],"thoughts":[],"todos":[],"phase":"outputting","run_secs":1,"terminal":"running"}}',
        ),
      },
      {
        type: "frame",
        frame: frame(
          '{"v":1,"type":"card_patch","convId":"web:work","messageId":"m1","state":"done","machine":"work-mac","card":{"text":"work 卡","tool_calls":[],"thoughts":[],"todos":[],"phase":"outputting","run_secs":2,"terminal":"done"}}',
        ),
      },
    );
    const homeCards = spaceOf(s, "home-mac").convs["web:work"].entries.filter((e) => e.kind === "card");
    const workCards = spaceOf(s, "work-mac").convs["web:work"].entries.filter((e) => e.kind === "card");
    expect(homeCards).toHaveLength(1);
    expect(workCards).toHaveLength(1);
    expect((homeCards[0] as { card: { text: string } }).card.text).toBe("home 卡");
    expect((workCards[0] as { card: { text: string } }).card.text).toBe("work 卡");
  });

  it("审批 per-machine：pending 携带 machine，approval_sent/approval_resolved 按机器命中", () => {
    let s = run(
      initialState(),
      {
        type: "frame",
        frame: frame(
          '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls","machine":"home-mac"}',
        ),
      },
      {
        type: "frame",
        frame: frame(
          '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls","machine":"work-mac"}',
        ),
      },
    );
    // 同 requestId 不同机器：各自空间的 pending 独立入列
    expect(spaceOf(s, "home-mac").convs[DEFAULT_CONV].pending[0]).toMatchObject({
      machine: "home-mac",
      requestId: "r1",
    });
    expect(spaceOf(s, "work-mac").convs[DEFAULT_CONV].pending[0]).toMatchObject({
      machine: "work-mac",
      requestId: "r1",
    });
    // 乐观移除只命中指定机器
    s = run(s, { type: "approval_sent", convId: DEFAULT_CONV, requestId: "r1", machine: "home-mac" });
    expect(spaceOf(s, "home-mac").convs[DEFAULT_CONV].pending).toHaveLength(0);
    expect(spaceOf(s, "work-mac").convs[DEFAULT_CONV].pending).toHaveLength(1);
    // resolved 只命中带 machine 的机器空间；其它机器同 requestId 不受影响
    s = run(s, {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"approval_resolved","convId":"web:default","requestId":"r1","decision":"allow","machine":"work-mac"}',
      ),
    });
    expect(spaceOf(s, "work-mac").convs[DEFAULT_CONV].pending).toHaveLength(0);
    expect(spaceOf(s, "work-mac").convs[DEFAULT_CONV].entries).toHaveLength(1); // 终态记录
    expect(spaceOf(s, "home-mac").convs[DEFAULT_CONV].entries).toHaveLength(0);
  });

  it("select_machine 切换当前机器；同机器幂等", () => {
    let s = run(initialState(), { type: "select_machine", machine: "home-mac" });
    expect(s.currentMachine).toBe("home-mac");
    const again = run(s, { type: "select_machine", machine: "home-mac" });
    expect(again).toBe(s); // 引用不变（无谓渲染防护）
  });

  it("单机兼容全链路：无 machines 帧 + 帧不带 machine —— 行为与既有单机路径一致", () => {
    // 与「行为与现在完全一致」约束对应的端到端兼容路径：文本 + 卡片 + 审批 +
    // 用户回显全部落 default 机器空间，概要/护栏语义不变
    let s = run(
      initialState(),
      { type: "frame", frame: frame('{"v":1,"type":"machine","online":true}') },
      { type: "frame", frame: frame(TEXT_FRAME("你好")) },
      { type: "frame", frame: frame(CARD_FRAME({ messageId: "m1", text: "卡片" })) },
      { type: "send_echo", convId: DEFAULT_CONV, text: "用户提问" },
      {
        type: "frame",
        frame: frame(
          '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls"}',
        ),
      },
    );
    expect(s.machinesKnown).toBe(false); // 旧 relay：从未收到 machines
    expect(s.currentMachine).toBe(DEFAULT_MACHINE);
    expect(s.machineOnline).toBe(true);
    const conv = convsOf(s)[DEFAULT_CONV];
    expect(conv.entries).toHaveLength(3); // text + card + user 回显（审批只进 pending 不进流）
    expect(conv.pending).toHaveLength(1);
    expect(conv.pending[0].machine).toBe(DEFAULT_MACHINE);
    expect(orderOf(s)).toEqual([DEFAULT_CONV]);
    // send_echo / approval_sent 缺 machine 字段落 default 空间（既有调用形态）
    s = run(s, { type: "approval_sent", convId: DEFAULT_CONV, requestId: "r1" });
    expect(convsOf(s)[DEFAULT_CONV].pending).toHaveLength(0);
  });

  it("v1.2 devices 帧：全量覆盖 devices，初始为 null", () => {
    expect(initialState().devices).toBeNull();
    const list1 = [
      { id: "d1", name: "Mac/Chrome", connected: true, lastSeen: 100, created: 50 },
      { id: "d2", name: "iPhone/Safari", connected: false, lastSeen: 90, created: 60 },
    ];
    let s = run(initialState(), {
      type: "frame",
      frame: frame('{"v":1,"type":"devices","devices":[{"id":"d1","name":"Mac/Chrome","connected":true,"lastSeen":100,"created":50},{"id":"d2","name":"iPhone/Safari","connected":false,"lastSeen":90,"created":60}]}'),
    });
    expect(s.devices).toEqual(list1);
    // device_revoke 后 relay 回更新列表：全量覆盖（d2 消失）
    s = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"devices","devices":[{"id":"d1","name":"Mac/Chrome","connected":true}]}'),
    });
    expect(s.devices).toEqual([{ id: "d1", name: "Mac/Chrome", connected: true }]);
  });
});

describe("encodeFrame", () => {
  it("出站帧序列化：message 带 convId、control 无 args 不序列化 args 键", () => {
    expect(encodeFrame({ v: 1, type: "message", convId: "web:default", text: "hi" })).toBe(
      '{"v":1,"type":"message","convId":"web:default","text":"hi"}',
    );
    expect(encodeFrame({ v: 1, type: "control", cmd: "stop" })).toBe('{"v":1,"type":"control","cmd":"stop"}');
    expect(
      encodeFrame({ v: 1, type: "approval_response", convId: "web:work", requestId: "r1", decision: "allow" }),
    ).toBe('{"v":1,"type":"approval_response","convId":"web:work","requestId":"r1","decision":"allow"}');
    expect(encodeFrame({ v: 1, type: "sync" })).toBe('{"v":1,"type":"sync"}');
    expect(encodeFrame({ v: 1, type: "ping" })).toBe('{"v":1,"type":"ping"}');
  });

  it("v1.1 出站帧带 machine：字段序列化在帧尾；不带时字节与 v1 一致", () => {
    expect(encodeFrame({ v: 1, type: "message", convId: "web:default", text: "hi", machine: "home-mac" })).toBe(
      '{"v":1,"type":"message","convId":"web:default","text":"hi","machine":"home-mac"}',
    );
    expect(
      encodeFrame({ v: 1, type: "approval_response", convId: "web:work", requestId: "r1", decision: "allow", machine: "work-mac" }),
    ).toBe(
      '{"v":1,"type":"approval_response","convId":"web:work","requestId":"r1","decision":"allow","machine":"work-mac"}',
    );
    expect(encodeFrame({ v: 1, type: "control", cmd: "stop", machine: "home-mac" })).toBe(
      '{"v":1,"type":"control","cmd":"stop","machine":"home-mac"}',
    );
    expect(encodeFrame({ v: 1, type: "sync", machine: "home-mac" })).toBe(
      '{"v":1,"type":"sync","machine":"home-mac"}',
    );
    expect(encodeFrame({ v: 1, type: "ping", machine: "home-mac" })).toBe(
      '{"v":1,"type":"ping","machine":"home-mac"}',
    );
    // machine: undefined → 键不出现（旧 relay 字节级兼容）
    expect(encodeFrame({ v: 1, type: "sync", machine: undefined })).toBe('{"v":1,"type":"sync"}');
    expect(encodeFrame({ v: 1, type: "control", cmd: "stop", args: undefined, machine: undefined })).toBe(
      '{"v":1,"type":"control","cmd":"stop"}',
    );
  });

  it("v1.2 hello 双形态与设备管理帧：字段序列化精确（undefined 不出现）", () => {
    // 首次登录：token + device
    expect(encodeFrame({ v: 1, type: "hello", token: "t9", device: "Mac/Chrome" })).toBe(
      '{"v":1,"type":"hello","token":"t9","device":"Mac/Chrome"}',
    );
    // 首次登录不带 device：与 v1.1 字节级一致
    expect(encodeFrame({ v: 1, type: "hello", token: "t9", device: undefined })).toBe(
      '{"v":1,"type":"hello","token":"t9"}',
    );
    // 重连：session 形态（token 不出现）
    expect(encodeFrame({ v: 1, type: "hello", session: "st-1" })).toBe(
      '{"v":1,"type":"hello","session":"st-1"}',
    );
    expect(encodeFrame({ v: 1, type: "device_list" })).toBe('{"v":1,"type":"device_list"}');
    expect(encodeFrame({ v: 1, type: "device_revoke", deviceId: "d1" })).toBe(
      '{"v":1,"type":"device_revoke","deviceId":"d1"}',
    );
  });
});

// ---------------------------------------------------------------------------
// v1.4 E2E 帧：e2e_hello / e2e_ack / enc 的解析与序列化
// ---------------------------------------------------------------------------

describe("v1.4 E2E 帧", () => {
  it("e2e_hello 出站序列化字节精确", () => {
    expect(
      encodeFrame({
        v: 1,
        type: "e2e_hello",
        machine: "home-mac",
        ek: "ab".repeat(32),
        n1: "cd".repeat(32),
      }),
    ).toBe(
      `{"v":1,"type":"e2e_hello","machine":"home-mac","ek":"${"ab".repeat(32)}","n1":"${"cd".repeat(32)}"}`,
    );
  });

  it("e2e_ack 解析：全字段齐备通过；身份公钥 ik/id/pk 别名均可", () => {
    const base = (idField: string) =>
      `{"v":1,"type":"e2e_ack","machine":"m","gk":"${"11".repeat(32)}","n2":"${"22".repeat(
        32,
      )}","sig":"${"33".repeat(64)}","${idField}":"${"44".repeat(32)}"}`;
    for (const f of ["ik", "id", "pk"]) {
      const parsed = parseFrame(base(f));
      expect(parsed?.type).toBe("e2e_ack");
      const rec = parsed as Record<string, unknown>;
      expect(rec[f]).toBe("44".repeat(32));
    }
  });

  it("e2e_ack 关键字段缺失 / 非法 → null", () => {
    expect(parseFrame('{"v":1,"type":"e2e_ack","machine":"m","gk":"11","n2":"22","sig":"33"}')).toBeNull(); // 无身份公钥
    expect(parseFrame('{"v":1,"type":"e2e_ack","machine":"m","n2":"22","sig":"33","ik":"44"}')).toBeNull(); // 缺 gk
    expect(parseFrame('{"v":1,"type":"e2e_ack","machine":1,"gk":"11","n2":"22","sig":"33","ik":"44"}')).toBeNull(); // machine 非串
  });

  it("enc 帧解析：machine + box 串即可；缺失 → null", () => {
    expect(parseFrame('{"v":1,"type":"enc","machine":"m","box":"QUJD"}')?.type).toBe("enc");
    expect(parseFrame('{"v":1,"type":"enc","machine":"m"}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"enc","box":"QUJD"}')).toBeNull();
  });

  it("enc 出站序列化（sealFrame 产物直发形态）", () => {
    expect(encodeFrame({ v: 1, type: "enc", machine: "m", box: "QUJD" })).toBe(
      '{"v":1,"type":"enc","machine":"m","box":"QUJD"}',
    );
  });

  it("reducer 对 e2e_ack/enc 静默忽略（ws 层消费，不进消息流）", () => {
    const s0 = initialState();
    const s1 = run(
      s0,
      { type: "frame", frame: frame('{"v":1,"type":"e2e_ack","machine":"m","gk":"11","n2":"22","sig":"33","ik":"44"}') },
      { type: "frame", frame: frame('{"v":1,"type":"enc","machine":"m","box":"QUJD"}') },
    );
    expect(s1).toBe(s0);
  });
});

// ---------------------------------------------------------------------------
// v1.5 准入与管理：管理帧出站形状 / 新入站帧解析 / reducer / 字段校验器
// ---------------------------------------------------------------------------

describe("v1.5 管理帧与配对（出站形状 + 解析 + reducer）", () => {
  it("管理帧出站：六帧逐字段（adminToken 全携带，undefined 不序列化）", () => {
    expect(encodeFrame({ v: 1, type: "machine_list", adminToken: "at" })).toBe(
      '{"v":1,"type":"machine_list","adminToken":"at"}',
    );
    expect(
      encodeFrame({ v: 1, type: "machine_add", adminToken: "at", name: "mac-mini", pubkey: "ab" }),
    ).toBe('{"v":1,"type":"machine_add","adminToken":"at","name":"mac-mini","pubkey":"ab"}');
    expect(encodeFrame({ v: 1, type: "machine_remove", adminToken: "at", name: "mac-mini" })).toBe(
      '{"v":1,"type":"machine_remove","adminToken":"at","name":"mac-mini"}',
    );
    expect(encodeFrame({ v: 1, type: "user_token_rotate", adminToken: "at" })).toBe(
      '{"v":1,"type":"user_token_rotate","adminToken":"at"}',
    );
    expect(encodeFrame({ v: 1, type: "pairing_approve", adminToken: "at", code: "483920" })).toBe(
      '{"v":1,"type":"pairing_approve","adminToken":"at","code":"483920"}',
    );
    expect(encodeFrame({ v: 1, type: "pairing_deny", adminToken: "at", code: "483920" })).toBe(
      '{"v":1,"type":"pairing_deny","adminToken":"at","code":"483920"}',
    );
  });

  it("hello 配对形态出站（pairing+device，无凭证字段）", () => {
    expect(encodeFrame({ v: 1, type: "hello", pairing: true, device: "iPhone/Safari" })).toBe(
      '{"v":1,"type":"hello","pairing":true,"device":"iPhone/Safari"}',
    );
  });

  it("机器名/公钥校验器：与 relay 同规（1..=32 [a-zA-Z0-9_-] / 64hex）", () => {
    expect(machineNameValid("mac-mini_01")).toBe(true);
    expect(machineNameValid("a")).toBe(true);
    expect(machineNameValid("A".repeat(32))).toBe(true);
    expect(machineNameValid("")).toBe(false);
    expect(machineNameValid("A".repeat(33))).toBe(false);
    expect(machineNameValid("机器")).toBe(false);
    expect(machineNameValid("mac mini")).toBe(false);
    expect(machineNameValid("mac.mini")).toBe(false);

    expect(machinePubkeyValid("ab".repeat(32))).toBe(true);
    expect(machinePubkeyValid("AB".repeat(32))).toBe(true); // 大小写宽容
    expect(machinePubkeyValid("ab".repeat(31))).toBe(false);
    expect(machinePubkeyValid("zz".repeat(32))).toBe(false); // 非 hex
  });

  it("parseFrame machines_cfg：合法形状放行；元素缺字段/类型错拒绝", () => {
    const ok = parseFrame(
      '{"v":1,"type":"machines_cfg","machines":[{"name":"m1","pubkey_fp":"aaaa bbbb cccc dddd","created":1900000000}]}',
    );
    expect(ok).toMatchObject({
      type: "machines_cfg",
      machines: [{ name: "m1", pubkey_fp: "aaaa bbbb cccc dddd", created: 1900000000 }],
    });
    // created 可选（容错展示字段）
    expect(
      parseFrame('{"v":1,"type":"machines_cfg","machines":[{"name":"m1","pubkey_fp":"fp"}]}'),
    ).toMatchObject({ machines: [{ name: "m1", pubkey_fp: "fp" }] });
    expect(parseFrame('{"v":1,"type":"machines_cfg"}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"machines_cfg","machines":{}}')).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"machines_cfg","machines":[{"name":"m1"}]}'),
    ).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"machines_cfg","machines":[{"name":"m1","pubkey_fp":"fp","created":"x"}]}'),
    ).toBeNull();
  });

  it("parseFrame user_token_rotated / pairing_request / pairing_pending / error.message", () => {
    expect(parseFrame('{"v":1,"type":"user_token_rotated","token":"' + "ab".repeat(32) + '"}')).toMatchObject({
      type: "user_token_rotated",
    });
    expect(parseFrame('{"v":1,"type":"user_token_rotated"}')).toBeNull();

    expect(
      parseFrame('{"v":1,"type":"pairing_request","code":"483920","device":"iPhone/Safari","expiresAt":1900000000}'),
    ).toMatchObject({ type: "pairing_request", code: "483920" });
    // expiresAt 可选（容错）
    expect(parseFrame('{"v":1,"type":"pairing_request","code":"1","device":"d"}')).not.toBeNull();
    expect(parseFrame('{"v":1,"type":"pairing_request","code":"1"}')).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"pairing_request","code":"1","device":"d","expiresAt":"x"}'),
    ).toBeNull();

    expect(parseFrame('{"v":1,"type":"pairing_pending","code":"483920"}')).toMatchObject({
      type: "pairing_pending",
    });
    expect(parseFrame('{"v":1,"type":"pairing_pending"}')).toBeNull();

    // v1.5 error 可带 message（字符串才放行）
    expect(parseFrame('{"v":1,"type":"error","code":"machine_invalid","message":"bad name"}')).toMatchObject({
      code: "machine_invalid",
      message: "bad name",
    });
    expect(parseFrame('{"v":1,"type":"error","code":"machine_invalid","message":5}')).toBeNull();
  });

  it("reducer：machines_cfg 全量覆盖；user_token_rotated 一次性 + clear", () => {
    const s0 = initialState();
    expect(s0.machinesCfg).toBeNull();
    expect(s0.pairingRequests).toEqual([]);
    expect(s0.rotatedToken).toBeNull();

    const s1 = run(s0, {
      type: "frame",
      frame: frame('{"v":1,"type":"machines_cfg","machines":[{"name":"m1","pubkey_fp":"fp"}]}'),
    });
    expect(s1.machinesCfg).toEqual([{ name: "m1", pubkey_fp: "fp" }]);
    const s2 = run(s1, {
      type: "frame",
      frame: frame('{"v":1,"type":"machines_cfg","machines":[]}'),
    });
    expect(s2.machinesCfg).toEqual([]);

    const s3 = run(s0, {
      type: "frame",
      frame: frame('{"v":1,"type":"user_token_rotated","token":"newtok"}'),
    });
    expect(s3.rotatedToken).toBe("newtok");
    expect(run(s3, { type: "clear_rotated_token" }).rotatedToken).toBeNull();
  });

  it("reducer：pairing_request 按 code 幂等追加；pairing_resolved 移除", () => {
    const s0 = initialState();
    const s1 = run(s0, {
      type: "frame",
      frame: frame('{"v":1,"type":"pairing_request","code":"111111","device":"A","expiresAt":1900000000}'),
    });
    expect(s1.pairingRequests).toHaveLength(1);
    expect(s1.pairingRequests[0]).toMatchObject({ code: "111111", device: "A", expiresAt: 1900000000 });
    // 同码广播重放不重复
    const s2 = run(s1, {
      type: "frame",
      frame: frame('{"v":1,"type":"pairing_request","code":"111111","device":"A"}'),
    });
    expect(s2.pairingRequests).toHaveLength(1);
    // 第二台新设备 → 两张卡
    const s3 = run(s2, {
      type: "frame",
      frame: frame('{"v":1,"type":"pairing_request","code":"222222","device":"B"}'),
    });
    expect(s3.pairingRequests).toHaveLength(2);
    // 应答其一 → 移除对应卡；未知码 no-op
    const s4 = run(s3, { type: "pairing_resolved", code: "111111" });
    expect(s4.pairingRequests.map((p) => p.code)).toEqual(["222222"]);
    expect(run(s4, { type: "pairing_resolved", code: "999999" })).toBe(s4);
  });

  it("reducer：pairing_pending 静默忽略（ws 层消费）", () => {
    const s0 = initialState();
    expect(run(s0, { type: "frame", frame: frame('{"v":1,"type":"pairing_pending","code":"1"}') })).toBe(s0);
  });
});

// ---------------------------------------------------------------------------
// v1.6 工作空间：出站帧形状 / 入站帧解析 / reducer（快照落表 + 增量合并）
// ---------------------------------------------------------------------------

describe("v1.6 工作空间帧（出站形状 + 解析 + reducer）", () => {
  it("出站四帧逐字段（machine 缺省不带字段，undefined 不序列化）", () => {
    expect(encodeFrame({ v: 1, type: "workspace_list" })).toBe('{"v":1,"type":"workspace_list"}');
    expect(encodeFrame({ v: 1, type: "workspace_save", name: "code", path: "/u/code" })).toBe(
      '{"v":1,"type":"workspace_save","name":"code","path":"/u/code"}',
    );
    expect(encodeFrame({ v: 1, type: "workspace_remove", name: "code" })).toBe(
      '{"v":1,"type":"workspace_remove","name":"code"}',
    );
    expect(
      encodeFrame({ v: 1, type: "conv_workdir_set", convId: "web:default", path: "/u/code" }),
    ).toBe('{"v":1,"type":"conv_workdir_set","convId":"web:default","path":"/u/code"}');
    // machine 可选路由键（v1.1 约定，同 message 族）
    expect(encodeFrame({ v: 1, type: "workspace_list", machine: "home-mac" })).toBe(
      '{"v":1,"type":"workspace_list","machine":"home-mac"}',
    );
    expect(
      encodeFrame({ v: 1, type: "workspace_save", name: "x", path: "/x", machine: undefined }),
    ).toBe('{"v":1,"type":"workspace_save","name":"x","path":"/x"}');
    expect(
      encodeFrame({
        v: 1,
        type: "conv_workdir_set",
        convId: "web:default",
        path: "/u/code",
        machine: "home-mac",
      }),
    ).toBe(
      '{"v":1,"type":"conv_workdir_set","convId":"web:default","path":"/u/code","machine":"home-mac"}',
    );
  });

  it("workspaceNameValid：1..=32 非空（trim 后）", () => {
    expect(workspaceNameValid("code")).toBe(true);
    expect(workspaceNameValid("  code  ")).toBe(true); // trim 后合法
    expect(workspaceNameValid("")).toBe(false);
    expect(workspaceNameValid("   ")).toBe(false);
    expect(workspaceNameValid("x".repeat(32))).toBe(true);
    expect(workspaceNameValid("x".repeat(33))).toBe(false);
  });

  it("parseFrame workspaces：合法全形状 / 可选字段（lastUsedAt、machine）；缺字段/错类型拒", () => {
    const ok = parseFrame(
      '{"v":1,"type":"workspaces","default":"/d","workspaces":[{"name":"a","path":"/a","lastUsedAt":100},{"name":"b","path":"/b"}],"convs":{"web:default":"/a","local:work":"/t"},"machine":"home-mac"}',
    );
    expect(ok).toMatchObject({ type: "workspaces", default: "/d", machine: "home-mac" });
    // lastUsedAt 缺席合法；convs 空记录合法；machine 缺席（旧网关）合法
    expect(
      parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{}}'),
    ).toMatchObject({ type: "workspaces", workspaces: [], convs: {} });
    // 缺 default
    expect(parseFrame('{"v":1,"type":"workspaces","workspaces":[],"convs":{}}')).toBeNull();
    // workspaces 非数组 / 元素缺 name / lastUsedAt 非数字
    expect(parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":"x","convs":{}}')).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":[{"path":"/a"}],"convs":{}}'),
    ).toBeNull();
    expect(
      parseFrame(
        '{"v":1,"type":"workspaces","default":"/d","workspaces":[{"name":"a","path":"/a","lastUsedAt":"x"}],"convs":{}}',
      ),
    ).toBeNull();
    // convs 缺席 / 非记录（数组）/ 值非字符串
    expect(parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":[]}')).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":[]}'),
    ).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{"web:a":7}}'),
    ).toBeNull();
    // machine 非字符串
    expect(
      parseFrame('{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{},"machine":1}'),
    ).toBeNull();
  });

  it("parseFrame conv_workdir：合法（machine 可选）；缺字段/错类型拒", () => {
    expect(
      parseFrame('{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/d","machine":"home-mac"}'),
    ).toMatchObject({ type: "conv_workdir", convId: "web:default", workdir: "/d" });
    expect(parseFrame('{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/d"}')).toMatchObject({
      type: "conv_workdir",
    });
    expect(parseFrame('{"v":1,"type":"conv_workdir","convId":"web:default"}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"conv_workdir","workdir":"/d"}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"conv_workdir","convId":1,"workdir":"/d"}')).toBeNull();
    expect(
      parseFrame('{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/d","machine":[]}'),
    ).toBeNull();
  });

  it("reducer：workspaces 快照按 machine 落表（缺席归 DEFAULT_MACHINE），全量覆盖", () => {
    expect(initialState().workspaces).toEqual({});
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"workspaces","default":"/d","workspaces":[{"name":"a","path":"/a"}],"convs":{"web:default":"/a"},"machine":"home-mac"}',
      ),
    });
    expect(s.workspaces["home-mac"]).toEqual({
      default: "/d",
      items: [{ name: "a", path: "/a" }],
      convs: { "web:default": "/a" },
    });
    // 无 machine（旧网关）→ DEFAULT_MACHINE
    s = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"workspaces","default":"/x","workspaces":[],"convs":{}}'),
    });
    expect(s.workspaces[DEFAULT_MACHINE]).toEqual({ default: "/x", items: [], convs: {} });
    // 同机器第二次快照整体覆盖（移除空间后 items 减少）
    s = run(s, {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{},"machine":"home-mac"}',
      ),
    });
    expect(s.workspaces["home-mac"]).toEqual({ default: "/d", items: [], convs: {} });
    // 两台机器互不干扰
    expect(s.workspaces[DEFAULT_MACHINE]).toEqual({ default: "/x", items: [], convs: {} });
  });

  it("reducer：conv_workdir 增量合并进对应机器 convs；无快照机器忽略", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{"web:default":"/d"},"machine":"home-mac"}',
      ),
    });
    s = run(s, {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/u/code","machine":"home-mac"}',
      ),
    });
    expect(s.workspaces["home-mac"]?.convs["web:default"]).toBe("/u/code");
    // 增量合并不动 items/default；其它键保留
    s = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"conv_workdir","convId":"web:side","workdir":"/u/tmp","machine":"home-mac"}'),
    });
    expect(s.workspaces["home-mac"]?.convs).toEqual({
      "web:default": "/u/code",
      "web:side": "/u/tmp",
    });
    expect(s.workspaces["home-mac"]?.default).toBe("/d");
    // 机器取帧的 machine 字段：work-mac 无快照 → 忽略（不造残缺条目）
    const ignored = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/w","machine":"work-mac"}'),
    });
    expect(ignored).toBe(s);
    // 无 machine（旧网关）→ DEFAULT_MACHINE；其无快照同样忽略
    expect(
      run(s, {
        type: "frame",
        frame: frame('{"v":1,"type":"conv_workdir","convId":"web:default","workdir":"/w"}'),
      }),
    ).toBe(s);
  });

  it("reducer：error 帧不改快照（失败附 error 且快照不变，SPA 只认回包）", () => {
    const s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"workspaces","default":"/d","workspaces":[{"name":"a","path":"/a"}],"convs":{},"machine":"home-mac"}',
      ),
    });
    const after = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"error","code":"workspace_invalid","message":"名称已存在"}'),
    });
    expect(after.lastError?.code).toBe("workspace_invalid");
    expect(after.workspaces).toBe(s.workspaces); // 引用不变 = 快照原样
  });
});

// ---------------------------------------------------------------------------
// v1.7 会话持久化：workspaces 帧可选 sessions 段（契约 B 段）
// ---------------------------------------------------------------------------

describe("workspaces.sessions（v1.7 会话持久化）", () => {
  /** 全量快照帧文本（default/workspaces/convs 三段固定形状，尾部 extra 拼接
   *  sessions / machine 段）。 */
  const SNAP = (extra = "") =>
    `{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{}${extra}}`;

  it("parseFrame：sessions 段合法形状放行；元素缺字段/错类型/段非数组拒；旧形态（无 sessions）兼容", () => {
    const ok = parseFrame(
      SNAP(',"sessions":[{"id":"web:s-01","title":"部署巡检","updatedAt":1770000000}]'),
    );
    expect(ok).toMatchObject({
      type: "workspaces",
      sessions: [{ id: "web:s-01", title: "部署巡检", updatedAt: 1770000000 }],
    });
    // 旧网关形态：无 sessions 段（既有三段不动）
    const legacy = parseFrame(SNAP("")) as WorkspacesFrame | null;
    expect(legacy?.sessions).toBeUndefined();
    expect(parseFrame(SNAP(',"sessions":[]'))).toMatchObject({ type: "workspaces", sessions: [] });
    // 元素缺 id / title 非字符串 / updatedAt 非数字 → 整帧拒
    expect(parseFrame(SNAP(',"sessions":[{"title":"x","updatedAt":1}]'))).toBeNull();
    expect(parseFrame(SNAP(',"sessions":[{"id":"web:a","title":1,"updatedAt":1}]'))).toBeNull();
    expect(parseFrame(SNAP(',"sessions":[{"id":"web:a","title":"x","updatedAt":"1"}]'))).toBeNull();
    expect(parseFrame(SNAP(',"sessions":[{"id":"web:a","title":"x"}]'))).toBeNull();
    // 段非数组 → 拒
    expect(parseFrame(SNAP(',"sessions":{}'))).toBeNull();
    expect(parseFrame(SNAP(',"sessions":"x"'))).toBeNull();
  });

  it("reducer：本地不存在的 conv 建空壳条目（lastPreview=title、lastAt=updatedAt*1000、unread=0），convOrder 按 updatedAt 降序插到 default 之后", () => {
    // 乱序到达（升序）也应归一为降序插入；id 恒为完整 conv 形态（web: 前缀）
    const s = run(initialState(), {
      type: "frame",
      frame: frame(
        SNAP(
          ',"sessions":[' +
            '{"id":"web:s-b","title":"任务B","updatedAt":2000},' +
            '{"id":"web:s-a","title":"任务A","updatedAt":3000},' +
            '{"id":"web:manual","title":"手动会话","updatedAt":1000}],"machine":"home-mac"',
        ),
      ),
    });
    const sp = spaceOf(s, "home-mac");
    expect(sp.convOrder).toEqual(["web:default", "web:s-a", "web:s-b", "web:manual"]); // 3000>2000>1000
    expect(sp.convs["web:s-a"]).toEqual({
      entries: [],
      pending: [],
      lastPreview: "任务A",
      lastAt: 3_000_000,
      unread: 0,
    });
    expect(sp.convs["web:manual"]?.lastPreview).toBe("手动会话");
    expect(sp.convs["web:manual"]?.lastAt).toBe(1_000_000);
    // 未读不点亮（离线历史不是新消息）
    expect(sp.convs["web:s-b"]?.unread).toBe(0);
    // default 会话条目仍在（newSpace 预置）
    expect(sp.convs["web:default"]).toBeTruthy();
  });

  it("reducer：已存在（在线期间有消息流）的 conv 不动——本地 title/entries/lastPreview 优先", () => {
    // 同机器（home-mac）先有消息流
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"text","convId":"web:s-a","text":"本地首条消息","machine":"home-mac"}',
      ),
    });
    const localTitle = spaceOf(s, "home-mac").convs["web:s-a"].title;
    expect(localTitle).toBe("本地首条消息");
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(
          ',"sessions":[{"id":"web:s-a","title":"服务端历史名","updatedAt":9000}],"machine":"home-mac"',
        ),
      ),
    });
    const conv = spaceOf(s, "home-mac").convs["web:s-a"];
    expect(conv.title).toBe("本地首条消息"); // 本地派生 title 不被覆盖
    expect(conv.entries).toHaveLength(1); // 本地消息流保留
    expect(conv.lastPreview).not.toBe("服务端历史名"); // 本地概要优先
    // convOrder 位置不动（快照不重排已存在 conv——default 后是既有首位）
    expect(spaceOf(s, "home-mac").convOrder).toEqual(["web:default", "web:s-a"]);
  });

  it("reducer：重复快照幂等（第二次不再改动）；无 sessions 段（旧网关）spaces 引用不变", () => {
    const raw = SNAP(
      ',"sessions":[{"id":"web:s-a","title":"A","updatedAt":3000},{"id":"web:s-b","title":"B","updatedAt":2000}],"machine":"home-mac"',
    );
    const s1 = run(initialState(), { type: "frame", frame: frame(raw) });
    const s2 = run(s1, { type: "frame", frame: frame(raw) });
    expect(spaceOf(s2, "home-mac")).toEqual(spaceOf(s1, "home-mac")); // 幂等
    // 旧形态：无 sessions 段不触发合并（spaces 引用不变）
    const s3 = run(s2, {
      type: "frame",
      frame: frame(SNAP(',"machine":"home-mac"')),
    });
    expect(s3.spaces).toBe(s2.spaces);
    // 机器隔离：另一台机器的快照不动 home-mac 空间
    const s4 = run(s3, {
      type: "frame",
      frame: frame(SNAP(',"sessions":[{"id":"web:s-x","title":"X","updatedAt":1}],"machine":"work-mac"')),
    });
    expect(spaceOf(s4, "work-mac").convOrder).toContain("web:s-x");
    expect(spaceOf(s4, "home-mac").convOrder).not.toContain("web:s-x");
  });

  it("reducer：本地既有其它 conv 时，sessions 新条目插在 default 之后、既有 conv 之前", () => {
    // 先有本地会话流（default 之后追加 web:local-1）
    let s = run(initialState(), {
      type: "frame",
      frame: frame(TEXT_FRAME("hi", "web:local-1")),
    });
    s = run(s, {
      type: "frame",
      frame: frame(SNAP(',"sessions":[{"id":"web:s-a","title":"A","updatedAt":5}]')),
    });
    expect(orderOf(s)).toEqual(["web:default", "web:s-a", "web:local-1"]);
  });
});

// ---------------------------------------------------------------------------
// 会话自动命名：title 派生（reducer）/ 自动短 ID（newAutoConvName）/ convTitle
// ---------------------------------------------------------------------------

describe("会话自动命名（title 派生 + 自动短 ID + convTitle）", () => {
  it("newAutoConvName：s-<4 位小写 hex>；多次调用基本不重复", () => {
    const names = Array.from({ length: 32 }, () => newAutoConvName());
    for (const n of names) {
      expect(n).toMatch(/^s-[0-9a-f]{4}$/);
    }
    // 65536 空间：32 次抽样允许 ≤2 次碰撞（≥3 次概率 ~1e-6，非脆弱阈值）
    expect(new Set(names).size).toBeGreaterThanOrEqual(30);
  });

  it("isAutoConv：web:s-xxxx 命中；手动名 / default / local / 裸短名不命中", () => {
    expect(isAutoConv("web:s-ab12")).toBe(true);
    expect(isAutoConv("web:s-0000")).toBe(true);
    // 手动名形态：前缀相似但非 4 位 hex / 非自动会话
    expect(isAutoConv("web:s-abc")).toBe(false);
    expect(isAutoConv("web:s-ABCD")).toBe(false);
    expect(isAutoConv("web:s-zzzz")).toBe(false);
    expect(isAutoConv("web:work")).toBe(false);
    expect(isAutoConv(DEFAULT_CONV)).toBe(false);
    expect(isAutoConv("local:work")).toBe(false);
    expect(isAutoConv("s-ab12")).toBe(false); // 裸短名非完整 conv 形态
  });

  it("首条 user 回显派生 title：多行取首行、中英混排、前后空白剥除", () => {
    const s = run(
      initialState(),
      { type: "send_echo", convId: "web:s-ab12", text: "  帮我 review 这个 PR  \n第二行不入名" },
    );
    expect(convsOf(s)["web:s-ab12"].title).toBe("帮我 review 这个 PR");
  });

  it("title 超长按码点截 24 字符（中文/emoji 不劈半）", () => {
    const han = run(initialState(), { type: "send_echo", convId: "web:s-ab12", text: "一".repeat(30) });
    const t1 = convsOf(han)["web:s-ab12"].title;
    expect(t1).toBe("一".repeat(CONV_TITLE_MAX_CHARS));
    expect(Array.from(t1 ?? "")).toHaveLength(24);
    // emoji（代理对）：截 24 个码点而非 UTF-16 单元
    const emoji = run(initialState(), { type: "send_echo", convId: "web:s-ab12", text: "😀".repeat(30) });
    const t2 = convsOf(emoji)["web:s-ab12"].title;
    expect(Array.from(t2 ?? "")).toHaveLength(24);
    expect((t2 ?? "").length).toBe(48); // 未劈半：24 码点 × 2 UTF-16 单元
    // 中英混排
    const mixed = run(initialState(), { type: "send_echo", convId: "web:s-ab12", text: "ab一cd二".repeat(10) });
    expect(convsOf(mixed)["web:s-ab12"].title).toBe("ab一cd二".repeat(4));
  });

  it("首条 text 帧同样派生 title（观看端视角：只有响应无 user 回显）", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("响应首行\\n次行", "web:s-ef56")) },
    );
    expect(convsOf(s)["web:s-ef56"].title).toBe("响应首行");
  });

  it("title 一旦写入不再变：后续 user/text/card 均不改写", () => {
    let s = run(initialState(), { type: "send_echo", convId: "web:s-ab12", text: "首条定调" });
    s = run(s, { type: "send_echo", convId: "web:s-ab12", text: "第二条更长的消息不会覆盖" });
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("响应也不覆盖", "web:s-ab12")) });
    s = run(
      s,
      { type: "frame", frame: frame(CARD_FRAME({ convId: "web:s-ab12", messageId: "m1", text: "卡片同样不覆盖" })) },
    );
    expect(convsOf(s)["web:s-ab12"].title).toBe("首条定调");
  });

  it("空摘要不写 title：首条去空白后为空 → 保持未定，后续消息仍可定调", () => {
    let s = run(initialState(), { type: "send_echo", convId: "web:s-ab12", text: "   \n\t " });
    expect(convsOf(s)["web:s-ab12"].title).toBeUndefined();
    // text 帧空串同样不定调
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("", "web:s-ab12")) });
    expect(convsOf(s)["web:s-ab12"].title).toBeUndefined();
    s = run(s, { type: "send_echo", convId: "web:s-ab12", text: "真正首条" });
    expect(convsOf(s)["web:s-ab12"].title).toBe("真正首条");
  });

  it("convTitle：自动会话用 title；无 title（尚未有消息）回退剥前缀短 ID", () => {
    const s = run(initialState(), { type: "select_conv", convId: "web:s-ab12" });
    expect(convTitle(convsOf(s)["web:s-ab12"], "web:s-ab12")).toBe("s-ab12"); // 未收消息：无 conv 状态亦同
    expect(convTitle(undefined, "web:s-ab12")).toBe("s-ab12");
    const withMsg = run(initialState(), { type: "send_echo", convId: "web:s-ab12", text: "自动会话首条" });
    expect(convTitle(convsOf(withMsg)["web:s-ab12"], "web:s-ab12")).toBe("自动会话首条");
  });

  it("convTitle：手动名 / default / local 会话即便有 title 也恒剥前缀（不被自动名覆盖）", () => {
    const manual = run(initialState(), { type: "send_echo", convId: "web:proj", text: "手动名会话首条消息" });
    expect(convsOf(manual)["web:proj"].title).toBe("手动名会话首条消息"); // 一律派生（state 里有）
    expect(convTitle(convsOf(manual)["web:proj"], "web:proj")).toBe("proj"); // 但显示名不用
    const def = run(initialState(), { type: "send_echo", convId: DEFAULT_CONV, text: "默认会话消息" });
    expect(convTitle(convsOf(def)[DEFAULT_CONV], DEFAULT_CONV)).toBe("default");
    const local = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("本地终端", "local:work")) });
    expect(convTitle(convsOf(local)["local:work"], "local:work")).toBe("local:work"); // 非 web: 前缀原样
  });
});
