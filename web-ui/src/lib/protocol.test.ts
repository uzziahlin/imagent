import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ACK_OFFLINE_TIMEOUT_MS,
  ACK_TIMEOUT_MS,
  CONV_TITLE_MAX_CHARS,
  DEFAULT_CONV,
  DEFAULT_MACHINE,
  MAX_STREAM_ENTRIES,
  applyApprovalsSnapshot,
  convDisplayName,
  convTitle,
  deviceSummary,
  encodeFrame,
  initialState,
  isAutoConv,
  machineNameValid,
  machinePubkeyValid,
  machineSupportsAcks,
  newAutoConvName,
  newClientId,
  normalizeExpiryMs,
  parseFrame,
  peerConvOf,
  relativeTime,
  reducer,
  spaceOf,
  timedOutAcks,
  waitingForText,
  windowStatusText,
  workspaceNameValid,
  type Action,
  type AckFrame,
  type ApprovalSnapshotItem,
  type AppState,
  type GatewayInfoFrame,
  type InboundFrame,
  type MachineWindows,
  type PendingApproval,
  type StreamEntry,
  type WindowInfo,
  type WindowStatus,
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

  it("v1.39 card 执行链条目可选 diff：缺席放行、string 合法（超长容忍）、非 string → null", () => {
    const frameWithCalls = (calls: string, type = "card") =>
      `{"v":1,"type":"${type}","convId":"web:default","messageId":"m","state":"streaming","card":{"text":"t","tool_calls":${calls},"thoughts":[],"todos":[],"phase":"outputting","run_secs":5,"terminal":"running"}}`;
    // 缺席 diff：放行（codex fileChange 仅通知场景条目照旧）
    expect(parseFrame(frameWithCalls('[{"name":"Edit","summary":"a.ts","done":true}]'))).not.toBeNull();
    // 字符串 diff（含超长）：放行且原样保留（渲染层截断，校验不拒）
    const longDiff = "x".repeat(50000);
    expect(
      parseFrame(frameWithCalls(`[{"name":"Edit","summary":"a.ts","done":true,"diff":"${longDiff}"}]`)),
    ).toMatchObject({ card: { tool_calls: [{ diff: longDiff }] } });
    // 非 string（数字 / 对象 / null / 布尔）→ 整帧拒收
    expect(parseFrame(frameWithCalls('[{"name":"Edit","summary":"a","done":true,"diff":1}]'))).toBeNull();
    expect(
      parseFrame(frameWithCalls('[{"name":"Edit","summary":"a","done":true,"diff":{"lines":1}}]')),
    ).toBeNull();
    expect(parseFrame(frameWithCalls('[{"name":"Edit","summary":"a","done":true,"diff":null}]'))).toBeNull();
    // 混合数组：任一元素 diff 非法即拒
    expect(
      parseFrame(
        frameWithCalls(
          '[{"name":"a","summary":"s","done":true},{"name":"b","summary":"s","done":true,"diff":true}]',
        ),
      ),
    ).toBeNull();
    // card_patch 同样校验
    expect(
      parseFrame(frameWithCalls('[{"name":"Edit","summary":"a","done":true,"diff":3}]', "card_patch")),
    ).toBeNull();
  });

  it("v1.39 history 深层校验沿用宽松策略：diff 非法不拒整帧（渲染层防御非 string）", () => {
    expect(
      parseFrame(
        '{"v":1,"type":"history","convId":"web:work","entries":[{"kind":"card","key":"h:4","messageId":"m1","card":{"text":"t","tool_calls":[{"name":"Edit","summary":"a","done":true,"diff":1}],"thoughts":[],"todos":[],"phase":"outputting","run_secs":5,"terminal":"done"},"state":"done"}],"lastSeq":4,"trimmed":false}',
      ),
    ).not.toBeNull();
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
    const after = run(s, { type: "send_echo", clientId: "c1", convId: "local:work", text: "继续" });
    const entries = convsOf(after)["local:work"].entries;
    expect(entries).toHaveLength(2);
    expect(entries[1]).toMatchObject({ kind: "user", text: "继续" });
  });

  it("send_echo 追加用户回显到指定 conv", () => {
    const s = run(initialState(), { type: "send_echo", clientId: "c2", convId: "web:work", text: "列出目录" });
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
    s = run(s, { type: "send_echo", clientId: "c3", convId: DEFAULT_CONV, text: "用户提问" });
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
      clientId: `c${i}`,
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
    s = run(s, { type: "send_echo", clientId: "c5", convId: "web:side", text: "用户回显不计数" });
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

  describe("v1.42.1 终态卡片点亮未读（替代驾驶舱已不再收到的「✅ 任务完成」文本）", () => {
    const CONV = "web:long";
    const unreadOf = (s: AppState) => convsOf(s)[CONV].unread;
    const streamingCard = () => frame(CARD_FRAME({ convId: CONV, messageId: "m1", text: "任务卡" }));
    const donePatch = () =>
      frame(CARD_FRAME({ type: "card_patch", convId: CONV, messageId: "m1", text: "完成", terminal: "done" }));
    /** 用户进入会话并发起任务：本机回显物化会话（不计未读），之后仍停在该会话。 */
    const enterAndAsk = (s: AppState) =>
      run(s, { type: "select_conv", convId: CONV }, { type: "send_echo", clientId: "c6", convId: CONV, text: "跑个长任务" });
    /** history 重放帧：一张本地没有的终态卡（重放恒为终态快照）。 */
    const HISTORY_DONE_CARD =
      `{"v":1,"type":"history","convId":"${CONV}","entries":[` +
      `{"kind":"card","key":"h:7","messageId":"m2","card":{"text":"历史里的终态卡","tool_calls":[],"thoughts":[],"todos":[],"phase":"outputting","run_secs":5,"terminal":"done"},"state":"done"}],` +
      `"lastSeq":7,"trimmed":false}`;

    it("非当前会话：流式卡片插入计 1 → select_conv 清零 → 再切走 → 收到终态 card_patch → unread 为 1", () => {
      let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("开场", CONV)) });
      s = run(s, { type: "frame", frame: streamingCard() });
      expect(unreadOf(s)).toBe(2); // text + 流式卡片插入
      s = run(s, { type: "select_conv", convId: CONV });
      expect(unreadOf(s)).toBe(0);
      s = run(s, { type: "select_conv", convId: DEFAULT_CONV });
      s = run(s, { type: "frame", frame: donePatch() });
      expect(unreadOf(s)).toBe(1);
    });

    it("非当前会话且 unread 已为 2 → 终态补丁后仍为 2（未读是「有多少东西要看」，不是事件计数）", () => {
      let s = enterAndAsk(initialState());
      s = run(s, { type: "frame", frame: streamingCard() });
      // 切走后，任务期间的文本输出已经点亮了未读
      s = run(
        s,
        { type: "select_conv", convId: DEFAULT_CONV },
        { type: "frame", frame: frame(TEXT_FRAME("中途输出 1", CONV)) },
        { type: "frame", frame: frame(TEXT_FRAME("中途输出 2", CONV)) },
      );
      expect(unreadOf(s)).toBe(2);
      s = run(s, { type: "frame", frame: donePatch() });
      expect(unreadOf(s)).toBe(2);
    });

    it("当前会话：流式 → 终态，unread 恒 0", () => {
      let s = enterAndAsk(initialState());
      s = run(s, { type: "frame", frame: streamingCard() });
      expect(unreadOf(s)).toBe(0);
      s = run(s, { type: "frame", frame: donePatch() });
      expect(s.currentConv).toBe(CONV);
      expect(unreadOf(s)).toBe(0);
    });

    it("终态卡片再收到同态帧（sync 重发 / 重复终态补丁 / history 重放）→ 不计", () => {
      let s = enterAndAsk(initialState());
      s = run(
        s,
        { type: "frame", frame: streamingCard() },
        { type: "select_conv", convId: DEFAULT_CONV },
      );
      s = run(s, { type: "frame", frame: donePatch() });
      expect(unreadOf(s)).toBe(1); // 状态转换的那一帧
      // 用户看过（清零）再切走：关于这张卡的任何同态帧都不再点亮
      s = run(s, { type: "select_conv", convId: CONV }, { type: "select_conv", convId: DEFAULT_CONV });
      expect(unreadOf(s)).toBe(0);
      s = run(
        s,
        // 重连 sync 重发（card 类型、同 messageId）
        { type: "frame", frame: frame(CARD_FRAME({ convId: CONV, messageId: "m1", text: "完成", terminal: "done" })) },
        // 重复终态补丁
        { type: "frame", frame: donePatch() },
      );
      expect(unreadOf(s)).toBe(0);
      // history 重放恒为终态快照、不经消息流：本地没有的终态卡补进来也不点亮
      s = run(s, { type: "frame", frame: frame(HISTORY_DONE_CARD) });
      expect(unreadOf(s)).toBe(0);
      expect(
        convsOf(s)[CONV].entries.map((e) => (e.kind === "card" ? e.messageId : e.kind)),
      ).toEqual(["user", "m1", "m2"]);
    });

    it("streaming → streaming 的补丁 → 不计（随后的终态补丁才计）", () => {
      let s = enterAndAsk(initialState());
      s = run(
        s,
        { type: "frame", frame: streamingCard() },
        { type: "select_conv", convId: DEFAULT_CONV },
      );
      s = run(
        s,
        { type: "frame", frame: frame(CARD_FRAME({ type: "card_patch", convId: CONV, messageId: "m1", text: "仍在运行" })) },
      );
      expect(unreadOf(s)).toBe(0);
      // 对照：同一会话同一张卡，终态补丁计 1——上面的 0 只因为卡片状态没变
      s = run(s, { type: "frame", frame: donePatch() });
      expect(unreadOf(s)).toBe(1);
    });
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
      { type: "send_echo", clientId: "c7", convId: "web:work", text: "home 回显", machine: "home-mac" },
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
      { type: "send_echo", clientId: "c8", convId: DEFAULT_CONV, text: "用户提问" },
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

  it("reducer：本地不存在的 conv 建空壳条目（lastPreview=title、title=title、lastAt=updatedAt*1000、unread=0），convOrder 按 updatedAt 降序插到 default 之后", () => {
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
      title: "任务A", // v1.9：快照 title 非空同时落 ConvState.title（网关权威显示名）
    });
    expect(sp.convs["web:manual"]?.lastPreview).toBe("手动会话");
    expect(sp.convs["web:manual"]?.lastAt).toBe(1_000_000);
    // 未读不点亮（离线历史不是新消息）
    expect(sp.convs["web:s-b"]?.unread).toBe(0);
    // default 会话条目仍在（newSpace 预置）
    expect(sp.convs["web:default"]).toBeTruthy();
  });

  it("reducer（v1.9）：已存在 conv 的 title 以快照为准升级，entries/概要/未读/排序仍不动", () => {
    // 自动形态会话（web:s-ab12 匹配 AUTO_CONV_RE——convTitle 用 title 显示）
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"text","convId":"web:s-ab12","text":"本地首条消息很长很长","machine":"home-mac"}',
      ),
    });
    expect(spaceOf(s, "home-mac").convs["web:s-ab12"].title).toBe("本地首条消息很长很长");
    const unreadBefore = spaceOf(s, "home-mac").convs["web:s-ab12"].unread; // 非当前会话来信计 1
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(
          ',"sessions":[{"id":"web:s-ab12","title":"语义标题","updatedAt":9000}],"machine":"home-mac"',
        ),
      ),
    });
    const conv = spaceOf(s, "home-mac").convs["web:s-ab12"];
    expect(conv.title).toBe("语义标题"); // v1.9：机械摘要 → 语义标题升级（网关权威）
    expect(conv.entries).toHaveLength(1); // 本地消息流保留
    expect(conv.lastPreview).not.toBe("语义标题"); // 本地概要不动
    expect(conv.unread).toBe(unreadBefore); // 未读不动
    // convOrder 位置不动（快照不重排已存在 conv——default 后是既有首位）
    expect(spaceOf(s, "home-mac").convOrder).toEqual(["web:default", "web:s-ab12"]);
    // convTitle 显示规则不变：自动形态用 title（升级立即可见）
    expect(convTitle(conv, "web:s-ab12")).toBe("语义标题");
  });

  it("reducer（v1.9）：空快照 title 不覆盖本地 title；重复相同快照幂等（title 相同即无操作）", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"text","convId":"web:s-a","text":"本地首条消息","machine":"home-mac"}',
      ),
    });
    // 网关暂无任何可用名（title 空串）：本地派生 title 保留
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(',"sessions":[{"id":"web:s-a","title":"","updatedAt":9000}],"machine":"home-mac"'),
      ),
    });
    expect(spaceOf(s, "home-mac").convs["web:s-a"].title).toBe("本地首条消息");
    // 升级后再收相同快照：title 已一致 → 无操作（幂等）
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(
          ',"sessions":[{"id":"web:s-a","title":"本地首条消息","updatedAt":9000}],"machine":"home-mac"',
        ),
      ),
    });
    const before = spaceOf(s, "home-mac");
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(
          ',"sessions":[{"id":"web:s-a","title":"本地首条消息","updatedAt":9000}],"machine":"home-mac"',
        ),
      ),
    });
    expect(spaceOf(s, "home-mac")).toBe(before); // 引用不变（真无操作）
  });

  it("reducer（v1.9）：手动名会话 title 跟随快照但显示不变（convTitle 恒走 convDisplayName）", () => {
    // 手动名 conv（非自动形态）：先有消息流，本地 title 已派生
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"text","convId":"web:manual","text":"随便什么消息","machine":"home-mac"}',
      ),
    });
    // 网关快照 title = 手动名（手动名 > 语义标题 > 摘要）
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(
          ',"sessions":[{"id":"web:manual","title":"manual","updatedAt":9000}],"machine":"home-mac"',
        ),
      ),
    });
    const conv = spaceOf(s, "home-mac").convs["web:manual"];
    expect(conv.title).toBe("manual"); // title 字段被快照归一（网关权威）
    expect(convTitle(conv, "web:manual")).toBe("manual"); // 显示恒剥前缀，不受 title 影响
    expect(convTitle(conv, "web:manual")).toBe(convDisplayName("web:manual"));
  });

  it("reducer：本地缓存预填（IndexedDB 恢复）的 conv 同样只升级 title——entries/概要原样保留", () => {
    // 模拟挂载恢复：conv 带本地缓存 entries + 派生 title（restoreHistory 产物形状）
    let s = initialState();
    s = {
      ...s,
      spaces: {
        ...s.spaces,
        "home-mac": {
          convs: {
            [DEFAULT_CONV]: { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 },
            "web:s-a": {
              entries: [{ kind: "text", key: "h:text:1", text: "缓存里的历史消息", at: 1234 }],
              pending: [],
              lastPreview: "缓存里的历史消息",
              lastAt: 1234,
              unread: 0,
              title: "缓存里的历史消息",
            },
          },
          convOrder: [DEFAULT_CONV, "web:s-a"],
        },
      },
    };
    // 网关快照携带同 conv 的语义标题 → 仅 title 升级，其余本地优先
    s = run(s, {
      type: "frame",
      frame: frame(
        SNAP(',"sessions":[{"id":"web:s-a","title":"服务端语义标题","updatedAt":9999}],"machine":"home-mac"'),
      ),
    });
    const conv = spaceOf(s, "home-mac").convs["web:s-a"];
    expect(conv.entries).toHaveLength(1);
    expect(conv.entries[0]).toMatchObject({ text: "缓存里的历史消息" });
    expect(conv.title).toBe("服务端语义标题"); // v1.9：title 以快照为准
    expect(conv.lastPreview).toBe("缓存里的历史消息"); // 概要不动
    expect(spaceOf(s, "home-mac").convOrder).toEqual([DEFAULT_CONV, "web:s-a"]);
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
// v1.8 模型菜单动态化：workspaces 帧可选 models 段
// ---------------------------------------------------------------------------

describe("workspaces.models（v1.8 模型菜单动态化）", () => {
  /** 全量快照帧文本（尾部 extra 拼接 models / machine 段）。 */
  const SNAP = (extra = "") =>
    `{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{}${extra}}`;
  const MODELS =
    ',"models":{"current":null,"cliDefault":"glm-4.6","options":[' +
    '{"label":"sonnet","target":"glm-4.6"},{"label":"opus","target":null}]}';

  it("parseFrame：models 段合法形状放行（current/cliDefault 可 null、target 可 null）；旧形态（无 models）兼容", () => {
    const ok = parseFrame(SNAP(MODELS + ',"machine":"home-mac"')) as WorkspacesFrame | null;
    expect(ok?.models).toEqual({
      current: null,
      cliDefault: "glm-4.6",
      options: [
        { label: "sonnet", target: "glm-4.6" },
        { label: "opus", target: null },
      ],
    });
    // current 非字符串 null 形态 + 空 options 同样合法
    expect(
      parseFrame(SNAP(',"models":{"current":"sonnet","cliDefault":null,"options":[]}')),
    ).toMatchObject({ models: { current: "sonnet", cliDefault: null, options: [] } });
    // 旧网关形态：无 models 段
    const legacy = parseFrame(SNAP("")) as WorkspacesFrame | null;
    expect(legacy?.models).toBeUndefined();
  });

  it("parseFrame 反例：current/cliDefault 非法类型、缺 cliDefault、options 元素畸形 → 整帧拒", () => {
    // current 数字 / cliDefault 数字 / 缺 cliDefault / 缺 current
    expect(parseFrame(SNAP(',"models":{"current":1,"cliDefault":null,"options":[]}'))).toBeNull();
    expect(parseFrame(SNAP(',"models":{"current":null,"cliDefault":2,"options":[]}'))).toBeNull();
    expect(parseFrame(SNAP(',"models":{"current":null,"options":[]}'))).toBeNull();
    expect(parseFrame(SNAP(',"models":{"cliDefault":null,"options":[]}'))).toBeNull();
    // options 非数组 / 元素缺 label / label 非字符串 / target 数字
    expect(parseFrame(SNAP(',"models":{"current":null,"cliDefault":null,"options":{}}'))).toBeNull();
    expect(
      parseFrame(SNAP(',"models":{"current":null,"cliDefault":null,"options":[{"target":"x"}]}')),
    ).toBeNull();
    expect(
      parseFrame(SNAP(',"models":{"current":null,"cliDefault":null,"options":[{"label":1}]}')),
    ).toBeNull();
    expect(
      parseFrame(
        SNAP(',"models":{"current":null,"cliDefault":null,"options":[{"label":"a","target":9}]}'),
      ),
    ).toBeNull();
    // models 非对象
    expect(parseFrame(SNAP(',"models":"x"'))).toBeNull();
  });

  it("reducer：models 随快照落 workspaces[machine].models；下一条无 models 的快照清除（整体覆盖语义）", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(SNAP(MODELS + ',"machine":"home-mac"')),
    });
    expect(s.workspaces["home-mac"]?.models).toEqual({
      current: null,
      cliDefault: "glm-4.6",
      options: [
        { label: "sonnet", target: "glm-4.6" },
        { label: "opus", target: null },
      ],
    });
    // 网关撤下映射/配空 official 后的快照（无 models 段）→ 清除（chips 回退内置）
    s = run(s, {
      type: "frame",
      frame: frame(SNAP(',"machine":"home-mac"')),
    });
    expect(s.workspaces["home-mac"]?.models).toBeUndefined();
    // 机器隔离：home-mac 的 models 不落 work-mac
    s = run(s, {
      type: "frame",
      frame: frame(SNAP(MODELS + ',"machine":"home-mac"')),
    });
    s = run(s, {
      type: "frame",
      frame: frame(SNAP(',"machine":"work-mac"')),
    });
    expect(s.workspaces["work-mac"]?.models).toBeUndefined();
    expect(s.workspaces["home-mac"]?.models?.cliDefault).toBe("glm-4.6");
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
      { type: "send_echo", clientId: "c9", convId: "web:s-ab12", text: "  帮我 review 这个 PR  \n第二行不入名" },
    );
    expect(convsOf(s)["web:s-ab12"].title).toBe("帮我 review 这个 PR");
  });

  it("title 超长按码点截 24 字符（中文/emoji 不劈半）", () => {
    const han = run(initialState(), { type: "send_echo", clientId: "c10", convId: "web:s-ab12", text: "一".repeat(30) });
    const t1 = convsOf(han)["web:s-ab12"].title;
    expect(t1).toBe("一".repeat(CONV_TITLE_MAX_CHARS));
    expect(Array.from(t1 ?? "")).toHaveLength(24);
    // emoji（代理对）：截 24 个码点而非 UTF-16 单元
    const emoji = run(initialState(), { type: "send_echo", clientId: "c11", convId: "web:s-ab12", text: "😀".repeat(30) });
    const t2 = convsOf(emoji)["web:s-ab12"].title;
    expect(Array.from(t2 ?? "")).toHaveLength(24);
    expect((t2 ?? "").length).toBe(48); // 未劈半：24 码点 × 2 UTF-16 单元
    // 中英混排
    const mixed = run(initialState(), { type: "send_echo", clientId: "c12", convId: "web:s-ab12", text: "ab一cd二".repeat(10) });
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
    let s = run(initialState(), { type: "send_echo", clientId: "c13", convId: "web:s-ab12", text: "首条定调" });
    s = run(s, { type: "send_echo", clientId: "c14", convId: "web:s-ab12", text: "第二条更长的消息不会覆盖" });
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("响应也不覆盖", "web:s-ab12")) });
    s = run(
      s,
      { type: "frame", frame: frame(CARD_FRAME({ convId: "web:s-ab12", messageId: "m1", text: "卡片同样不覆盖" })) },
    );
    expect(convsOf(s)["web:s-ab12"].title).toBe("首条定调");
  });

  it("空摘要不写 title：首条去空白后为空 → 保持未定，后续消息仍可定调", () => {
    let s = run(initialState(), { type: "send_echo", clientId: "c15", convId: "web:s-ab12", text: "   \n\t " });
    expect(convsOf(s)["web:s-ab12"].title).toBeUndefined();
    // text 帧空串同样不定调
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("", "web:s-ab12")) });
    expect(convsOf(s)["web:s-ab12"].title).toBeUndefined();
    s = run(s, { type: "send_echo", clientId: "c16", convId: "web:s-ab12", text: "真正首条" });
    expect(convsOf(s)["web:s-ab12"].title).toBe("真正首条");
  });

  it("convTitle：自动会话用 title；无 title（尚未有消息）回退剥前缀短 ID", () => {
    const s = run(initialState(), { type: "select_conv", convId: "web:s-ab12" });
    expect(convTitle(convsOf(s)["web:s-ab12"], "web:s-ab12")).toBe("s-ab12"); // 未收消息：无 conv 状态亦同
    expect(convTitle(undefined, "web:s-ab12")).toBe("s-ab12");
    const withMsg = run(initialState(), { type: "send_echo", clientId: "c17", convId: "web:s-ab12", text: "自动会话首条" });
    expect(convTitle(convsOf(withMsg)["web:s-ab12"], "web:s-ab12")).toBe("自动会话首条");
  });

  it("convTitle：手动名 / local 会话即便有 title 也恒剥前缀；default 会话改用 title（未派生仍显 default）", () => {
    const manual = run(initialState(), { type: "send_echo", clientId: "c18", convId: "web:proj", text: "手动名会话首条消息" });
    expect(convsOf(manual)["web:proj"].title).toBe("手动名会话首条消息"); // 一律派生（state 里有）
    expect(convTitle(convsOf(manual)["web:proj"], "web:proj")).toBe("proj"); // 但显示名不用
    const local = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("本地终端", "local:work")) });
    expect(convTitle(convsOf(local)["local:work"], "local:work")).toBe("local:work"); // 非 web: 前缀原样
  });

  it("convTitle（v1.7.x 放宽）：default 会话发首条消息后显示 title；未发消息仍显示 default", () => {
    // 未发消息（无 conv 状态 / 空壳）：回退剥前缀 "default"
    expect(convTitle(undefined, DEFAULT_CONV)).toBe("default");
    expect(convTitle(convsOf(initialState())[DEFAULT_CONV], DEFAULT_CONV)).toBe("default");
    // 发首条消息：title 派生 → 显示名采用（侧栏/状态条同源）
    const def = run(initialState(), { type: "send_echo", clientId: "c19", convId: DEFAULT_CONV, text: "默认会话的第一条消息" });
    expect(convsOf(def)[DEFAULT_CONV].title).toBe("默认会话的第一条消息");
    expect(convTitle(convsOf(def)[DEFAULT_CONV], DEFAULT_CONV)).toBe("默认会话的第一条消息");
    // title 定调后不再变：第二条消息不覆盖显示名
    const def2 = run(def, { type: "send_echo", clientId: "c20", convId: DEFAULT_CONV, text: "第二条消息" });
    expect(convTitle(convsOf(def2)[DEFAULT_CONV], DEFAULT_CONV)).toBe("默认会话的第一条消息");
  });

  it("convTitle（v1.41）：peer: 会话剥前缀直接显示窗口名（conv 即窗口标识，不加前缀）", () => {
    expect(convDisplayName("peer:窗口A")).toBe("窗口A");
    expect(convDisplayName("local:work")).toBe("local:work"); // 既有非 web:/peer: 前缀不受影响
    // 收到窗口入站文本（text 帧落 peer: conv）后，convTitle 显示剥前缀窗口名。
    const s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("窗口消息正文", "peer:窗口A")) });
    expect(convTitle(convsOf(s)["peer:窗口A"], "peer:窗口A")).toBe("窗口A");
  });
});

