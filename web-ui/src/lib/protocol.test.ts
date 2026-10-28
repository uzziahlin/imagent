import { describe, expect, it } from "vitest";
import {
  DEFAULT_CONV,
  encodeFrame,
  initialState,
  parseFrame,
  reducer,
  type Action,
  type AppState,
  type InboundFrame,
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
});

describe("reducer", () => {
  it("初始状态含默认 conv，无消息", () => {
    const s = initialState();
    expect(s.convOrder).toEqual([DEFAULT_CONV]);
    expect(s.convs[DEFAULT_CONV]).toEqual({ entries: [], pending: [], lastPreview: "", lastAt: 0 });
    expect(s.machineOnline).toBe(false);
  });

  it("text 帧追加一条助手消息", () => {
    const s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("你好")) });
    const entries = s.convs[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "text", text: "你好" });
  });

  it("card 帧新建卡片条目", () => {
    const s = run(initialState(), {
      type: "frame",
      frame: frame(CARD_FRAME({ messageId: "m1", text: "流式中" })),
    });
    const entries = s.convs[DEFAULT_CONV].entries;
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
    const entries = s.convs[DEFAULT_CONV].entries;
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
    const entries = resynced.convs[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "card", messageId: "m1", state: "done" });
    expect((entries[0] as { card: { text: string } }).card.text).toBe("终稿");
  });

  it("approval_request 入 pending；同 requestId 幂等", () => {
    const req = '{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"rm -rf /tmp/x"}';
    let s = run(initialState(), { type: "frame", frame: frame(req) });
    expect(s.convs[DEFAULT_CONV].pending).toHaveLength(1);
    expect(s.convs[DEFAULT_CONV].pending[0]).toMatchObject({ requestId: "r1", tool: "Bash" });
    s = run(s, { type: "frame", frame: frame(req) });
    expect(s.convs[DEFAULT_CONV].pending).toHaveLength(1);
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
    expect(s.convs[DEFAULT_CONV].pending).toHaveLength(0);
    const entries = s.convs[DEFAULT_CONV].entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ kind: "approval_note", text: "已取消", message: "/stop 顶替" });
    // 无对应 pending 的 resolved 被忽略（防噪声）
    const again = run(s, {
      type: "frame",
      frame: frame('{"v":1,"type":"approval_resolved","convId":"web:default","requestId":"rX","decision":"allow"}'),
    });
    expect(again.convs[DEFAULT_CONV].entries).toHaveLength(1);
  });

  it("approval_sent 本地乐观移除", () => {
    let s = run(initialState(), {
      type: "frame",
      frame: frame('{"v":1,"type":"approval_request","convId":"web:default","requestId":"r1","tool":"Bash","inputSummary":"ls"}'),
    });
    s = run(s, { type: "approval_sent", convId: DEFAULT_CONV, requestId: "r1" });
    expect(s.convs[DEFAULT_CONV].pending).toHaveLength(0);
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
    expect(s.convs[DEFAULT_CONV].entries).toHaveLength(0);
  });

  it("conv 集合按帧收集，web:default 恒在首位", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("a", "web:work")) },
      { type: "frame", frame: frame(TEXT_FRAME("b", "web:proj-x")) },
      { type: "frame", frame: frame(TEXT_FRAME("c")) },
    );
    expect(s.convOrder).toEqual(["web:default", "web:work", "web:proj-x"]);
    expect(s.convs["web:work"].entries).toHaveLength(1);
    expect(s.convs["web:proj-x"].entries).toHaveLength(1);
    expect(s.convs["web:default"].entries).toHaveLength(1);
  });

  it("send_echo 追加用户回显到指定 conv", () => {
    const s = run(initialState(), { type: "send_echo", convId: "web:work", text: "列出目录" });
    expect(s.convOrder).toContain("web:work");
    expect(s.convs["web:work"].entries).toHaveLength(1);
    expect(s.convs["web:work"].entries[0]).toMatchObject({ kind: "user", text: "列出目录" });
  });

  it("会话概要：lastPreview/lastAt 随最后一条消息更新（user/text/card 各形态）", () => {
    let s = initialState();
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("");
    expect(s.convs[DEFAULT_CONV].lastAt).toBe(0);
    // text：取首行（夹具为 JSON 原文，换行需转义）
    s = run(s, { type: "frame", frame: frame(TEXT_FRAME("你好\\n第二行")) });
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("你好");
    expect(s.convs[DEFAULT_CONV].lastAt).toBeGreaterThan(0);
    // user 回显覆盖概要
    s = run(s, { type: "send_echo", convId: DEFAULT_CONV, text: "用户提问" });
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("用户提问");
    // card：正文首行（夹具为 JSON 原文，换行需转义）
    s = run(s, {
      type: "frame",
      frame: frame(CARD_FRAME({ messageId: "m1", text: "卡片正文首行\\n次行" })),
    });
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("卡片正文首行");
    // 空正文卡片回退 task_digest
    s = run(s, {
      type: "frame",
      frame: frame(
        '{"v":1,"type":"card","convId":"web:default","messageId":"m2","state":"streaming","card":{"text":"","tool_calls":[],"thoughts":[],"todos":[],"phase":"thinking","run_secs":1,"task_digest":"部署中","terminal":"running"}}',
      ),
    });
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("部署中");
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
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("后到的文本");
  });

  it("会话概要：各 conv 独立维护", () => {
    const s = run(
      initialState(),
      { type: "frame", frame: frame(TEXT_FRAME("默认会话消息")) },
      { type: "frame", frame: frame(TEXT_FRAME("工作会话消息", "web:work")) },
    );
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("默认会话消息");
    expect(s.convs["web:work"].lastPreview).toBe("工作会话消息");
  });

  it("内存护栏：超 200 条目丢弃最早的用户回显，服务端条目保留", () => {
    let s = run(initialState(), { type: "frame", frame: frame(TEXT_FRAME("服务端消息")) });
    const echoes: Action[] = Array.from({ length: 205 }, (_, i) => ({
      type: "send_echo",
      convId: DEFAULT_CONV,
      text: `msg${i}`,
    }));
    s = run(s, ...echoes);
    const entries = s.convs[DEFAULT_CONV].entries;
    // 206 条 → 丢 6 条最早 user 回显 → 恰好 200
    expect(entries).toHaveLength(200);
    // 服务端 text 条目不丢（卡片由 sync 兜底，text/user 回显属 ephemeral）
    expect(entries[0]).toMatchObject({ kind: "text", text: "服务端消息" });
    expect(entries.some((e) => e.kind === "user" && e.text === "msg0")).toBe(false);
    expect(entries.some((e) => e.kind === "user" && e.text === "msg5")).toBe(false);
    expect(entries.some((e) => e.kind === "user" && e.text === "msg6")).toBe(true);
    expect(entries.some((e) => e.kind === "user" && e.text === "msg204")).toBe(true);
    // 概要仍是最后一条
    expect(s.convs[DEFAULT_CONV].lastPreview).toBe("msg204");
  });

  it("内存护栏：无本地回显可丢时（纯服务端条目）不强截断", () => {
    const actions: Action[] = Array.from({ length: 205 }, (_, i) => ({
      type: "frame",
      frame: frame(TEXT_FRAME(`t${i}`)),
    }));
    const s = run(initialState(), ...actions);
    expect(s.convs[DEFAULT_CONV].entries).toHaveLength(205);
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
});