// ---------------------------------------------------------------------------
// v1.8 会话历史重放：history_query 出站 / history 入站解析 / reducer 合并
// ---------------------------------------------------------------------------

describe("v1.8 会话历史重放（history 帧）", () => {
  /** 终态 card 载荷（历史重放恒为 done 快照）。 */
  const CARD_JSON = (text: string) =>
    `{"text":"${text}","tool_calls":[],"thoughts":[],"todos":[],"phase":"outputting","run_secs":5,"terminal":"done"}`;
  /** history 帧文本（convId 固定 web:work，尾部拼 extra 段）。 */
  const HIST = (extra = "") => `{"v":1,"type":"history","convId":"web:work"${extra}}`;
  const LAST = `"lastSeq":4,"trimmed":false`;

  it("出站 history_query 逐字段（machine 缺省不带字段，undefined 不序列化）", () => {
    expect(encodeFrame({ v: 1, type: "history_query", convId: "web:default", since: 0 })).toBe(
      '{"v":1,"type":"history_query","convId":"web:default","since":0}',
    );
    expect(
      encodeFrame({ v: 1, type: "history_query", convId: "web:default", since: 42, machine: "home-mac" }),
    ).toBe('{"v":1,"type":"history_query","convId":"web:default","since":42,"machine":"home-mac"}');
    expect(
      encodeFrame({ v: 1, type: "history_query", convId: "web:default", since: 0, machine: undefined }),
    ).toBe('{"v":1,"type":"history_query","convId":"web:default","since":0}');
  });

  it("parseFrame history：合法形状放行（text/card 条目、machine 可选、空 entries）", () => {
    const ok = parseFrame(
      HIST(
        `,"entries":[` +
          `{"kind":"text","key":"h:3","text":"你好","at":1000},` +
          `{"kind":"card","key":"h:4","messageId":"m1","card":${CARD_JSON("终态卡")},"state":"done"}],` +
        `"lastSeq":4,"trimmed":false,"machine":"home-mac"`,
      ),
    );
    expect(ok).toMatchObject({ type: "history", convId: "web:work", lastSeq: 4, trimmed: false });
    expect((ok as { entries: unknown[] }).entries).toHaveLength(2);
    // machine 缺席（旧网关归 DEFAULT_MACHINE）；空 entries + lastSeq 0（新会话）合法
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":0,"trimmed":false`))).toMatchObject({
      entries: [],
      lastSeq: 0,
      trimmed: false,
    });
    // trimmed=true（GC 形态）合法
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":9,"trimmed":true`))).toMatchObject({
      trimmed: true,
    });
  });

  it("parseFrame history 反例：字段缺失/类型错/条目形状非法 → 整帧 null", () => {
    // 缺 entries / entries 非数组 / 缺 lastSeq / 缺 trimmed / 缺 convId
    expect(parseFrame(HIST(`,"lastSeq":0,"trimmed":false`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":{},"lastSeq":0,"trimmed":false`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[],"trimmed":false`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":0`))).toBeNull();
    expect(parseFrame(`{"v":1,"type":"history","entries":[],"lastSeq":0,"trimmed":false}`)).toBeNull();
    // lastSeq：负数 / 非整数 / 非数字
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":-1,"trimmed":false`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":1.5,"trimmed":false`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":"0","trimmed":false`))).toBeNull();
    // trimmed 非布尔
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":0,"trimmed":"no"`))).toBeNull();
    // machine 非字符串
    expect(parseFrame(HIST(`,"entries":[],"lastSeq":0,"trimmed":false,"machine":7`))).toBeNull();
    // 条目：kind 非法 / text 缺 key / text 缺 text / card 缺 messageId / card.card 非对象
    expect(parseFrame(HIST(`,"entries":[{"kind":"user","key":"h:1","text":"x"}],${LAST}`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[{"kind":"text","text":"x","at":1}],${LAST}`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[{"kind":"text","key":"h:1","at":1}],${LAST}`))).toBeNull();
    expect(parseFrame(HIST(`,"entries":[{"kind":"card","key":"h:1","card":${CARD_JSON("t")}}],${LAST}`))).toBeNull();
    expect(
      parseFrame(HIST(`,"entries":[{"kind":"card","key":"h:1","messageId":"m","card":"x"}],${LAST}`)),
    ).toBeNull();
    expect(parseFrame(HIST(`,"entries":["h:1"],${LAST}`))).toBeNull();
  });

  it("reducer：本地空 conv 首次物化——重放集整体置入 + 概要/title 派生 + 不计未读", () => {
    const s = run(initialState(), {
      type: "frame",
      frame: frame(
        HIST(
          `,"entries":[{"kind":"text","key":"h:3","text":"历史首条","at":1000},` +
            `{"kind":"card","key":"h:4","messageId":"m1","card":${CARD_JSON("历史终态卡")},"state":"done"}],` +
          `"lastSeq":4,"trimmed":false,"machine":"home-mac"`,
        ),
      ),
    });
    const conv = spaceOf(s, "home-mac").convs["web:work"];
    expect(conv?.entries.map((e) => e.key)).toEqual(["h:3", "h:4"]);
    expect(conv?.lastPreview).toBe("历史终态卡"); // 末条概要
    expect(conv?.lastAt).toBe(1000); // 末条带 at 的（card 无 at，回取 text）
    expect(conv?.title).toBe("历史首条"); // 首条 text 派生
    expect(conv?.unread).toBe(0); // 重放不计未读（非当前 conv 也不点亮）
    expect(spaceOf(s, "home-mac").convOrder).toEqual([DEFAULT_CONV, "web:work"]);
  });

  it("reducer：增量合并按 key 去重 append（本地在前），概要不刷、重复重放幂等", () => {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("实时消息", "web:work")) });
    s = run(s, {
      type: "frame",
      frame: frame(
        HIST(
          `,"entries":[{"kind":"text","key":"h:2","text":"更早的重放","at":500},` +
            `{"kind":"text","key":"h:3","text":"较新重放","at":900}],"lastSeq":3,"trimmed":false`,
        ),
      ),
    });
    const conv = convsOf(s)["web:work"];
    expect(conv.entries.map((e) => e.kind)).toEqual(["text", "text", "text"]);
    expect((conv.entries[1] as { key: string }).key).toBe("h:2"); // 追加在本地条目之后
    expect(conv.lastPreview).toBe("实时消息"); // 本地非空：概要不刷
    // 重复重放同 key：全部命中去重 → 原引用返回（幂等）
    const again = run(s, {
      type: "frame",
      frame: frame(HIST(`,"entries":[{"kind":"text","key":"h:3","text":"较新重放","at":900}],"lastSeq":3,"trimmed":false`)),
    });
    expect(again).toBe(s);
  });

  it("reducer：card 同 messageId 本地优先（重放终态不追加不覆盖）", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(CARD_FRAME({ convId: "web:work", messageId: "m1", text: "本地终态", terminal: "done" })),
    });
    s = run(s, {
      type: "frame",
      frame: frame(
        HIST(
          `,"entries":[{"kind":"card","key":"h:4","messageId":"m1","card":${CARD_JSON("重放终态")},"state":"done"},` +
            `{"kind":"text","key":"h:5","text":"后随文本","at":1}],"lastSeq":5,"trimmed":false`,
        ),
      ),
    });
    const conv = convsOf(s)["web:work"];
    expect(conv.entries).toHaveLength(2); // 重放 card 跳过、text 追加
    expect((conv.entries[0] as { card: { text: string } }).card.text).toBe("本地终态");
    expect((conv.entries[1] as { key: string }).key).toBe("h:5");
  });

  it("reducer：trimmed 整体替换——本地缓存丢弃、重放集重建 + 概要刷新", () => {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("将被丢弃的本地缓存", "web:work")) });
    s = run(s, {
      type: "frame",
      frame: frame(
        HIST(`,"entries":[{"kind":"text","key":"h:9","text":"GC 后的完整历史","at":9000}],"lastSeq":9,"trimmed":true`),
      ),
    });
    const conv = convsOf(s)["web:work"];
    expect(conv.entries.map((e) => e.key)).toEqual(["h:9"]); // 本地条目被丢弃
    expect(conv.lastPreview).toBe("GC 后的完整历史"); // 概要以重放集刷新
    expect(conv.lastAt).toBe(9000);
  });

  it("reducer：空增量（entries=[]）仅水位——state 原样返回", () => {
    const s0 = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("x")) });
    expect(run(s0, { type: "frame", frame: frame(HIST(`,"entries":[],"lastSeq":7,"trimmed":false`)) })).toBe(s0);
  });

  it("reducer：history 按帧 machine 归属机器空间（缺席归 DEFAULT_MACHINE）", () => {
    const s = run(initialState(), {
      type: "frame",
      frame: frame(
        HIST(`,"entries":[{"kind":"text","key":"h:1","text":"别的机器的历史","at":1}],"lastSeq":1,"trimmed":false,"machine":"work-mac"`),
      ),
    });
    expect(spaceOf(s, "work-mac").convs["web:work"]?.entries).toHaveLength(1);
    expect(spaceOf(s, "work-mac").convs["web:work"]?.unread).toBe(0);
    expect(spaceOf(s, DEFAULT_MACHINE).convs["web:work"]).toBeUndefined();
  });

  // v1.36 text 重放去重：实时帧自带网关落盘同源 at，重放条目 key（h:<seq>）
  // 与实时 key（text:N）键空间不相交，(text, at) 精确匹配是唯一可靠对齐凭证。

  it("reducer（v1.36）：实时条目保留帧同源 at；重放 (text,at) 精确匹配 → 跳过（key 不同亦去重）", () => {
    // 实时帧带 at（新网关）：本地条目 key=text:1、at=帧原值（appendEntry 不盖本地时间）
    let s = run(initialState(), {
      type: "frame",
      frame: frame(`{"v":1,"type":"text","convId":"web:work","text":"查询回显","at":1700000000000}`),
    });
    expect(convsOf(s)["web:work"].entries[0]).toEqual({
      kind: "text",
      key: "text:1",
      text: "查询回显",
      at: 1700000000000,
    });
    // 重放同一条消息：key 空间不同（h:9）但 (text, at) 相同 → 跳过不重复
    s = run(s, {
      type: "frame",
      frame: frame(
        HIST(`,"entries":[{"kind":"text","key":"h:9","text":"查询回显","at":1700000000000}],"lastSeq":9,"trimmed":false`),
      ),
    });
    const conv = convsOf(s)["web:work"];
    expect(conv.entries).toHaveLength(1); // 无重复
    expect((conv.entries[0] as { key: string }).key).toBe("text:1"); // 本地条目保留
  });

  it("reducer（v1.36）：同文本异 at 不算重复——两条都保留（/help 两次回显形态）", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(`{"v":1,"type":"text","convId":"web:work","text":"/help","at":1700000000001}`),
    });
    s = run(s, {
      type: "frame",
      frame: frame(
        HIST(
          `,"entries":[{"kind":"text","key":"h:1","text":"/help","at":1700000000001},` +
            `{"kind":"text","key":"h:2","text":"/help","at":1700000000002}],"lastSeq":2,"trimmed":false`,
        ),
      ),
    });
    // 第一条 (text,at) 与本地相同跳过；第二条同文本但 at 不同 → 保留
    expect(convsOf(s)["web:work"].entries.map((e) => e.key)).toEqual(["text:1", "h:2"]);
  });

  it("reducer（v1.36）：本地条目无 at（旧缓存形态）+ 重放带 at → 保留（退化重复，升级一次性残留可接受）", () => {
    // 直接构造本地 text 条目无 at（旧版本缓存/旧网关实时帧盖本地时间的形态
    // ——实时路径 appendEntry 恒盖 at，无 at 只出现在缓存恢复前的旧数据）。
    // conv 用 HIST 夹具的 web:work。
    const base = initialState();
    const s0: AppState = {
      ...base,
      spaces: {
        [DEFAULT_MACHINE]: {
          convs: {
            [DEFAULT_CONV]: { entries: [], pending: [], lastPreview: "", lastAt: 0, unread: 0 },
            "web:work": { entries: [{ kind: "text", key: "text:1", text: "查询回显" }], pending: [], lastPreview: "", lastAt: 0, unread: 0 },
          },
          convOrder: [DEFAULT_CONV, "web:work"],
        },
      },
    };
    const s = run(s0, {
      type: "frame",
      frame: frame(
        HIST(`,"entries":[{"kind":"text","key":"h:9","text":"查询回显","at":1700000000000}],"lastSeq":9,"trimmed":false`),
      ),
    });
    // 无 at 无法与重放对齐 → 重放条目照常追加（重复一份），不误删任何一条
    expect(s0 === s).toBe(false);
    expect(convsOf(s)["web:work"].entries.map((e) => e.key)).toEqual(["text:1", "h:9"]);
  });

  it("reducer（v1.36）：重放集内部同 (text,at) 重复同样跳过；带 at 新重放照常 append", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(`{"v":1,"type":"text","convId":"web:work","text":"实时消息","at":100}`),
    });
    s = run(s, {
      type: "frame",
      frame: frame(
        HIST(
          `,"entries":[{"kind":"text","key":"h:2","text":"实时消息","at":100},` +
            `{"kind":"text","key":"h:3","text":"实时消息","at":100},` +
            `{"kind":"text","key":"h:4","text":"实时消息","at":200}],"lastSeq":4,"trimmed":false`,
        ),
      ),
    });
    // h:2/h:3 同 (text,at) 均命中（本地 + 重放集内已见）→ 只留 h:4（at=200 异值）
    expect(convsOf(s)["web:work"].entries.map((e) => e.key)).toEqual(["text:1", "h:4"]);
  });
});

// ---------------------------------------------------------------------------
// v1.36 text 帧可选 at（网关落盘同源时间戳）：解析校验 + 旧网关（无 at）兼容
// ---------------------------------------------------------------------------

describe("v1.36 text 帧可选 at（解析校验）", () => {
  const T = (extra: string) => `{"v":1,"type":"text","convId":"web:work","text":"x"${extra}}`;

  it("at 缺席放行（旧网关）；合法 unix 毫秒整数放行", () => {
    expect(parseFrame(T(""))).toMatchObject({ type: "text", text: "x" });
    expect(parseFrame(T(`,"at":1700000000000`))).toMatchObject({ at: 1700000000000 });
    expect(parseFrame(T(`,"at":0`))).toMatchObject({ at: 0 });
  });

  it("at 非法拒：负数 / 非整数 / 非数字 / NaN / Infinity", () => {
    expect(parseFrame(T(`,"at":-1`))).toBeNull();
    expect(parseFrame(T(`,"at":1.5`))).toBeNull();
    expect(parseFrame(T(`,"at":"x"`))).toBeNull();
    expect(parseFrame(T(`,"at":null`))).toBeNull();
    // null ≠ 缺席（JSON 里显式 null 是畸形，与 models.current 的可空语义不同）
  });
});

// ---------------------------------------------------------------------------
// v1.41.8 text 帧可选 self（用户自发文本镜像，peer 通道）：解析校验 + reducer
// 用户气泡 / 与本机回显去重 / history 重放补回。背景：网关把用户发给本地
// 窗口的文本镜像回 `peer:<窗口名>` 会话，此前是普通 text 帧——SPA 渲染成
// imagent 的发言（「网关复读却没回答」），在 peer 会话直接打字则与本机
// 回显双显。
// ---------------------------------------------------------------------------

describe("text 帧可选 self（v1.41.8 用户自发文本镜像）", () => {
  const PEER = "peer:win-a";
  /** 网关回投的用户自发文本镜像帧（不带 machine → 归 DEFAULT_MACHINE）。 */
  const SELF = (text: string, convId = PEER) =>
    `{"v":1,"type":"text","convId":"${convId}","text":"${text}","at":1700000000000,"self":true}`;
  const hist = (convId: string, entries: string, lastSeq: number): Action => ({
    type: "frame",
    frame: frame(
      `{"v":1,"type":"history","convId":"${convId}","entries":[${entries}],"lastSeq":${lastSeq},"trimmed":false}`,
    ),
  });
  const selfRow = (seq: number, text: string, at: number) =>
    `{"kind":"text","key":"h:${seq}","text":"${text}","at":${at},"self":true}`;
  const textRow = (seq: number, text: string, at: number) =>
    `{"kind":"text","key":"h:${seq}","text":"${text}","at":${at}}`;
  /** 会话条目的 [kind, key] 序列（顺序与形态一眼可辨）。 */
  const shape = (s: AppState, convId = PEER): (string | undefined)[][] =>
    (convsOf(s)[convId]?.entries ?? []).map((e) => [e.kind, e.key]);

  it("parseFrame：self 为 true / false / 缺席均放行；非 boolean 不拒帧（帧校验不因该字段变化）", () => {
    expect(parseFrame(SELF("x"))).toMatchObject({ type: "text", self: true });
    expect(parseFrame(SELF("x").replace(',"self":true', ""))).toMatchObject({ type: "text" });
    expect(parseFrame(SELF("x").replace('"self":true', '"self":false'))).toMatchObject({
      type: "text",
      self: false,
    });
    for (const bad of ['"self":"true"', '"self":1', '"self":null', '"self":{}']) {
      expect(parseFrame(SELF("x").replace('"self":true', bad))).toMatchObject({ type: "text", text: "x" });
    }
    // self 不能顶替必填字段：缺 text / convId 照旧拒帧。
    expect(parseFrame('{"v":1,"type":"text","convId":"peer:win-a","self":true}')).toBeNull();
  });

  it("reducer：self 文本入为 user 条目（与本地回显同形态、盖本地时间戳、不计未读、概要照常更新）；同帧无 self 仍是 agent text", () => {
    let s = run(initialState(), { type: "frame", frame: frame(SELF("你目前的待办项有哪些？")) });
    const conv = convsOf(s)[PEER];
    expect(shape(s)).toEqual([["user", "user:1"]]);
    const e = conv?.entries[0];
    expect(e).toMatchObject({ kind: "user", text: "你目前的待办项有哪些？" });
    // 本地回显同款盖戳（到达时间，ms epoch）
    expect(e?.kind === "user" && typeof e.at === "number" && e.at > 0).toBe(true);
    expect(conv?.unread).toBe(0); // 非当前会话收到用户自发文本不计未读
    expect(conv?.lastPreview).toBe("你目前的待办项有哪些？");
    expect(conv?.lastAt).toBeGreaterThan(0);
    expect(orderOf(s)).toEqual([DEFAULT_CONV, PEER]); // 会话照常收集进侧栏

    // 对照：同一帧去掉 self → 既有 agent text（帧自带 at 原样保留、计未读）。
    s = run(initialState(), {
      type: "frame",
      frame: frame(SELF("你目前的待办项有哪些？").replace(',"self":true', "")),
    });
    expect(shape(s)).toEqual([["text", "text:1"]]);
    expect(convsOf(s)[PEER]?.entries[0]).toMatchObject({ at: 1700000000000 });
    expect(convsOf(s)[PEER]?.unread).toBe(1);
  });

  it("reducer：与末尾 5 条内的本机回显同文 → 丢弃镜像帧（peer 会话直接打字不再双显）", () => {
    // 真机时序：进入 peer 会话打字 → 本机回显（右侧）→ 网关把同一句镜像回来。
    let s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("已建立与窗口「win-a」的会话", PEER)) },
      { type: "select_conv", convId: PEER },
      { type: "send_echo", clientId: "c21", convId: PEER, text: "帮我跑下构建" },
    );
    const before = s;
    s = run(s, { type: "frame", frame: frame(SELF("帮我跑下构建")) });
    expect(s).toBe(before); // 原引用返回：无新条目、概要/未读/序号均不动
    // v1.44：本机回显的键是 user:<clientId>（不再是序号）；镜像补回的仍是序号键
    expect(shape(s)).toEqual([["text", "text:1"], ["user", "user:c21"]]);

    // 连发多条同文：每条镜像都被自己的回显吸收。
    s = run(s, { type: "send_echo", clientId: "c22", convId: PEER, text: "继续" }, { type: "send_echo", clientId: "c23", convId: PEER, text: "继续" });
    s = run(s, { type: "frame", frame: frame(SELF("继续")) }, { type: "frame", frame: frame(SELF("继续")) });
    expect(shape(s)).toEqual([
      ["text", "text:1"],
      ["user", "user:c21"],
      ["user", "user:c22"],
      ["user", "user:c23"],
    ]);

    // 非当前会话同样去重，且不点亮未读。
    let bg = run(initialState(), { type: "send_echo", clientId: "c24", convId: PEER, text: "后台那句" });
    const bgBefore = bg;
    bg = run(bg, { type: "frame", frame: frame(SELF("后台那句")) });
    expect(bg).toBe(bgBefore);
    expect(convsOf(bg)[PEER]?.unread).toBe(0);
  });

  it("reducer：网关镜像的是 trim 后的文本——回显带首尾空白/换行同样去重", () => {
    // dispatch_peer_message 取 text.trim() 再镜像；本机回显保留用户原样输入。
    let s = run(initialState(), { type: "send_echo", clientId: "c25", convId: PEER, text: "  帮我跑下构建\n" });
    const before = s;
    s = run(s, { type: "frame", frame: frame(SELF("帮我跑下构建")) });
    expect(s).toBe(before);
    expect(convsOf(s)[PEER]?.entries).toHaveLength(1);
  });

  it("reducer：同文回执已被挤出末尾 5 条 → 不再算本条的回显，镜像照常补回（去重窗口语义）", () => {
    let s = run(initialState(), { type: "send_echo", clientId: "c26", convId: PEER, text: "旧的那句" });
    // 其后挤进 5 条窗口回复（text），回显落到第 6 位之外。
    for (let i = 1; i <= 5; i++) {
      s = run(s, { type: "frame", frame: frame(TEXT_FRAME(`回复${i}`, PEER)) });
    }
    s = run(s, { type: "frame", frame: frame(SELF("旧的那句")) });
    expect(shape(s)).toEqual([
      ["user", "user:c26"],
      ["text", "text:2"],
      ["text", "text:3"],
      ["text", "text:4"],
      ["text", "text:5"],
      ["text", "text:6"],
      ["user", "user:7"], // 回显不在窗口内 → 镜像补回
    ]);
    // 恰在窗口边界（回显是倒数第 5 条）仍去重。
    let edge = run(initialState(), { type: "send_echo", clientId: "c27", convId: PEER, text: "边界" });
    for (let i = 1; i <= 4; i++) {
      edge = run(edge, { type: "frame", frame: frame(TEXT_FRAME(`回复${i}`, PEER)) });
    }
    const edgeBefore = edge;
    edge = run(edge, { type: "frame", frame: frame(SELF("边界")) });
    expect(edge).toBe(edgeBefore);
  });

  it("reducer：只认 user 条目的同文——窗口回复（agent text）与同文不等价，不影响去重判定", () => {
    // 会话末尾是窗口的回复「继续」（agent text），没有本机回显：镜像照常入 user 条目。
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("继续", PEER)) });
    s = run(s, { type: "frame", frame: frame(SELF("继续")) });
    expect(shape(s)).toEqual([["text", "text:1"], ["user", "user:2"]]);
  });

  it("reducer：去重只认本机回显——镜像补回的 user 条目（带 self 标记）不吸收后续同文镜像（观看端连发同文不丢）", () => {
    // 本机回显（send_echo）不带 self 标记。
    const echo = convsOf(run(initialState(), { type: "send_echo", clientId: "c28", convId: PEER, text: "继续" }))[PEER]?.entries[0];
    expect(echo).toMatchObject({ kind: "user", text: "继续" });
    expect(echo).not.toHaveProperty("self");

    // 观看端（另一台设备）没有本机回显：对端连发两次「继续」，两条镜像各是一条消息。
    let s = run(
      initialState(),
      { type: "frame", frame: frame(SELF("继续")) },
      { type: "frame", frame: frame(SELF("继续")) },
    );
    expect(shape(s)).toEqual([["user", "user:1"], ["user", "user:2"]]);
    expect(convsOf(s)[PEER]?.entries.every((e) => e.kind === "user" && e.self === true)).toBe(true);

    // 重放补回的条目同理：补回后再到一条同文实时镜像，照常入会话。
    let r = run(initialState(), hist(PEER, selfRow(1, "继续", 1), 1));
    r = run(r, { type: "frame", frame: frame(SELF("继续")) });
    expect(shape(r)).toEqual([["user", "h:1"], ["user", "user:1"]]);
    expect(convsOf(r)[PEER]?.entries[0]).toMatchObject({ self: true });
  });

  it("内存护栏：只丢本机回显——镜像补回的 user 条目（self 标记）等同服务端条目，不丢", () => {
    // 此前镜像是不丢的 text 条目；改成 user 条目后不得因此变成可丢弃对象。
    const replies: Action[] = Array.from({ length: MAX_STREAM_ENTRIES }, (_, i) => ({
      type: "frame" as const,
      frame: frame(TEXT_FRAME(`回复${i}`, PEER)),
    }));
    let s = run(
      initialState(),
      { type: "frame", frame: frame(SELF("镜像补回的那句")) },
      { type: "send_echo", clientId: "c29", convId: PEER, text: "本机回显" },
      ...replies,
    );
    // 共追加 202 条（镜像 1 + 回显 1 + 回复 200）→ 超限丢掉的只能是最早的本机
    // 回显（镜像保留）：恰丢 1 条，余 201。
    const after = convsOf(s)[PEER]?.entries ?? [];
    expect(after).toHaveLength(MAX_STREAM_ENTRIES + 1);
    expect(after[0]).toMatchObject({ kind: "user", text: "镜像补回的那句", self: true });
    expect(after.some((e) => e.kind === "user" && e.text === "本机回显")).toBe(false);
    // 已无本机回显可丢：再进一条不强截断（与纯服务端条目同语义）。
    s = run(s, { type: "frame", frame: frame(SELF("再来一句")) });
    const entries = convsOf(s)[PEER]?.entries ?? [];
    expect(entries).toHaveLength(MAX_STREAM_ENTRIES + 2);
    expect(entries[entries.length - 1]).toMatchObject({ kind: "user", text: "再来一句", self: true });
  });

  it("reducer：self 非布尔按缺席 → agent text（既有行为不变）", () => {
    for (const bad of ['"self":"true"', '"self":1']) {
      const s = run(initialState(), {
        type: "frame",
        frame: frame(SELF("x").replace('"self":true', bad)),
      });
      expect(shape(s)).toEqual([["text", "text:1"]]);
    }
    const off = run(initialState(), {
      type: "frame",
      frame: frame(SELF("x").replace('"self":true', '"self":false')),
    });
    expect(shape(off)).toEqual([["text", "text:1"]]);
  });

  it("reducer：self 帧按 machine 归属机器空间；去重只看同一机器空间内的回显", () => {
    /** 带 machine=home-mac 的镜像帧（多机模式网关出站帧恒带 machine）。 */
    const selfOnHomeMac = (text: string) => `${SELF(text).slice(0, -1)},"machine":"home-mac"}`;
    // 同机器空间：home-mac 上的回显吸收 home-mac 的镜像。
    const before = run(
      initialState(),
      { type: "send_echo", clientId: "c30", convId: PEER, text: "同一句", machine: "home-mac" },
    );
    const same = run(before, { type: "frame", frame: frame(selfOnHomeMac("同一句")) });
    expect(same).toBe(before);
    // 跨机器空间：default 机器上的同文回显不吸收 home-mac 机器的镜像。
    const cross = run(
      initialState(),
      { type: "send_echo", clientId: "c31", convId: PEER, text: "同一句" },
      { type: "frame", frame: frame(selfOnHomeMac("同一句")) },
    );
    expect(shape(cross)).toEqual([["user", "user:c31"]]); // default 空间：仅本机回显
    expect(spaceOf(cross, "home-mac").convs[PEER]?.entries).toMatchObject([
      { kind: "user", key: "user:2", text: "同一句" },
    ]);
    expect(spaceOf(cross, "home-mac").convs[PEER]?.unread).toBe(0);
  });

  // ---- history 重放（落盘 payload 带 self → history 条目透传 → 刷新/跨设备补回）----

  it("parseFrame history：self 条目布尔 / 缺席 / 非布尔均放行；非布尔按缺席重放为 agent text", () => {
    const ok = parseFrame(
      `{"v":1,"type":"history","convId":"${PEER}","entries":[${selfRow(1, "a", 1)},${textRow(2, "b", 2)},{"kind":"text","key":"h:3","text":"c","at":3,"self":"yes"}],"lastSeq":3,"trimmed":false}`,
    );
    expect(ok).toMatchObject({ type: "history", lastSeq: 3 });
    expect((ok as { entries: { self?: unknown }[] }).entries.map((e) => e.self)).toEqual([true, undefined, "yes"]);
    const s = run(initialState(), {
      type: "frame",
      frame: ok as InboundFrame,
    });
    expect(shape(s)).toEqual([["user", "h:1"], ["text", "h:2"], ["text", "h:3"]]);
  });

  it("reducer：history 重放的 self 条目补回为 user 条目（键保 h:<seq>、时间戳取网关 at、不计未读）", () => {
    const s = run(
      initialState(),
      hist(PEER, [selfRow(1, "你目前的待办项有哪些？", 1000), textRow(2, "待办有三项…", 2000), selfRow(3, "继续", 3000)].join(","), 3),
    );
    const conv = convsOf(s)[PEER];
    expect(shape(s)).toEqual([["user", "h:1"], ["text", "h:2"], ["user", "h:3"]]);
    expect(conv?.entries[0]).toMatchObject({ kind: "user", text: "你目前的待办项有哪些？", at: 1000 });
    expect(conv?.entries[2]).toMatchObject({ kind: "user", text: "继续", at: 3000 });
    expect(conv?.unread).toBe(0); // 重放不计未读
    expect(conv?.lastPreview).toBe("继续"); // 末条概要
    expect(conv?.lastAt).toBe(3000); // 末条是 self（user）条目：概要时间取其网关 at
    expect(conv?.title).toBe("你目前的待办项有哪些？"); // 首条（self）定调，与 appendEntry 的 user/text 口径一致
    expect(spaceOf(s, DEFAULT_MACHINE).convOrder).toEqual([DEFAULT_CONV, PEER]);
  });

  it("reducer：重放的 self 条目与本地回显同文 → 吸收不重复（回显盖本地时间，与网关 at 不同源）；重复重放幂等", () => {
    // 场景：peer 会话打字后切走/重连 → history_query 把这条消息的落盘行重放
    // 回来。本机已有回显，不能再冒出第二个气泡。
    let s = run(initialState(), { type: "send_echo", clientId: "c32", convId: PEER, text: "帮我跑下构建" });
    const before = s;
    s = run(s, hist(PEER, selfRow(5, "帮我跑下构建", 1700000000000), 5));
    expect(s).toBe(before); // 原引用返回
    expect(run(s, hist(PEER, selfRow(5, "帮我跑下构建", 1700000000000), 5))).toBe(s);
  });

  it("reducer：按文本计数吸收——每条本机回显至多吸收一条同文重放，多出的补回（跨设备同文不丢）", () => {
    // 本机发过 2 次「继续」，另一台设备也发过 2 次 → 落盘 4 行。
    let s = run(
      initialState(),
      { type: "send_echo", clientId: "c33", convId: PEER, text: "继续" },
      { type: "send_echo", clientId: "c34", convId: PEER, text: "继续" },
    );
    s = run(
      s,
      hist(PEER, [selfRow(1, "继续", 1), selfRow(2, "继续", 2), selfRow(3, "继续", 3), selfRow(4, "继续", 4)].join(","), 4),
    );
    expect(shape(s)).toEqual([
      ["user", "user:c33"],
      ["user", "user:c34"],
      ["user", "h:3"],
      ["user", "h:4"],
    ]);
  });

  it("reducer：混合重放——本机回显吸收自己的 self 行、窗口回复按 (text,at) 去重、他机发出的 self 行补回为 user", () => {
    // 本机时序：回显 → 网关镜像帧（已被去重）→ 窗口回复（实时 text）。
    let s = run(
      initialState(),
      { type: "send_echo", clientId: "c35", convId: PEER, text: "帮我跑下构建" },
      { type: "frame", frame: frame(SELF("帮我跑下构建")) },
      { type: "frame", frame: frame(`{"v":1,"type":"text","convId":"${PEER}","text":"已跑完，构建通过","at":1700000005000}`) },
    );
    expect(shape(s)).toEqual([["user", "user:c35"], ["text", "text:2"]]);
    // 重放三行：自己的（吸收）+ 回复（(text,at) 命中）+ 别的设备发的（补回）。
    s = run(
      s,
      hist(
        PEER,
        [
          selfRow(1, "帮我跑下构建", 1700000000000),
          textRow(2, "已跑完，构建通过", 1700000005000),
          selfRow(3, "再跑一次测试", 1700000009000),
        ].join(","),
        3,
      ),
    );
    expect(shape(s)).toEqual([["user", "user:c35"], ["text", "text:2"], ["user", "h:3"]]);
    // 本地非空：概要不刷（既有语义）。
    expect(convsOf(s)[PEER]?.lastPreview).toBe("已跑完，构建通过");
  });

  it("reducer：trimmed 整体替换——本地缓存丢弃，self 条目重建为 user 条目、普通行为 text", () => {
    const s = run(
      initialState(),
      { type: "send_echo", clientId: "c36", convId: PEER, text: "将被丢弃的本地回显" },
      {
        type: "frame",
        frame: frame(
          `{"v":1,"type":"history","convId":"${PEER}","entries":[${selfRow(9, "GC 后的历史", 9000)},${textRow(10, "回复", 9500)}],"lastSeq":10,"trimmed":true}`,
        ),
      },
    );
    expect(shape(s)).toEqual([["user", "h:9"], ["text", "h:10"]]);
    const conv = convsOf(s)[PEER];
    expect(conv?.lastPreview).toBe("回复");
    expect(conv?.lastAt).toBe(9500);
  });
});

// ---------------------------------------------------------------------------
// v1.36 workspaces.backends 可选段（新会话引擎选择）：合法采纳、畸形整段丢弃
// 按缺席（不拒帧）、缺席（旧网关）透传 undefined
// ---------------------------------------------------------------------------

describe("workspaces.backends（v1.36 新会话引擎选择）", () => {
  const WS = (extra = "") =>
    `{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{}` +
    `,"machine":"home-mac"${extra}}`;
  const BK =
    `,"backends":{"current":"claude-code","options":["default","claude-code","codex-app"]}`;

  it("reducer：合法 backends 段随快照落表（current + options 保序）", () => {
    const s = run(initialState(), { type: "frame", frame: frame(WS(BK)) });
    expect(s.workspaces["home-mac"].backends).toEqual({
      current: "claude-code",
      options: ["default", "claude-code", "codex-app"],
    });
    // 其余快照段照常落表
    expect(s.workspaces["home-mac"].default).toBe("/d");
  });

  it("reducer：畸形 backends 段整段丢弃按缺席（不拒帧——其余快照段照常落表）", () => {
    const bads = [
      `,"backends":{"current":"","options":["default"]}`, // current 空串
      `,"backends":{"current":"claude-code"}`, // options 缺失
      `,"backends":{"current":"claude-code","options":[]}`, // options 空数组（契约非空）
      `,"backends":{"current":"claude-code","options":["default",7]}`, // 元素非 string
      `,"backends":{"current":"claude-code","options":["default",""]}`, // 元素空串
      `,"backends":{"current":7,"options":["default"]}`, // current 非字符串
      `,"backends":"claude-code"`, // 段非对象
    ];
    for (const bad of bads) {
      const s = run(initialState(), { type: "frame", frame: frame(WS(bad)) });
      expect(s.workspaces["home-mac"].backends).toBeUndefined(); // 段丢弃按缺席
      expect(s.workspaces["home-mac"].default).toBe("/d"); // 帧其余部分照收（未拒）
    }
  });

  it("reducer：无 backends 段（旧网关/空注册表）→ backends undefined；重复快照整体覆盖", () => {
    const s = run(initialState(), { type: "frame", frame: frame(WS("")) });
    expect(s.workspaces["home-mac"].backends).toBeUndefined();
    // 下一条快照带段则覆盖（整体覆盖语义同 models）
    const s2 = run(s, { type: "frame", frame: frame(WS(BK)) });
    expect(s2.workspaces["home-mac"].backends?.current).toBe("claude-code");
    // 再来一条无段的快照 → 回落 undefined（缺席即清除）
    const s3 = run(s2, { type: "frame", frame: frame(WS("")) });
    expect(s3.workspaces["home-mac"].backends).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// v1.37 审批倒计时契约：approval_request 帧可选 timeoutSecs（秒，超时预算，
// 非剩余时间；SPA 以本地收到时刻起倒数，缺席 = 旧网关不渲染倒计时）
// ---------------------------------------------------------------------------

const APPROVAL_REQ = (extra = "") =>
  `{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls"${extra}}`;

describe("v1.37 approval_request timeoutSecs", () => {
  it("带 timeoutSecs 解析保留；reducer pending 条目携带 timeoutSecs + 收到时刻 at", () => {
    const f = parseFrame(APPROVAL_REQ(',"timeoutSecs":30'));
    expect(f).toMatchObject({ type: "approval_request", timeoutSecs: 30 });
    const s = run(initialState(), { type: "frame", frame: f as InboundFrame });
    const p = convsOf(s)[DEFAULT_CONV].pending[0];
    expect(p.timeoutSecs).toBe(30);
    expect(typeof p.at).toBe("number"); // 倒计时锚点（毫秒，本地收到时刻）
  });

  it("timeoutSecs 缺席（旧网关）：解析合法、pending 不落 timeoutSecs 键", () => {
    const f = parseFrame(APPROVAL_REQ());
    expect(f).toMatchObject({ type: "approval_request" });
    expect(f).not.toHaveProperty("timeoutSecs");
    const s = run(initialState(), { type: "frame", frame: f as InboundFrame });
    expect(convsOf(s)[DEFAULT_CONV].pending[0].timeoutSecs).toBeUndefined();
  });

  it("timeoutSecs 畸形（0/负数/字符串/null/非有限）→ null 拒帧", () => {
    const bads = [',"timeoutSecs":0', ',"timeoutSecs":-5', ',"timeoutSecs":"30"', ',"timeoutSecs":null', ',"timeoutSecs":1e999'];
    for (const bad of bads) {
      expect(parseFrame(APPROVAL_REQ(bad))).toBeNull();
    }
  });
});

// ---------------------------------------------------------------------------
// v1.40 语音转写 whisper 主路径：asr_result 回包（校验 + reducer 防御）与
// workspaces 快照 asr 能力通告
// ---------------------------------------------------------------------------

describe("asr_result（v1.40 语音转写回包）", () => {
  it("parseFrame 合法帧：text/durationMs/machine 齐备放行；machine 缺席（单机）放行；durationMs 小数宽容", () => {
    expect(
      parseFrame(
        '{"v":1,"type":"asr_result","text":"识别文本","durationMs":1234,"machine":"home-mac"}',
      ),
    ).toMatchObject({ type: "asr_result", text: "识别文本", durationMs: 1234, machine: "home-mac" });
    expect(parseFrame('{"v":1,"type":"asr_result","text":"","durationMs":0}')).toMatchObject({
      type: "asr_result",
      durationMs: 0,
    });
    // durationMs 非整数宽容（重采样边界可产生小数毫秒）
    expect(parseFrame('{"v":1,"type":"asr_result","text":"x","durationMs":12.5}')).toBeTruthy();
  });

  it("parseFrame 反例：text 缺失/非串、durationMs 缺失/非数/负数/非有限、machine 非串 → 拒帧", () => {
    const bads = [
      '{"v":1,"type":"asr_result","durationMs":1}',
      '{"v":1,"type":"asr_result","text":3,"durationMs":1}',
      '{"v":1,"type":"asr_result","text":"x"}',
      '{"v":1,"type":"asr_result","text":"x","durationMs":"1"}',
      '{"v":1,"type":"asr_result","text":"x","durationMs":-1}',
      '{"v":1,"type":"asr_result","text":"x","durationMs":1e999}',
      '{"v":1,"type":"asr_result","text":"x","durationMs":1,"machine":7}',
    ];
    for (const bad of bads) {
      expect(parseFrame(bad)).toBeNull();
    }
  });

  it("reducer 防御：asr_result 不进消息流（不落 entries/概要）——ws 连接层拦截路径的上游漏拦兜底", () => {
    const base = initialState();
    const s = run(base, {
      type: "frame",
      frame: frame('{"v":1,"type":"asr_result","text":"迟到回包","durationMs":100}'),
    });
    expect(convsOf(s)[DEFAULT_CONV].entries).toHaveLength(0);
    expect(s).toBe(base); // 静默忽略（state 原样）
  });

  it("encodeFrame：asr_request 出站字段形状（契约字段名不得偏离；单机模式不带 machine）", () => {
    const wire = JSON.parse(
      encodeFrame({ v: 1, type: "asr_request", audio: "QUJD", mime: "audio/wav", durationMs: 1234 }),
    ) as Record<string, unknown>;
    expect(wire).toMatchObject({
      v: 1,
      type: "asr_request",
      audio: "QUJD",
      mime: "audio/wav",
      durationMs: 1234,
    });
    expect(wire).not.toHaveProperty("machine");
  });
});

describe("workspaces.asr（v1.40 语音转写能力通告）", () => {
  const SNAP = (extra = "") =>
    `{"v":1,"type":"workspaces","default":"/d","workspaces":[],"convs":{}${extra}}`;

  it("parseFrame：asr true/false 合法、缺席放行（旧网关同义 false）；非 boolean 拒帧", () => {
    expect(parseFrame(SNAP(',"asr":true,"machine":"home-mac"'))).toMatchObject({ asr: true });
    expect(parseFrame(SNAP(',"asr":false'))).toMatchObject({ asr: false });
    expect(parseFrame(SNAP())).not.toHaveProperty("asr");
    expect(parseFrame(SNAP(',"asr":1'))).toBeNull();
    expect(parseFrame(SNAP(',"asr":"true"'))).toBeNull();
    expect(parseFrame(SNAP(',"asr":null'))).toBeNull();
  });

  it("reducer：asr 随快照落表（true = whisper 可用）；下一条无 asr 的快照清除（整体覆盖语义）", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(SNAP(',"asr":true,"machine":"home-mac"')),
    });
    expect(s.workspaces["home-mac"]?.asr).toBe(true);
    s = run(s, { type: "frame", frame: frame(SNAP(',"machine":"home-mac"')) });
    expect(s.workspaces["home-mac"]?.asr).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// v1.41.3 会话删除（delete_conv reducer：本地先行移除 + 快照权威收敛的前半环）
// ---------------------------------------------------------------------------

describe("reducer delete_conv（v1.41.3）", () => {
  /** 种子：default + web:side（各一条消息）+ web:other；返回已构造状态。 */
  function seeded(): AppState {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("首页")) });
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("侧栏", "web:side")) });
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("其它", "web:other")) });
    return s;
  }

  it("删除条目：convs 移除 + convOrder 摘除（entries/未读随条目清），其它会话不动", () => {
    const s = run(seeded(), { type: "delete_conv", convId: "web:side" });
    expect(s.spaces[DEFAULT_MACHINE].convs["web:side"]).toBeUndefined();
    expect(orderOf(s)).toEqual([DEFAULT_CONV, "web:other"]);
    expect(convsOf(s)["web:other"].entries).toHaveLength(1);
  });

  it("删除当前会话：currentConv 切回 DEFAULT_CONV（被删条目不再可达）", () => {
    let s = run(seeded(), { type: "select_conv", convId: "web:side" });
    expect(s.currentConv).toBe("web:side");
    s = run(s, { type: "delete_conv", convId: "web:side" });
    expect(s.currentConv).toBe(DEFAULT_CONV);
  });

  it("删除非当前会话：currentConv 不动", () => {
    const s = run(seeded(), { type: "delete_conv", convId: "web:side" });
    expect(s.currentConv).toBe(DEFAULT_CONV);
  });

  it("幂等：条目不存在原样返回；DEFAULT_CONV 恒存在不可删（防御性）", () => {
    const s = seeded();
    expect(run(s, { type: "delete_conv", convId: "web:ghost" })).toBe(s);
    const after = run(s, { type: "delete_conv", convId: DEFAULT_CONV });
    expect(orderOf(after)).toContain(DEFAULT_CONV);
    expect(convsOf(after)[DEFAULT_CONV]).toBeDefined();
  });

  it("机器维度：删除落指定机器空间，其它机器不受牵连", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"text","convId":"web:home-only","text":"家里消息","machine":"home-mac"}',
      ),
    });
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("默认机消息")) });
    expect(s.spaces["home-mac"].convs["web:home-only"]).toBeDefined();
    s = run(s, { type: "delete_conv", convId: "web:home-only", machine: "home-mac" });
    expect(s.spaces["home-mac"].convs["web:home-only"]).toBeUndefined();
    expect(orderOf(s)).toContain(DEFAULT_CONV);
    expect(convsOf(s)[DEFAULT_CONV].entries).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// v1.42 待审批快照：approvals_query / approvals 帧契约（出站形状 + 入站逐字段
// 校验）与 reducer 防御；applyApprovalsSnapshot 是快照套用到会话空间的纯归约
// ---------------------------------------------------------------------------

describe("approvals 帧（v1.42 待审批快照）", () => {
  /** 契约示例里的条目。 */
  const item = (extra: Record<string, unknown> = {}) => ({
    convId: "web:s-192d",
    requestId: "req-1",
    tool: "Bash",
    inputSummary: "npm test",
    timeoutSecs: 731,
    ...extra,
  });
  const approvalsFrame = (items: unknown, head = '"machine":"home-mac",') =>
    `{"v":1,"type":"approvals",${head}"items":${JSON.stringify(items)}}`;
  const APPROVAL_REQ = (requestId: string, convId = DEFAULT_CONV, machine?: string) =>
    `{"v":1,"type":"approval_request","convId":"${convId}","requestId":"${requestId}","tool":"Bash","inputSummary":"ls"${
      machine === undefined ? "" : `,"machine":"${machine}"`
    }}`;

  it("parseFrame 合法帧：契约示例逐字段保留；items 可为空数组", () => {
    // 任务书契约示例原文
    const f = parseFrame(
      '{"v":1,"type":"approvals","machine":"home-mac","items":[{"convId":"web:s-192d","requestId":"<id>","tool":"Bash","inputSummary":"npm test","timeoutSecs":731}]}',
    );
    expect(f).toEqual({
      v: 1,
      type: "approvals",
      machine: "home-mac",
      items: [{ convId: "web:s-192d", requestId: "<id>", tool: "Bash", inputSummary: "npm test", timeoutSecs: 731 }],
    });
    // 空集合 = 该机器没有待审批（合法且有意义：会让面板清空）
    expect(parseFrame(approvalsFrame([]))).toEqual({ v: 1, type: "approvals", machine: "home-mac", items: [] });
    // inputSummary 可为空串；timeoutSecs 为 1 是合法的最后一秒
    expect(parseFrame(approvalsFrame([item({ inputSummary: "", timeoutSecs: 1 })]))).toBeTruthy();
  });

  it("parseFrame 拒帧：machine 缺失 / 空串 / 非字符串；items 缺失 / 非数组", () => {
    expect(parseFrame(approvalsFrame([item()], ""))).toBeNull(); // machine 缺失
    expect(parseFrame(approvalsFrame([item()], '"machine":"",'))).toBeNull();
    expect(parseFrame(approvalsFrame([item()], '"machine":7,'))).toBeNull();
    expect(parseFrame(approvalsFrame([item()], '"machine":null,'))).toBeNull();
    expect(parseFrame('{"v":1,"type":"approvals","machine":"home-mac"}')).toBeNull(); // items 缺失
    expect(parseFrame('{"v":1,"type":"approvals","machine":"home-mac","items":{}}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"approvals","machine":"home-mac","items":null}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"approvals","machine":"home-mac","items":"x"}')).toBeNull();
  });

  it("parseFrame 拒帧：任一条目畸形即整帧拒收（缺字段 / 空串 / 类型不符 / 非对象）", () => {
    const badItems: unknown[] = [
      "req-1", // 元素不是对象
      null,
      [],
      item({ convId: undefined }), // JSON 序列化会丢键 → 缺字段
      item({ convId: "" }),
      item({ convId: 7 }),
      item({ requestId: undefined }),
      item({ requestId: "" }),
      item({ tool: undefined }),
      item({ tool: "" }),
      item({ inputSummary: undefined }),
      item({ inputSummary: null }),
      item({ inputSummary: 3 }),
    ];
    for (const bad of badItems) {
      expect(parseFrame(approvalsFrame([item(), bad])), JSON.stringify(bad)).toBeNull(); // 好条目救不了坏条目
    }
  });

  it("parseFrame 拒帧：timeoutSecs 须为正整数（0 / 负 / 小数 / 字符串 / 缺失 / 非有限）", () => {
    for (const timeoutSecs of [0, -1, 0.5, 731.25, "731", null, undefined, 1e999, Number.NaN]) {
      expect(
        parseFrame(approvalsFrame([item({ timeoutSecs })])),
        `timeoutSecs=${String(timeoutSecs)}`,
      ).toBeNull();
    }
  });

  it("encodeFrame：approvals_query 出站字段形状（契约字段名不得偏离；machine 缺省不带）", () => {
    expect(encodeFrame({ v: 1, type: "approvals_query", machine: "home-mac" })).toBe(
      '{"v":1,"type":"approvals_query","machine":"home-mac"}',
    );
    expect(JSON.parse(encodeFrame({ v: 1, type: "approvals_query" }))).toEqual({
      v: 1,
      type: "approvals_query",
    });
  });

  it("reducer 防御：原始 approvals 帧不进帧归约（上游漏拦兜底）——pending / entries 原样", () => {
    const base = run(
      initialState(),
      { type: "frame", frame: frame(APPROVAL_REQ("r-old")) },
    );
    const s = run(base, { type: "frame", frame: frame(approvalsFrame([])) });
    expect(s).toBe(base); // 没有 queriedAt 就套用 = 按完全权威清掉实时审批，故必须忽略
  });
});
describe("applyApprovalsSnapshot（v1.42 待审批快照对齐）", () => {
  const M = "home-mac";
  /** 可控时钟：实时条目的 at、快照条目的 at、queriedAt 全取 Date.now()，
   *  用 spy 固定它，让「查询之后 / 之前到达」成为可精确构造的条件（真实时钟下
   *  连续两帧常落在同一毫秒，无法区分）。 */
  let now = 0;
  beforeEach(() => {
    now = 1_795_000_000_000;
    vi.spyOn(Date, "now").mockImplementation(() => now);
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });
  const advance = (ms: number): void => {
    now += ms;
  };

  const snap = (requestId: string, extra: Partial<ApprovalSnapshotItem> = {}): ApprovalSnapshotItem => ({
    convId: DEFAULT_CONV,
    requestId,
    tool: "Bash",
    inputSummary: `cmd ${requestId}`,
    timeoutSecs: 731,
    ...extra,
  });
  /** 实时 approval_request 帧（带 900s 超时预算，at = 此刻时钟）。 */
  const realtimeReq = (requestId: string, convId = DEFAULT_CONV, machine = M) =>
    frame(
      `{"v":1,"type":"approval_request","convId":"${convId}","requestId":"${requestId}","tool":"Edit","inputSummary":"实时 ${requestId}","timeoutSecs":900,"machine":"${machine}"}`,
    );
  const resolvedFrame = (requestId: string, decision = "allow") =>
    frame(
      `{"v":1,"type":"approval_resolved","convId":"${DEFAULT_CONV}","requestId":"${requestId}","decision":"${decision}","machine":"${M}"}`,
    );
  const pendingOf = (s: AppState, convId = DEFAULT_CONV, machine = M): PendingApproval[] =>
    spaceOf(s, machine).convs[convId]?.pending ?? [];
  const ids = (list: PendingApproval[]): string[] => list.map((p) => p.requestId);
  /** 侧栏 / 审批面板的派生方式（App.allPending 同构）：按机器空间 convOrder 遍历。 */
  const listPending = (s: AppState, machine: string): string[] => {
    const sp = spaceOf(s, machine);
    const out: string[] = [];
    for (const c of sp.convOrder) {
      out.push(...(sp.convs[c]?.pending ?? []).map((p) => p.requestId));
    }
    return out;
  };

  it("① 空快照清掉旧 pending（离开期间已超时 / 已解决的审批不再挂到刷新）", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r1") });
    advance(10);
    s = run(s, { type: "frame", frame: realtimeReq("r2") });
    expect(ids(pendingOf(s))).toEqual(["r1", "r2"]);
    advance(60_000);
    const queriedAt = now;
    advance(100);
    const after = applyApprovalsSnapshot(s, M, [], queriedAt);
    expect(pendingOf(after)).toEqual([]);
    expect(after).not.toBe(s);
  });

  it("② 快照条目替换旧 pending：timeoutSecs 取快照的剩余值、at 等于套用时刻、其余字段照搬", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r1") });
    // 实时条目：timeoutSecs 是 900s 超时预算
    expect(pendingOf(s)[0]).toMatchObject({ requestId: "r1", tool: "Edit", timeoutSecs: 900, at: now });
    const queriedAt = now;
    advance(169_000); // 已过 169s：剩余 731s
    const after = applyApprovalsSnapshot(
      s,
      M,
      [snap("r1", { tool: "Bash", inputSummary: "npm test", timeoutSecs: 731 })],
      queriedAt,
    );
    // 形状与 addApproval 写入的一致（七个字段，无多无少）
    expect(pendingOf(after)[0]).toEqual({
      machine: M,
      convId: DEFAULT_CONV,
      requestId: "r1",
      tool: "Bash",
      inputSummary: "npm test",
      at: now,
      timeoutSecs: 731,
    });
    // 倒计时语义：截止时刻 = 套用时刻 + 剩余秒数（与实时条目的 收到时刻 + 900s 一致）
    expect(pendingOf(after)[0].at + 731_000).toBe(queriedAt + 900_000);
  });

  it("③ 查询之后到达的实时条目被保留（at > queriedAt）；与快照 requestId 重复时以快照为准、不重复", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r-old") });
    advance(10);
    const queriedAt = now; // 查询发出：此刻 r-old 已在网关侧，r-new 还没有
    advance(10);
    s = run(s, { type: "frame", frame: realtimeReq("r-new") }); // 查询之后才到达

    // 快照（生成于 queriedAt）不含 r-new：必须保留，不能被「不在快照里」误删
    const keep = applyApprovalsSnapshot(s, M, [snap("r-old")], queriedAt);
    expect(ids(pendingOf(keep))).toEqual(["r-old", "r-new"]); // 快照条目在前，保留的实时条目在后
    expect(pendingOf(keep)[0].at).toBe(now); // 快照条目：锚点换成套用时刻
    expect(pendingOf(keep)[1]).toBe(pendingOf(s)[1]); // 保留的是原条目对象（锚点 / 预算不动）
    expect(pendingOf(keep)[1]).toMatchObject({ at: queriedAt + 10, timeoutSecs: 900, tool: "Edit" });

    // 更晚的一次查询，快照也含 r-new：以快照为准，不重复
    const dedup = applyApprovalsSnapshot(
      s,
      M,
      [snap("r-old"), snap("r-new", { timeoutSecs: 890 })],
      queriedAt,
    );
    expect(ids(pendingOf(dedup))).toEqual(["r-old", "r-new"]);
    expect(pendingOf(dedup)[1]).toMatchObject({ timeoutSecs: 890, tool: "Bash" }); // 快照的剩余值，不是实时的预算

    // 边界：at 恰等于 queriedAt 不算「查询之后」（严格大于）
    expect(ids(pendingOf(applyApprovalsSnapshot(s, M, [], queriedAt + 10)))).toEqual([]);
    expect(ids(pendingOf(applyApprovalsSnapshot(s, M, [], queriedAt + 9)))).toEqual(["r-new"]);
  });

  it("④ 快照引用了尚不存在的 conv → 物化空会话（entries 空、无未读），审批派生能列出来", () => {
    // home-mac 空间此前完全不存在（从未收到该机器任何帧）
    expect(initialState().spaces[M]).toBeUndefined();
    const s = applyApprovalsSnapshot(
      initialState(),
      M,
      [snap("r1", { convId: "web:s-192d" }), snap("r2", { convId: "peer:win-a" })],
      now,
    );
    expect(orderOf(s, M)).toEqual([DEFAULT_CONV, "web:s-192d", "peer:win-a"]); // 物化追加在末尾，default 恒居首
    expect(spaceOf(s, M).convs["web:s-192d"]).toEqual({
      entries: [],
      lastPreview: "",
      lastAt: 0,
      unread: 0,
      pending: [
        {
          machine: M,
          convId: "web:s-192d",
          requestId: "r1",
          tool: "Bash",
          inputSummary: "cmd r1",
          at: now,
          timeoutSecs: 731,
        },
      ],
    });
    expect(listPending(s, M)).toEqual(["r1", "r2"]);
    // 条目的 machine / convId 归属正确（回传 approval_response 时原样带回）
    expect(pendingOf(s, "peer:win-a")[0]).toMatchObject({ machine: M, convId: "peer:win-a" });
    // 已存在的 conv 不重复物化、顺序不变
    expect(orderOf(s, M).filter((c) => c === DEFAULT_CONV)).toHaveLength(1);
  });

  it("⑤ queriedAt 为 undefined → 完全权威：原有 pending 全部替换（查询后才到的也不留）", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r1") });
    advance(10);
    s = run(s, { type: "frame", frame: realtimeReq("r2") });
    expect(ids(pendingOf(s))).toEqual(["r1", "r2"]);
    expect(ids(pendingOf(applyApprovalsSnapshot(s, M, [], undefined)))).toEqual([]);
    expect(ids(pendingOf(applyApprovalsSnapshot(s, M, [snap("s1")], undefined)))).toEqual(["s1"]);
    // 物化新 conv 的快照在 undefined 下同样只含快照条目
    const fresh = applyApprovalsSnapshot(s, M, [snap("s2", { convId: "web:new" })], undefined);
    expect(listPending(fresh, M)).toEqual(["s2"]);
  });

  it("⑥ 快照内容与现有完全相同 → 返回同一引用（两个触发点各来一份同样的快照不白渲染）", () => {
    const base = initialState();
    const first = applyApprovalsSnapshot(base, M, [snap("r1")], now);
    expect(first).not.toBe(base);
    const firstPending = pendingOf(first);
    // 紧接着的第二份同内容快照（E1 与 E2 的应答）：原引用，pending 数组与条目对象都不换
    advance(50);
    const second = applyApprovalsSnapshot(first, M, [snap("r1")], now);
    expect(second).toBe(first);
    expect(pendingOf(second)).toBe(firstPending);
    // 空快照 + 本来就空：原引用
    expect(applyApprovalsSnapshot(base, M, [], now)).toBe(base);
    // 该机器空间不存在 + 空快照：不凭空物化机器空间
    expect(applyApprovalsSnapshot(base, "never-seen", [], now)).toBe(base);
    // 含「查询后到达」的保留条目且快照相同：同样原引用
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r-old") });
    const queriedAt = now;
    advance(10);
    s = run(s, { type: "frame", frame: realtimeReq("r-new") });
    const once = applyApprovalsSnapshot(s, M, [snap("r-old")], queriedAt);
    expect(applyApprovalsSnapshot(once, M, [snap("r-old")], queriedAt)).toBe(once);
  });

  it("⑥b 内容有变化才换引用：只动受影响的 conv，其余 conv 对象与别的字段保持不变", () => {
    let s = applyApprovalsSnapshot(
      initialState(),
      M,
      [snap("r1"), snap("r2", { convId: "web:other" })],
      now,
    );
    const defaultConv = spaceOf(s, M).convs[DEFAULT_CONV];
    const otherConv = spaceOf(s, M).convs["web:other"];
    expect(ids(otherConv.pending)).toEqual(["r2"]);
    // 只移除 web:other 的 r2：default conv 对象不变，web:other 换新
    advance(10);
    const next = applyApprovalsSnapshot(s, M, [snap("r1")], now);
    expect(spaceOf(next, M).convs[DEFAULT_CONV]).toBe(defaultConv);
    expect(spaceOf(next, M).convs["web:other"]).not.toBe(otherConv);
    expect(pendingOf(next, "web:other")).toEqual([]);
    // 条目字段值不同（如剩余秒数变小）也算变化
    const tick = applyApprovalsSnapshot(
      s,
      M,
      [snap("r1", { timeoutSecs: 730 }), snap("r2", { convId: "web:other" })],
      now,
    );
    expect(tick).not.toBe(s);
    expect(pendingOf(tick)[0].timeoutSecs).toBe(730);
    // 别的机器空间对象不动
    expect(next.spaces[DEFAULT_MACHINE]).toBe(s.spaces[DEFAULT_MACHINE]);
  });

  it("⑦ 不产生 approval_note：被快照移除的审批不留终态记录（真实结局不可知，不编造）", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r1") });
    expect(spaceOf(s, M).convs[DEFAULT_CONV].entries).toHaveLength(0); // 实时到达本身不产生条目
    advance(10);
    const queriedAt = now;
    const removed = applyApprovalsSnapshot(s, M, [], queriedAt);
    expect(spaceOf(removed, M).convs[DEFAULT_CONV].entries).toHaveLength(0);
    expect(removed.seq).toBe(s.seq);
    // 套用 / 替换 / 物化同样不写消息流；未读不动
    const replaced = applyApprovalsSnapshot(
      s,
      M,
      [snap("r1"), snap("r2", { convId: "web:new" })],
      queriedAt,
    );
    expect(spaceOf(replaced, M).convs[DEFAULT_CONV].entries).toHaveLength(0);
    expect(spaceOf(replaced, M).convs["web:new"].entries).toHaveLength(0);
    expect(spaceOf(replaced, M).convs[DEFAULT_CONV].unread).toBe(0);
    // 既有终态记录路径不变：approval_resolved 照旧移出 pending 并留记录
    const resolved = run(replaced, { type: "frame", frame: resolvedFrame("r1") });
    expect(spaceOf(resolved, M).convs[DEFAULT_CONV].entries.map((e) => e.kind)).toEqual(["approval_note"]);
    expect(ids(pendingOf(resolved))).toEqual([]);
  });

  it("⑧ 只影响目标机器的空间：别的机器的 pending 与会话空间对象完全不动", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r-home") });
    // 同名 requestId 落在另一台机器（审批按机器空间隔离）
    s = run(s, { type: "frame", frame: realtimeReq("r-home", DEFAULT_CONV, "office-mac") });
    s = run(s, { type: "frame", frame: realtimeReq("r-office", DEFAULT_CONV, "office-mac") });
    advance(10);
    const officeSpace = s.spaces["office-mac"];
    const officePending = pendingOf(s, DEFAULT_CONV, "office-mac");
    const defaultSpace = s.spaces[DEFAULT_MACHINE];
    expect(ids(officePending)).toEqual(["r-home", "r-office"]);
    const after = applyApprovalsSnapshot(s, M, [snap("r-home", { timeoutSecs: 5 })], now);
    expect(after.spaces["office-mac"]).toBe(officeSpace); // 原空间对象
    expect(pendingOf(after, DEFAULT_CONV, "office-mac")).toBe(officePending); // 原 pending 数组
    expect(after.spaces[DEFAULT_MACHINE]).toBe(defaultSpace);
    expect(pendingOf(after)[0].timeoutSecs).toBe(5);
    // 其它 state 字段原样
    expect(after.seq).toBe(s.seq);
    expect(after.currentConv).toBe(s.currentConv);
    expect(after.workspaces).toBe(s.workspaces);
  });

  it("快照内重复 requestId（同 conv）只留首条；跨 conv 的不同条目各自保留", () => {
    const s = applyApprovalsSnapshot(
      initialState(),
      M,
      [snap("r1", { inputSummary: "first" }), snap("r1", { inputSummary: "second" }), snap("r2", { convId: "web:other" })],
      now,
    );
    expect(pendingOf(s)).toEqual([expect.objectContaining({ requestId: "r1", inputSummary: "first" })]);
    expect(ids(pendingOf(s, "web:other"))).toEqual(["r2"]);
  });

  it("该机器尚无会话空间时收到非空快照：连同 default 会话一并物化（ensureConv 语义）；default 机器空间不受影响", () => {
    const base = initialState();
    expect(base.spaces["work-mac"]).toBeUndefined();
    const s = applyApprovalsSnapshot(base, "work-mac", [snap("r1", { convId: "web:side" })], now);
    expect(orderOf(s, "work-mac")).toEqual([DEFAULT_CONV, "web:side"]);
    expect(pendingOf(s, DEFAULT_CONV, "work-mac")).toEqual([]);
    expect(pendingOf(s, "web:side", "work-mac")).toEqual([
      expect.objectContaining({ requestId: "r1", machine: "work-mac", convId: "web:side" }),
    ]);
    // default 机器空间是原对象（未动）；currentMachine 等不因物化而改变
    expect(s.spaces[DEFAULT_MACHINE]).toBe(base.spaces[DEFAULT_MACHINE]);
    expect(s.currentMachine).toBe(DEFAULT_MACHINE);
    expect(s.machinesKnown).toBe(false);
  });

  it("用户已点「允许」（approval_sent 本地移除）后迟到的旧快照会让条目回栏，随后的 approval_resolved 照旧移除（最终一致）", () => {
    let s = run(initialState(), { type: "frame", frame: realtimeReq("r1") });
    const queriedAt = now; // 查询早于用户操作：那份快照仍含 r1
    s = run(s, { type: "approval_sent", convId: DEFAULT_CONV, requestId: "r1", machine: M });
    expect(pendingOf(s)).toEqual([]);
    const late = applyApprovalsSnapshot(s, M, [snap("r1")], queriedAt);
    expect(ids(pendingOf(late))).toEqual(["r1"]); // 短暂回栏（快照是它生成时刻的权威）
    const resolved = run(late, { type: "frame", frame: resolvedFrame("r1") });
    expect(pendingOf(resolved)).toEqual([]);
    expect(spaceOf(resolved, M).convs[DEFAULT_CONV].entries.map((e) => e.kind)).toEqual(["approval_note"]);
    // 再来一份更新（已不含 r1）的快照也不会把这条终态记录抹掉
    const fresh = applyApprovalsSnapshot(resolved, M, [], now);
    expect(spaceOf(fresh, M).convs[DEFAULT_CONV].entries.map((e) => e.kind)).toEqual(["approval_note"]);
  });
});

// ---------------------------------------------------------------------------
// v1.43 本地窗口看板：windows_query / window_watch 出站形状（契约键序）、windows
// 快照帧入站逐字段校验（任一条目畸形整帧拒收）、reducer 整体替换 + 内容相同
// 保持引用、toggle_window_watch 乐观翻转，以及界面文案纯函数。
// ---------------------------------------------------------------------------

describe("v1.43 本地窗口：出站帧形状 + windows 帧解析校验", () => {
  /** 契约 §2 示例里的条目（可选键齐备的完整形态）。 */
  const win = (extra: Record<string, unknown> = {}) => ({
    name: "jormungandr-f8",
    status: "idle",
    cwd: "/Users/demo/work/api",
    startedAt: 1795114000000,
    statusUpdatedAt: 1795351568790,
    watched: false,
    ...extra,
  });
  const windowsFrame = (items: unknown, head = '"machine":"home-mac","hookInstalled":true,') =>
    `{"v":1,"type":"windows",${head}"items":${JSON.stringify(items)}}`;

  it("parseFrame 合法帧：契约示例原文逐字段保留（waitingFor 缺席即无键）；items 可为空数组", () => {
    const f = parseFrame(
      '{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":[' +
        '{"name":"jormungandr-f8","status":"idle","cwd":"/Users/demo/work/api","startedAt":1795114000000,"statusUpdatedAt":1795351568790,"watched":false},' +
        '{"name":"imagent-a7","status":"waiting","waitingFor":"dialog open","cwd":"/Users/demo/work/b","startedAt":1795115000000,"statusUpdatedAt":1795352000000,"watched":true}]}',
    );
    expect(f).toEqual({
      v: 1,
      type: "windows",
      machine: "home-mac",
      hookInstalled: true,
      items: [
        {
          name: "jormungandr-f8",
          status: "idle",
          cwd: "/Users/demo/work/api",
          startedAt: 1795114000000,
          statusUpdatedAt: 1795351568790,
          watched: false,
        },
        {
          name: "imagent-a7",
          status: "waiting",
          waitingFor: "dialog open",
          cwd: "/Users/demo/work/b",
          startedAt: 1795115000000,
          statusUpdatedAt: 1795352000000,
          watched: true,
        },
      ],
    });
    // 空数组 = peer 通道已启用但当前没有在线窗口（合法且有意义：键不省略）
    expect(parseFrame(windowsFrame([]))).toEqual({
      v: 1,
      type: "windows",
      machine: "home-mac",
      hookInstalled: true,
      items: [],
    });
    expect(parseFrame(windowsFrame([win()], '"machine":"home-mac","hookInstalled":false,'))).toEqual({
      v: 1,
      type: "windows",
      machine: "home-mac",
      hookInstalled: false,
      items: [win()],
    });
  });

  it("parseFrame 可选键缺席放行：waitingFor / startedAt / statusUpdatedAt 不带键的条目合法；四种 status 均合法；cwd 可为空串", () => {
    for (const status of ["busy", "idle", "waiting", "unknown"]) {
      expect(parseFrame(windowsFrame([{ name: "w", status, cwd: "", watched: true }])), status).toEqual({
        v: 1,
        type: "windows",
        machine: "home-mac",
        hookInstalled: true,
        items: [{ name: "w", status, cwd: "", watched: true }],
      });
    }
    // waiting 的 waitingFor 也可缺席（契约：为 None 时省略键）
    expect(parseFrame(windowsFrame([{ name: "w", status: "waiting", cwd: "/x", watched: false }]))).toBeTruthy();
    // waitingFor 可以是任意字符串（没见过的原因原样放行，界面按原文显示）
    expect(parseFrame(windowsFrame([win({ status: "waiting", waitingFor: "something new" })]))).toBeTruthy();
  });

  it("parseFrame 拒帧：status 越界（非四值 / 缺失 / 空串 / 非字符串）——好条目救不了坏条目", () => {
    for (const status of ["running", "Idle", "", undefined, 1, null, true]) {
      expect(parseFrame(windowsFrame([win(), win({ name: "bad", status })])), String(status)).toBeNull();
    }
  });

  it("parseFrame 拒帧：machine 缺失 / 空串 / 非字符串；hookInstalled 缺失 / 非布尔；items 缺失 / 非数组", () => {
    expect(parseFrame(windowsFrame([win()], '"hookInstalled":true,'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":"","hookInstalled":true,'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":7,"hookInstalled":true,'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":null,"hookInstalled":true,'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":"home-mac",'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":"home-mac","hookInstalled":"true",'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":"home-mac","hookInstalled":1,'))).toBeNull();
    expect(parseFrame(windowsFrame([win()], '"machine":"home-mac","hookInstalled":null,'))).toBeNull();
    expect(parseFrame('{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":{}}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":null}')).toBeNull();
    expect(parseFrame('{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":"w"}')).toBeNull();
  });

  it("parseFrame 拒帧：任一条目畸形即整帧拒收（缺字段 / 类型不符 / 非对象）", () => {
    const badItems: unknown[] = [
      "jormungandr-f8",
      null,
      [],
      win({ name: undefined }),
      win({ name: "" }),
      win({ name: 7 }),
      win({ waitingFor: 3 }),
      win({ waitingFor: null }),
      win({ cwd: undefined }),
      win({ cwd: null }),
      win({ cwd: 5 }),
      win({ startedAt: "1795114000000" }),
      win({ startedAt: null }),
      win({ statusUpdatedAt: "1795351568790" }),
      win({ statusUpdatedAt: null }),
      win({ watched: undefined }),
      win({ watched: "false" }),
      win({ watched: 0 }),
      win({ watched: null }),
    ];
    for (const bad of badItems) {
      expect(parseFrame(windowsFrame([win(), bad])), JSON.stringify(bad)).toBeNull();
    }
  });

  it("parseFrame 拒帧：startedAt / statusUpdatedAt 须为有限数字（1e999 解析成 Infinity）；有限值（含 0 与小数）放行", () => {
    const item = (patch: string) =>
      `{"v":1,"type":"windows","machine":"home-mac","hookInstalled":true,"items":[{"name":"w","status":"idle","cwd":"/x"${patch},"watched":false}]}`;
    expect(parseFrame(item(',"startedAt":1e999'))).toBeNull();
    expect(parseFrame(item(',"statusUpdatedAt":1e999'))).toBeNull();
    expect(parseFrame(item(',"startedAt":0,"statusUpdatedAt":1.5'))).toBeTruthy();
  });
});

describe("v1.43 本地窗口：reducer（windows 整体替换 + 内容相同保持引用；toggle_window_watch）", () => {
  const M = "home-mac";
  const IDLE: WindowInfo = {
    name: "jormungandr-f8",
    status: "idle",
    cwd: "/Users/demo/work/api",
    startedAt: 1795114000000,
    statusUpdatedAt: 1795351568790,
    watched: false,
  };
  const WAIT: WindowInfo = {
    name: "imagent-a7",
    status: "waiting",
    waitingFor: "dialog open",
    cwd: "/Users/demo/work/b",
    startedAt: 1795115000000,
    statusUpdatedAt: 1795352000000,
    watched: true,
  };
  const BUSY: WindowInfo = { name: "runner", status: "busy", cwd: "/tmp", watched: false };
  /** 入站 windows 帧（经 parseFrame，与真实路径一致）。 */
  const windowsF = (items: unknown[], machine = M, hookInstalled = true): InboundFrame =>
    frame(
      `{"v":1,"type":"windows","machine":"${machine}","hookInstalled":${hookInstalled},"items":${JSON.stringify(items)}}`,
    );
  const snapOf = (s: AppState, machine = M): MachineWindows | undefined => s.windows[machine];
  const watchedOf = (s: AppState, name: string, machine = M): boolean | undefined =>
    snapOf(s, machine)?.items.find((w) => w.name === name)?.watched;

  it("initialState.windows 为 {}：没有某机器的键 = 从未收到过它的快照（旧网关 / 未启用 peer 通道）", () => {
    expect(initialState().windows).toEqual({});
  });

  it("整体替换：后一帧的条目与 hookInstalled 以最新一帧为准（不与旧条目合并）", () => {
    const s1 = run(initialState(), { type: "frame", frame: windowsF([IDLE, WAIT]) });
    expect(snapOf(s1)).toEqual({ hookInstalled: true, items: [IDLE, WAIT] });
    const s2 = run(s1, { type: "frame", frame: windowsF([BUSY], M, false) });
    expect(snapOf(s2)).toEqual({ hookInstalled: false, items: [BUSY] });
    // 清空 = 该机器此刻已没有在线窗口（键仍在，items 为空数组）
    const s3 = run(s2, { type: "frame", frame: windowsF([]) });
    expect(snapOf(s3)).toEqual({ hookInstalled: true, items: [] });
  });

  it("内容相同返回原 state 引用：应答查询与主动推送给来同一份快照时不触发重渲染", () => {
    const s1 = run(initialState(), { type: "frame", frame: windowsF([IDLE, WAIT]) });
    // 再解析一次得到的是「新对象、同内容」的帧
    expect(run(s1, { type: "frame", frame: windowsF([IDLE, WAIT]) })).toBe(s1);
    const s2 = run(s1, { type: "frame", frame: windowsF([IDLE, WAIT], M, false) });
    expect(run(s2, { type: "frame", frame: windowsF([IDLE, WAIT], M, false) })).toBe(s2);
    // 可选键缺席的条目同样按内容比较
    const s3 = run(s2, { type: "frame", frame: windowsF([BUSY]) });
    expect(run(s3, { type: "frame", frame: windowsF([BUSY]) })).toBe(s3);
    // 空快照重复到达亦幂等
    const s4 = run(s3, { type: "frame", frame: windowsF([]) });
    expect(run(s4, { type: "frame", frame: windowsF([]) })).toBe(s4);
  });

  it("任一字段不同即新引用：逐字段探测七个条目字段 + hookInstalled + 条目数 / 顺序 / 可选键有无", () => {
    const base = run(initialState(), { type: "frame", frame: windowsF([IDLE, WAIT]) });
    const variants: unknown[][] = [
      [{ ...IDLE, name: "other" }, WAIT],
      [{ ...IDLE, status: "busy" }, WAIT],
      [{ ...IDLE, status: "waiting", waitingFor: "dialog open" }, WAIT],
      [IDLE, { ...WAIT, waitingFor: "input needed" }],
      [{ ...IDLE, cwd: "/elsewhere" }, WAIT],
      [{ ...IDLE, startedAt: 1 }, WAIT],
      [{ ...IDLE, statusUpdatedAt: 1795351568791 }, WAIT],
      [{ ...IDLE, watched: true }, WAIT],
      [WAIT, IDLE],
      [IDLE],
      [IDLE, WAIT, BUSY],
    ];
    for (const items of variants) {
      expect(run(base, { type: "frame", frame: windowsF(items) }), JSON.stringify(items)).not.toBe(base);
    }
    // 可选键「有 → 无」也是变化（等待原因被清掉）：单独构造，免得 undefined 被 JSON 序列化吞掉
    const noReason: WindowInfo = {
      name: WAIT.name,
      status: WAIT.status,
      cwd: WAIT.cwd,
      watched: WAIT.watched,
    };
    const withReason = run(initialState(), { type: "frame", frame: windowsF([WAIT]) });
    expect(run(withReason, { type: "frame", frame: windowsF([noReason]) })).not.toBe(withReason);
    // hookInstalled 单独变化
    expect(run(base, { type: "frame", frame: windowsF([IDLE, WAIT], M, false) })).not.toBe(base);
  });

  it("两台机器互不影响：各自的快照独立落表，一台的后续快照不动另一台的条目对象", () => {
    let s = run(initialState(), { type: "frame", frame: windowsF([IDLE], M) });
    s = run(s, { type: "frame", frame: windowsF([BUSY], "work-mac", false) });
    expect(snapOf(s, M)).toEqual({ hookInstalled: true, items: [IDLE] });
    expect(snapOf(s, "work-mac")).toEqual({ hookInstalled: false, items: [BUSY] });
    const homeBefore = snapOf(s, M);
    s = run(s, { type: "frame", frame: windowsF([], "work-mac", false) });
    expect(snapOf(s, M)).toBe(homeBefore);
    expect(snapOf(s, "work-mac")?.items).toEqual([]);
  });

  it("toggle_window_watch：翻转该机器快照里该窗口的 watched，其余条目与 hookInstalled 不动", () => {
    const s0 = run(initialState(), { type: "frame", frame: windowsF([IDLE, WAIT], M, false) });
    const s1 = run(s0, { type: "toggle_window_watch", machine: M, name: IDLE.name });
    expect(watchedOf(s1, IDLE.name)).toBe(true);
    expect(snapOf(s1, M)).toEqual({ hookInstalled: false, items: [{ ...IDLE, watched: true }, WAIT] });
    const s2 = run(s1, { type: "toggle_window_watch", machine: M, name: WAIT.name });
    expect(watchedOf(s2, WAIT.name)).toBe(false);
    // 每个条目各翻两次（假 → 真 → 假 / 真 → 假 → 真）：内容回到原样
    const s3 = run(s2, { type: "toggle_window_watch", machine: M, name: IDLE.name });
    expect(watchedOf(s3, IDLE.name)).toBe(false);
    expect(watchedOf(s3, WAIT.name)).toBe(false); // WAIT 只翻了一次，此刻仍是假
    const s4 = run(s3, { type: "toggle_window_watch", machine: M, name: WAIT.name });
    expect(snapOf(s4, M)).toEqual(snapOf(s0, M));
  });

  it("toggle_window_watch：未知窗口 / 未知机器 no-op（返回原 state 引用）", () => {
    const s0 = run(initialState(), { type: "frame", frame: windowsF([IDLE]) });
    expect(run(s0, { type: "toggle_window_watch", machine: M, name: "no-such-window" })).toBe(s0);
    expect(run(s0, { type: "toggle_window_watch", machine: "work-mac", name: IDLE.name })).toBe(s0);
  });

  it("toggle_window_watch：不可变——原 state 及其条目对象未被改写（冻结对象上执行，任何写入即抛错）", () => {
    // 运行时冻结、类型上仍按可变声明（双重断言）：reducer 若改写入参，严格模式下
    // 直接抛错；而类型检查照常覆盖 reducer 的签名。
    const frozenItems = Object.freeze([Object.freeze(IDLE), Object.freeze(WAIT)]) as unknown as WindowInfo[];
    const frozenSnap: MachineWindows = Object.freeze({ hookInstalled: true, items: frozenItems });
    const s0: AppState = {
      ...initialState(),
      windows: Object.freeze({ [M]: frozenSnap }),
    };
    const s1 = run(s0, { type: "toggle_window_watch", machine: M, name: IDLE.name });
    expect(watchedOf(s0, IDLE.name)).toBe(false);
    expect(watchedOf(s1, IDLE.name)).toBe(true);
    expect(snapOf(s1, M)).not.toBe(snapOf(s0, M));
    expect(snapOf(s1, M)?.items).not.toBe(snapOf(s0, M)?.items);
    // 没被点的那一行保持同一引用（不白建对象）
    expect(snapOf(s1, M)?.items[1]).toBe(WAIT);
  });
});

describe("v1.43 本地窗口纯函数（界面文案）", () => {
  it("peerConvOf：窗口名 → `peer:<窗口名>`（v1.41 既有会话形态；convDisplayName 可剥回窗口名）", () => {
    expect(peerConvOf("jormungandr-f8")).toBe("peer:jormungandr-f8");
    expect(convDisplayName(peerConvOf("imagent-a7"))).toBe("imagent-a7");
  });

  it("windowStatusText：四值映射（Record 类型穷举——漏一个值即类型错误）", () => {
    const all: Record<WindowStatus, string> = {
      busy: windowStatusText("busy"),
      idle: windowStatusText("idle"),
      waiting: windowStatusText("waiting"),
      unknown: windowStatusText("unknown"),
    };
    expect(all).toEqual({ busy: "运行中", idle: "空闲", waiting: "等待确认", unknown: "未知" });
  });

  it("waitingForText：已观察到的四种等待原因 → 人话；没见过的原因原样返回", () => {
    expect(waitingForText("dialog open")).toBe("等你确认");
    expect(waitingForText("input needed")).toBe("等你输入");
    expect(waitingForText("sandbox request")).toBe("等沙箱授权");
    expect(waitingForText("goal proposal")).toBe("等你定目标");
    expect(waitingForText("mcp auth")).toBe("mcp auth");
    expect(waitingForText("")).toBe("");
  });

  it("relativeTime：边界——59 秒 / 60 秒 / 59 分 / 60 分 / 23 小时 / 24 小时", () => {
    const now = 1_795_000_000_000;
    const ago = (ms: number): string => relativeTime(now - ms, now);
    expect(ago(0)).toBe("刚刚");
    expect(ago(59_000)).toBe("刚刚");
    expect(ago(60_000)).toBe("1 分钟前");
    expect(ago(119_999)).toBe("1 分钟前");
    expect(ago(3_540_000)).toBe("59 分钟前");
    expect(ago(3_600_000)).toBe("1 小时前");
    expect(ago(23 * 3_600_000)).toBe("23 小时前");
    expect(ago(86_399_999)).toBe("23 小时前");
    expect(ago(86_400_000)).toBe("1 天前");
    expect(ago(3 * 86_400_000)).toBe("3 天前");
  });

  it("relativeTime：缺失返回空串；时间基准取自参数（不偷读系统时钟）；时钟偏差下的未来时间戳按「刚刚」", () => {
    const now = 1_795_000_000_000;
    expect(relativeTime(undefined, now)).toBe("");
    expect(relativeTime(now + 5_000, now)).toBe("刚刚");
    // 同一个 at，now 推进一小时后文案随之变化
    expect(relativeTime(now - 60_000, now)).toBe("1 分钟前");
    expect(relativeTime(now - 60_000, now + 3_600_000)).toBe("1 小时前");
  });
});

// ---------------------------------------------------------------------------
// v1.44 送达回执 + 网关版本：出站帧形状 / ack 与 gateway_info 解析校验 /
// newClientId；reducer（send_echo 的键与 ack、ack、ack_timeout、gateway_info 及
// 其作废）；超时判定纯函数 timedOutAcks
// ---------------------------------------------------------------------------

describe("v1.44 送达回执 + 网关版本：出站形状 + 解析校验 + newClientId", () => {
  /** 契约 §2 示例原文（两种状态）。 */
  const ACK_RECEIVED =
    '{"v":1,"type":"ack","machine":"home-mac","clientId":"m_1a2b3c4d","convId":"web:s-1","status":"received"}';
  const ACK_REJECTED =
    '{"v":1,"type":"ack","machine":"home-mac","clientId":"m_1a2b3c4d","convId":"web:s-1","status":"rejected","reason":"not_allowed","message":"发送者未获授权，消息未被处理"}';
  const INFO = '{"v":1,"type":"gateway_info","machine":"home-mac","version":"1.44.0","caps":["acks"]}';
  /** gateway_info 帧：caps 段整体定制（含前导逗号；空串 = 缺该键）。 */
  const infoRaw = (caps: string) =>
    `{"v":1,"type":"gateway_info","machine":"home-mac","version":"1.44.0"${caps}}`;
  /** ack 帧：tail 接在 status 之后（reason / message 的各种组合）。 */
  const ackRaw = (status: string, tail = "") =>
    `{"v":1,"type":"ack","machine":"m","clientId":"c","convId":"web:default","status":"${status}"${tail}}`;

  it("parseFrame：ack 两种合法形态（契约示例原文）逐字段还原", () => {
    expect(parseFrame(ACK_RECEIVED)).toEqual({
      v: 1,
      type: "ack",
      machine: "home-mac",
      clientId: "m_1a2b3c4d",
      convId: "web:s-1",
      status: "received",
    });
    expect(parseFrame(ACK_REJECTED)).toEqual({
      v: 1,
      type: "ack",
      machine: "home-mac",
      clientId: "m_1a2b3c4d",
      convId: "web:s-1",
      status: "rejected",
      reason: "not_allowed",
      message: "发送者未获授权，消息未被处理",
    });
  });

  it("parseFrame：rejected 的 reason 四值均合法", () => {
    for (const reason of ["not_allowed", "queue_full", "peer_unreachable", "malformed"]) {
      const raw = ackRaw("rejected", `,"reason":"${reason}","message":"x"`);
      expect(parseFrame(raw), reason).toMatchObject({ type: "ack", status: "rejected", reason });
    }
  });

  it("parseFrame：rejected 缺 reason / 缺 message / reason 越界 / 类型不符 → 整帧拒收", () => {
    for (const bad of [
      ackRaw("rejected"), // 两个都缺
      ackRaw("rejected", ',"message":"x"'), // 缺 reason
      ackRaw("rejected", ',"reason":"queue_full"'), // 缺 message
      ackRaw("rejected", ',"reason":"unknown_reason","message":"x"'), // reason 越界
      ackRaw("rejected", ',"reason":"NotAllowed","message":"x"'), // 大小写敏感
      ackRaw("rejected", ',"reason":"queue_full","message":1'), // message 非字符串
      ackRaw("rejected", ',"reason":1,"message":"x"'), // reason 非字符串
    ]) {
      expect(parseFrame(bad), bad).toBeNull();
    }
  });

  it("parseFrame：received 不得带 reason / message；status 越界或缺席 → 拒收", () => {
    for (const bad of [
      ackRaw("received", ',"reason":"not_allowed","message":"x"'),
      ackRaw("received", ',"message":"x"'),
      ackRaw("received", ',"reason":"not_allowed"'),
      ackRaw("sent"),
      ackRaw("pending"),
      ackRaw("Received"),
      ackRaw(""),
      '{"v":1,"type":"ack","machine":"m","clientId":"c","convId":"web:default"}', // 缺 status
      '{"v":1,"type":"ack","machine":"m","clientId":"c","convId":"web:default","status":1}',
    ]) {
      expect(parseFrame(bad), bad).toBeNull();
    }
  });

  it("parseFrame：ack 的 machine / clientId / convId 须为非空字符串（缺席 / 空串 / 非字符串均拒收）", () => {
    const good = JSON.parse(ACK_RECEIVED) as Record<string, unknown>;
    for (const key of ["machine", "clientId", "convId"]) {
      expect(parseFrame(JSON.stringify({ ...good, [key]: "" })), `${key} 空串`).toBeNull();
      expect(parseFrame(JSON.stringify({ ...good, [key]: undefined })), `${key} 缺席`).toBeNull();
      expect(parseFrame(JSON.stringify({ ...good, [key]: 1 })), `${key} 非字符串`).toBeNull();
    }
    expect(parseFrame(JSON.stringify(good))).toMatchObject({ type: "ack" }); // 对照：原样合法
  });

  it("parseFrame：gateway_info 合法（契约示例原文；空 caps 亦合法）；caps 非数组 / 含非字符串 / 缺键 / machine 或 version 为空 → 拒收", () => {
    expect(parseFrame(INFO)).toEqual({ v: 1, type: "gateway_info", machine: "home-mac", version: "1.44.0", caps: ["acks"] });
    // 空数组 = 该网关没有任何可选能力（合法，只是不支持回执）
    expect(parseFrame(infoRaw(',"caps":[]'))).toMatchObject({ type: "gateway_info", caps: [] });
    for (const bad of [
      infoRaw(""), // 缺 caps
      infoRaw(',"caps":"acks"'), // 非数组
      infoRaw(',"caps":{}'),
      infoRaw(',"caps":["acks",1]'), // 含非字符串
      infoRaw(',"caps":[null]'),
      '{"v":1,"type":"gateway_info","machine":"","version":"1.44.0","caps":[]}',
      '{"v":1,"type":"gateway_info","machine":"home-mac","version":"","caps":[]}',
      '{"v":1,"type":"gateway_info","machine":"home-mac","caps":[]}', // 缺 version
    ]) {
      expect(parseFrame(bad), bad).toBeNull();
    }
  });

  it("encodeFrame：gateway_info_query / e2e_close / 带 clientId 的 message 与契约示例逐字节一致（键序）", () => {
    expect(encodeFrame({ v: 1, type: "gateway_info_query", machine: "home-mac" })).toBe(
      '{"v":1,"type":"gateway_info_query","machine":"home-mac"}',
    );
    // e2e_close 内帧不带 machine（路由靠 enc 信封）
    expect(encodeFrame({ v: 1, type: "e2e_close" })).toBe('{"v":1,"type":"e2e_close"}');
    expect(
      encodeFrame({ v: 1, type: "message", convId: "web:s-1", text: "你好", machine: "home-mac", clientId: "m_1a2b3c4d" }),
    ).toBe('{"v":1,"type":"message","convId":"web:s-1","text":"你好","machine":"home-mac","clientId":"m_1a2b3c4d"}');
    // 带图：clientId 排在 media 之后；不带 clientId 时该键不出现（与升级前字节一致）
    const media = { name: "a.png", mime: "image/png", b64: "AA==" };
    expect(encodeFrame({ v: 1, type: "message", convId: "web:s-1", text: "你好", machine: "home-mac", media, clientId: "m_1a2b3c4d" })).toBe(
      '{"v":1,"type":"message","convId":"web:s-1","text":"你好","machine":"home-mac","media":{"name":"a.png","mime":"image/png","b64":"AA=="},"clientId":"m_1a2b3c4d"}',
    );
    expect(encodeFrame({ v: 1, type: "message", convId: "web:s-1", text: "你好", machine: "home-mac" })).toBe(
      '{"v":1,"type":"message","convId":"web:s-1","text":"你好","machine":"home-mac"}',
    );
  });

  it("newClientId：形状 ^m_[0-9a-z]+_[0-9a-f]{8}$、长度 ≤64、字符集合规；连生成 200 个互不相同", () => {
    const ids = Array.from({ length: 200 }, () => newClientId());
    for (const id of ids) {
      expect(id).toMatch(/^m_[0-9a-z]+_[0-9a-f]{8}$/);
      expect(id.length).toBeLessThanOrEqual(64);
      expect(id).toMatch(/^[A-Za-z0-9_-]+$/);
    }
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("newClientId：同一毫秒内连发也不撞（时间戳段取 36 进制）；crypto 不可用时退回 Math.random，形状不变", () => {
    let now = 1_795_000_000_000;
    vi.spyOn(Date, "now").mockImplementation(() => now);
    try {
      const ids = Array.from({ length: 50 }, () => newClientId());
      expect(new Set(ids).size).toBe(50);
      const stamp = now.toString(36);
      for (const id of ids) {
        expect(id.startsWith(`m_${stamp}_`)).toBe(true);
      }
      vi.stubGlobal("crypto", {}); // 没有 getRandomValues 的环境
      for (let i = 0; i < 50; i++) {
        expect(newClientId()).toMatch(/^m_[0-9a-z]+_[0-9a-f]{8}$/);
      }
    } finally {
      vi.unstubAllGlobals();
      vi.restoreAllMocks();
    }
  });
});

describe("v1.44 送达回执 + 网关版本：reducer", () => {
  const M = "home-mac";
  const OTHER = "work-mac";
  const CONV = DEFAULT_CONV;
  const PEER = "peer:win-a";
  type UserEntry = Extract<StreamEntry, { kind: "user" }>;
  const infoF = (machine: string, version = "1.44.0", caps: string[] = ["acks"]): GatewayInfoFrame => ({
    v: 1,
    type: "gateway_info",
    machine,
    version,
    caps,
  });
  type AckExtra = Partial<Pick<AckFrame, "machine" | "convId" | "reason" | "message">>;
  const ackF = (clientId: string, status: "received" | "rejected", extra: AckExtra = {}): AckFrame => ({
    v: 1,
    type: "ack",
    machine: M,
    clientId,
    convId: CONV,
    status,
    ...extra,
  });
  const REJECT_QUEUE_FULL: AckExtra = { reason: "queue_full", message: "排队消息过多，这条被丢弃" };
  const echo = (clientId: string, text = "你好", convId = CONV, machine = M): Action => ({
    type: "send_echo",
    convId,
    text,
    clientId,
    machine,
  });
  const timeout = (clientId: string, convId = CONV, machine = M): Action => ({
    type: "ack_timeout",
    machine,
    convId,
    clientId,
  });
  const echoOf = (s: AppState, clientId: string, convId = CONV, machine = M): UserEntry | undefined =>
    spaceOf(s, machine).convs[convId]?.entries.find(
      (e): e is UserEntry => e.kind === "user" && e.clientId === clientId,
    );
  /** 机器 M 已报告支持回执的初始状态。 */
  const acked = (): AppState => run(initialState(), { type: "frame", frame: infoF(M) });

  // ---- send_echo：键与 ack 字段 ----

  it("send_echo：条目键 user:<clientId>、恒带 clientId；机器支持回执时带 ack:\"pending\"，不支持时不带 ack 字段", () => {
    const plain = run(initialState(), echo("c1"));
    const e = echoOf(plain, "c1");
    expect(e).toMatchObject({ kind: "user", key: "user:c1", text: "你好", clientId: "c1" });
    expect(typeof e?.at).toBe("number");
    expect(e).not.toHaveProperty("ack");
    expect(e).not.toHaveProperty("self");
    const withAck = run(acked(), echo("c2"));
    expect(echoOf(withAck, "c2")).toMatchObject({ key: "user:c2", clientId: "c2", ack: "pending" });
  });

  it("send_echo：回执能力按机器判定——只有 caps 含 acks 的机器才 pending；caps 不含 acks 等同不支持", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: infoF(M) },
      { type: "frame", frame: infoF(OTHER, "1.43.0", []) },
      echo("c1", "你好", CONV, M),
      echo("c2", "你好", CONV, OTHER),
      echo("c3", "你好", CONV, "no-such-machine"),
    );
    expect(echoOf(s, "c1", CONV, M)?.ack).toBe("pending");
    expect(echoOf(s, "c2", CONV, OTHER)).toBeDefined();
    expect(echoOf(s, "c2", CONV, OTHER)).not.toHaveProperty("ack");
    expect(echoOf(s, "c3", CONV, "no-such-machine")).toBeDefined();
    expect(echoOf(s, "c3", CONV, "no-such-machine")).not.toHaveProperty("ack");
    expect(machineSupportsAcks(s, M)).toBe(true);
    expect(machineSupportsAcks(s, OTHER)).toBe(false);
    expect(machineSupportsAcks(s, "no-such-machine")).toBe(false);
  });

  it("send_echo 带 image：条目落 image:true；缺省不落键", () => {
    const s = run(
      acked(),
      { type: "send_echo", convId: CONV, text: "[图片]", clientId: "c1", machine: M, image: true },
      echo("c2"),
    );
    expect(echoOf(s, "c1")).toMatchObject({ key: "user:c1", image: true, ack: "pending" });
    expect(echoOf(s, "c2")).not.toHaveProperty("image");
  });

  it("键形态变化不动 text 条目的序号键：send_echo 照常推进 seq", () => {
    // 文本帧带 machine 落到同一机器空间（多机模式网关出站帧恒带）
    const reply = frame(`{"v":1,"type":"text","convId":"${CONV}","text":"回复","machine":"${M}"}`);
    const s = run(initialState(), echo("c1"), { type: "frame", frame: reply }, echo("c2"));
    expect(spaceOf(s, M).convs[CONV].entries.map((e) => e.key)).toEqual(["user:c1", "text:2", "user:c2"]);
  });

  // ---- ack 帧 ----

  it("ack received：pending → sent，不带 ackReason", () => {
    const s = run(acked(), echo("c1"), { type: "frame", frame: ackF("c1", "received") });
    expect(echoOf(s, "c1")).toMatchObject({ ack: "sent" });
    expect(echoOf(s, "c1")).not.toHaveProperty("ackReason");
  });

  it("ack rejected：→ failed，ackReason = 网关给的中文 message（不是原因码）", () => {
    const s = run(acked(), echo("c1"), { type: "frame", frame: ackF("c1", "rejected", REJECT_QUEUE_FULL) });
    expect(echoOf(s, "c1")).toMatchObject({ ack: "failed", ackReason: "排队消息过多，这条被丢弃" });
  });

  it("ack 完整往返：received（→ sent）之后 rejected 到达 → failed，原因取 message", () => {
    const s = run(
      acked(),
      echo("c1"),
      { type: "frame", frame: ackF("c1", "received") },
      { type: "frame", frame: ackF("c1", "rejected", { reason: "not_allowed", message: "发送者未获授权，消息未被处理" }) },
    );
    expect(echoOf(s, "c1")).toMatchObject({ ack: "failed", ackReason: "发送者未获授权，消息未被处理" });
  });

  it("迟到的 received 把 unconfirmed 翻回 sent（超时判定不是终局）", () => {
    const s = run(
      acked(),
      echo("c1"),
      timeout("c1"),
      { type: "frame", frame: ackF("c1", "received") },
    );
    expect(echoOf(s, "c1")?.ack).toBe("sent");
  });

  it("未知 clientId / 别的会话 / 别的机器的回执 → 原 state 引用（别的设备发的消息本机没有条目）", () => {
    const s = run(acked(), echo("c1"));
    expect(run(s, { type: "frame", frame: ackF("nope", "received") })).toBe(s);
    expect(run(s, { type: "frame", frame: ackF("c1", "received", { convId: "web:other" }) })).toBe(s);
    expect(run(s, { type: "frame", frame: ackF("c1", "received", { machine: OTHER }) })).toBe(s);
    // 该机器根本没有这个会话
    expect(run(s, { type: "frame", frame: ackF("c1", "rejected", REJECT_QUEUE_FULL) })).not.toBe(s);
  });

  it("已是 sent 再收到 received → 不变（原引用）；内容相同的重复 rejected → 原引用", () => {
    const sent = run(acked(), echo("c1"), { type: "frame", frame: ackF("c1", "received") });
    expect(run(sent, { type: "frame", frame: ackF("c1", "received") })).toBe(sent);
    const failed = run(acked(), echo("c1"), { type: "frame", frame: ackF("c1", "rejected", REJECT_QUEUE_FULL) });
    expect(run(failed, { type: "frame", frame: ackF("c1", "rejected", REJECT_QUEUE_FULL) })).toBe(failed);
  });

  it("只认本机回显：网关镜像补回的 user 条目（无 clientId）不受任何回执影响", () => {
    const s = run(
      acked(),
      {
        type: "frame",
        frame: frame(`{"v":1,"type":"text","convId":"${CONV}","text":"镜像的那句","at":1,"self":true,"machine":"${M}"}`),
      },
    );
    expect(spaceOf(s, M).convs[CONV].entries[0]).toMatchObject({ kind: "user", self: true });
    expect(run(s, { type: "frame", frame: ackF("nope", "received") })).toBe(s);
  });

  it("IndexedDB 恢复的条目（键带 h: 前缀）照样按 clientId 关联：迟到的 received 落到它身上", () => {
    const live = run(acked(), echo("c1"));
    const entry = echoOf(live, "c1");
    if (!entry) {
      throw new Error("夹具：未找到回显");
    }
    const space = spaceOf(live, M);
    const restored: AppState = {
      ...live,
      spaces: {
        ...live.spaces,
        [M]: {
          ...space,
          convs: {
            ...space.convs,
            [CONV]: {
              ...space.convs[CONV],
              entries: [{ ...entry, key: `h:${entry.key}`, ack: "unconfirmed" }],
            },
          },
        },
      },
    };
    const s = run(restored, { type: "frame", frame: ackF("c1", "received") });
    expect(spaceOf(s, M).convs[CONV].entries).toHaveLength(1);
    expect(spaceOf(s, M).convs[CONV].entries[0]).toMatchObject({ key: "h:user:c1", ack: "sent" });
  });

  it("发出时能力尚不可知（条目无 ack）的回显：回执到了照常落状态——received → sent、rejected → failed", () => {
    const s = run(
      initialState(),
      echo("c1"),
      echo("c2"),
      { type: "frame", frame: ackF("c1", "received") },
      { type: "frame", frame: ackF("c2", "rejected", REJECT_QUEUE_FULL) },
    );
    expect(echoOf(s, "c1")).toMatchObject({ ack: "sent" });
    expect(echoOf(s, "c2")).toMatchObject({ ack: "failed", ackReason: "排队消息过多，这条被丢弃" });
  });

  it("ack / ack_timeout 只改那一条回显的 ack 字段：未读、概要、title、seq 与改写前的条目对象均不动", () => {
    const base = run(acked(), echo("c1", "第一条"), echo("c2", "第二条"));
    const before = convsOf(base)[CONV];
    const first = echoOf(base, "c1");
    const s = run(base, { type: "frame", frame: ackF("c1", "received") }, timeout("c2"));
    const conv = convsOf(s)[CONV];
    expect(echoOf(s, "c1")?.ack).toBe("sent");
    expect(echoOf(s, "c2")?.ack).toBe("unconfirmed");
    expect(conv?.unread).toBe(before?.unread);
    expect(conv?.lastPreview).toBe(before?.lastPreview);
    expect(conv?.lastAt).toBe(before?.lastAt);
    expect(conv?.title).toBe(before?.title);
    expect(s.seq).toBe(base.seq);
    expect(first).toMatchObject({ clientId: "c1", ack: "pending" }); // 旧条目对象未被改写
    expect(echoOf(s, "c1")).not.toBe(first);
  });

  // ---- ack_timeout ----

  it("ack_timeout：pending → unconfirmed；只动 pending——sent / failed / unconfirmed / 无 ack 的回显原 state 引用", () => {
    let s = run(initialState(), echo("c5")); // 能力未知时发出：无 ack
    s = run(
      s,
      { type: "frame", frame: infoF(M) },
      echo("c1"),
      echo("c2"),
      echo("c3"),
      echo("c4"),
      { type: "frame", frame: ackF("c1", "received") },
      { type: "frame", frame: ackF("c2", "rejected", REJECT_QUEUE_FULL) },
      timeout("c3"),
    );
    expect(echoOf(s, "c1")?.ack).toBe("sent");
    expect(echoOf(s, "c2")?.ack).toBe("failed");
    expect(echoOf(s, "c3")?.ack).toBe("unconfirmed");
    expect(echoOf(s, "c4")?.ack).toBe("pending");
    expect(echoOf(s, "c5")).not.toHaveProperty("ack");
    for (const id of ["c1", "c2", "c3", "c5"]) {
      expect(run(s, timeout(id)), id).toBe(s);
    }
    const after = run(s, timeout("c4"));
    expect(echoOf(after, "c4")?.ack).toBe("unconfirmed");
    expect(after).not.toBe(s);
    // 再超时一次：已是 unconfirmed，原引用
    expect(run(after, timeout("c4"))).toBe(after);
  });

  it("ack_timeout：未知 clientId / 别的会话 / 别的机器 → 原 state 引用", () => {
    const s = run(acked(), echo("c1"));
    expect(run(s, timeout("nope"))).toBe(s);
    expect(run(s, timeout("c1", "web:other"))).toBe(s);
    expect(run(s, timeout("c1", CONV, OTHER))).toBe(s);
  });

  // ---- gateway_info 与能力作废 ----

  it("gateway_info：按机器整体替换；内容相同（哪怕 caps 是另一个数组实例）返回原引用；各机器条目互不牵连", () => {
    const s1 = run(initialState(), { type: "frame", frame: infoF(M) });
    expect(s1.gatewayInfo).toEqual({ [M]: { version: "1.44.0", caps: ["acks"] } });
    expect(run(s1, { type: "frame", frame: infoF(M) })).toBe(s1);
    const s2 = run(s1, { type: "frame", frame: infoF(M, "1.44.1") });
    expect(s2.gatewayInfo).toEqual({ [M]: { version: "1.44.1", caps: ["acks"] } });
    const s3 = run(s2, { type: "frame", frame: infoF(M, "1.44.1", []) });
    expect(s3.gatewayInfo).toEqual({ [M]: { version: "1.44.1", caps: [] } });
    expect(run(s3, { type: "frame", frame: infoF(M, "1.44.1", []) })).toBe(s3);
    const home = s3.gatewayInfo[M];
    const s4 = run(s3, { type: "frame", frame: infoF(OTHER) });
    expect(Object.keys(s4.gatewayInfo).sort()).toEqual([M, OTHER]);
    expect(s4.gatewayInfo[M]).toBe(home);
    // 不可变：先前 state 的记录原样
    expect(s1.gatewayInfo[M]).toEqual({ version: "1.44.0", caps: ["acks"] });
  });

  it("gateway_info_forget：作废该机器的能力记录；本就没有记录时原引用；其它机器的记录不动", () => {
    const init = initialState();
    expect(run(init, { type: "gateway_info_forget", machine: M })).toBe(init);
    const s = run(init, { type: "frame", frame: infoF(M) }, { type: "frame", frame: infoF(OTHER) });
    const other = s.gatewayInfo[OTHER];
    const f = run(s, { type: "gateway_info_forget", machine: M });
    expect(Object.keys(f.gatewayInfo)).toEqual([OTHER]);
    expect(f.gatewayInfo[OTHER]).toBe(other);
    expect(run(f, { type: "gateway_info_forget", machine: M })).toBe(f);
    expect(s.gatewayInfo[M]).toBeDefined(); // 不可变
  });

  it("能力丢失（加密会话换代 / 网关改报不支持）：仍 pending 的回显收敛为 unconfirmed；sent / failed / unconfirmed / 无 ack 的不动", () => {
    let s = run(initialState(), echo("c5")); // 能力未知时发出：无 ack
    s = run(
      s,
      { type: "frame", frame: infoF(M) },
      echo("c1"),
      echo("c2"),
      echo("c3"),
      echo("c4"),
      { type: "frame", frame: ackF("c1", "received") },
      { type: "frame", frame: ackF("c2", "rejected", REJECT_QUEUE_FULL) },
      timeout("c3"),
    );
    const losses: [string, AppState][] = [
      ["gateway_info_forget", run(s, { type: "gateway_info_forget", machine: M })],
      ["gateway_info caps 为空", run(s, { type: "frame", frame: infoF(M, "1.43.0", []) })],
      ["gateway_info caps 不含 acks", run(s, { type: "frame", frame: infoF(M, "1.44.0", ["something_else"]) })],
    ];
    for (const [how, lost] of losses) {
      expect(echoOf(lost, "c4")?.ack, how).toBe("unconfirmed");
      expect(echoOf(lost, "c1")?.ack, how).toBe("sent");
      expect(echoOf(lost, "c2"), how).toMatchObject({ ack: "failed", ackReason: "排队消息过多，这条被丢弃" });
      expect(echoOf(lost, "c3")?.ack, how).toBe("unconfirmed");
      expect(echoOf(lost, "c5"), how).not.toHaveProperty("ack");
      expect(machineSupportsAcks(lost, M), how).toBe(false);
    }
    // 没有待确认的回显时，作废只动能力记录：条目一根没碰（spaces 同一引用）
    const none = run(s, timeout("c4"));
    const forgotten = run(none, { type: "gateway_info_forget", machine: M });
    expect(forgotten.gatewayInfo).toEqual({});
    expect(forgotten.spaces).toBe(none.spaces);
  });

  it("作废之后：新发的回显不再带 ack（旧网关的行为）；网关应答回来后恢复跟踪，先前那条不回头补", () => {
    let s = run(acked(), { type: "gateway_info_forget", machine: M });
    s = run(s, echo("c1"));
    expect(echoOf(s, "c1")).not.toHaveProperty("ack");
    s = run(s, { type: "frame", frame: infoF(M) }, echo("c2"));
    expect(echoOf(s, "c1")).not.toHaveProperty("ack");
    expect(echoOf(s, "c2")?.ack).toBe("pending");
  });

  // ---- 与 v1.41.8 用户自发文本镜像的协作（键形态变化的回归）----

  it("带 clientId 的本机回显 + 同文本 self 帧：实时镜像被去重（原 state 引用）、history 的同文 self 行被吸收；回执仍落到这条回显上", () => {
    let s = run(acked(), echo("c1", "帮我跑下构建", PEER));
    const before = s;
    expect(echoOf(s, "c1", PEER)).toMatchObject({ key: "user:c1", ack: "pending" });
    s = run(s, {
      type: "frame",
      frame: frame(`{"v":1,"type":"text","convId":"${PEER}","text":"帮我跑下构建","at":1700000000000,"self":true,"machine":"${M}"}`),
    });
    expect(s).toBe(before);
    s = run(s, {
      type: "frame",
      frame: frame(
        `{"v":1,"type":"history","convId":"${PEER}","entries":[{"kind":"text","key":"h:5","text":"帮我跑下构建","at":1700000000000,"self":true}],"lastSeq":5,"trimmed":false,"machine":"${M}"}`,
      ),
    });
    expect(s).toBe(before);
    s = run(s, { type: "frame", frame: ackF("c1", "received", { convId: PEER }) });
    const entries = spaceOf(s, M).convs[PEER].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "user", key: "user:c1", ack: "sent" });
    expect(entries[0]).not.toHaveProperty("self");
  });
});

describe("timedOutAcks（v1.44 送达回执超时判定，纯函数）", () => {
  const M = "home-mac";
  const OTHER = "work-mac";
  const CONV = DEFAULT_CONV;
  const T0 = 1_795_000_000_000;
  /** 可控时钟：回显的 at 取 Date.now()（与 applyApprovalsSnapshot 组同手法）。 */
  let now = 0;
  beforeEach(() => {
    now = T0;
    vi.spyOn(Date, "now").mockImplementation(() => now);
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });
  const infoF = (machine: string): GatewayInfoFrame => ({
    v: 1,
    type: "gateway_info",
    machine,
    version: "1.44.0",
    caps: ["acks"],
  });
  const echo = (clientId: string, convId = CONV, machine = M): Action => ({
    type: "send_echo",
    convId,
    text: "x",
    clientId,
    machine,
  });
  const ackF = (clientId: string): AckFrame => ({
    v: 1,
    type: "ack",
    machine: M,
    clientId,
    convId: CONV,
    status: "received",
  });

  it("支持回执的机器：发出满 15 秒才列出（差 1ms 不列）", () => {
    const s = run(initialState(), { type: "frame", frame: infoF(M) }, echo("c1"));
    expect(timedOutAcks(s, T0 + ACK_TIMEOUT_MS - 1, true)).toEqual([]);
    expect(timedOutAcks(s, T0 + ACK_TIMEOUT_MS, true)).toEqual([{ machine: M, convId: CONV, clientId: "c1" }]);
    expect(timedOutAcks(s, T0 + 10 * ACK_TIMEOUT_MS, true)).toHaveLength(1);
  });

  it("WS 不在 ready：发出满 2 秒即列出（不必等 15 秒）；未满 2 秒不列出", () => {
    const s = run(initialState(), { type: "frame", frame: infoF(M) }, echo("c1"));
    expect(timedOutAcks(s, T0 + ACK_OFFLINE_TIMEOUT_MS - 1, false)).toEqual([]);
    expect(timedOutAcks(s, T0 + ACK_OFFLINE_TIMEOUT_MS, false)).toEqual([
      { machine: M, convId: CONV, clientId: "c1" },
    ]);
    // 对照：同一时刻 WS 是 ready → 不列
    expect(timedOutAcks(s, T0 + ACK_OFFLINE_TIMEOUT_MS, true)).toEqual([]);
  });

  it("机器不支持回执（没有能力记录）时不计时——哪怕条目还挂着 pending（旧网关永不回 ack）", () => {
    const s = run(initialState(), { type: "frame", frame: infoF(M) }, echo("c1"));
    const lost: AppState = { ...s, gatewayInfo: {} };
    expect(timedOutAcks(lost, T0 + 10 * ACK_TIMEOUT_MS, true)).toEqual([]);
    expect(timedOutAcks(lost, T0 + 10 * ACK_TIMEOUT_MS, false)).toEqual([]);
    // 对照：能力记录还在 → 同一时刻列出
    expect(timedOutAcks(s, T0 + 10 * ACK_TIMEOUT_MS, true)).toHaveLength(1);
  });

  it("只列 pending：sent / failed / unconfirmed / 无 ack 的回显永不列出", () => {
    let s = run(initialState(), echo("c5")); // 能力未知时发出：无 ack
    s = run(
      s,
      { type: "frame", frame: infoF(M) },
      echo("c1"),
      echo("c2"),
      echo("c3"),
      echo("c4"),
      { type: "frame", frame: ackF("c1") },
      { type: "frame", frame: { ...ackF("c2"), status: "rejected", reason: "malformed", message: "消息格式无效" } },
      { type: "ack_timeout", machine: M, convId: CONV, clientId: "c3" },
    );
    expect(timedOutAcks(s, T0 + 10 * ACK_TIMEOUT_MS, true)).toEqual([{ machine: M, convId: CONV, clientId: "c4" }]);
  });

  it("逐机器逐会话扫描：其它机器、其它会话里的待确认回显各自列出（非当前会话同样计时）", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: infoF(M) },
      { type: "frame", frame: infoF(OTHER) },
      echo("c1"),
      echo("c2", "web:work"),
      echo("c3", "web:work", OTHER),
    );
    expect(timedOutAcks(s, T0 + ACK_TIMEOUT_MS, true)).toEqual([
      { machine: M, convId: CONV, clientId: "c1" },
      { machine: M, convId: "web:work", clientId: "c2" },
      { machine: OTHER, convId: "web:work", clientId: "c3" },
    ]);
  });
});
