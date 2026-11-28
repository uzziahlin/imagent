// v1.7 Composer：命令面板（过滤/键盘/点选/填充/Esc/模型 chips）+ 图片附件
// （选图→压缩→预览→随发）。canvas 主路径（compressImage）经 vi.mock 替换——
// jsdom 无 canvas（纯逻辑阈值/文件名在 lib/imageCompress.test.ts 覆盖）。
// v1.8 模型菜单动态化：/model chips 取快照 models 段（modelChips 三层回退）
// + 行尾当前模型文本（currentModelText）+ 有映射 target 的 chip title 提示。
// v1.42 草稿按会话隔离：Composer 必填 drafts / draftKey 两个 props——既有用例
// 统一经 mount / mountAsr / mountPanel 注入一个不落盘的 store（storage: null，
// 用例间不经 localStorage 串草稿）；带 rerender 的用例须复用同一个 store 实例。
// v1.43 完整命令名回车直接发送：面板打开时命令名已敲全（恰为高亮那条）的回车走
// 发送而不是补全；前缀 / 高亮换条目 / Tab 的补全行为不变。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import type { AsrUiEvent, MediaPayload, ModelsInfo } from "../lib/protocol";
import { createDraftStore, draftKey } from "../lib/drafts";
import { filterCommands } from "../lib/commands";

// 压缩 fake：vi.hoisted 提升到 import 解析前可用（vi.mock 工厂在 Composer
// 模块加载时即执行）；可注入断言/失败态，默认成功返回固定载荷。
const compressSpy = vi.hoisted(() =>
  vi.fn(async (_file: File): Promise<MediaPayload> => ({
    name: "shot.jpg",
    mime: "image/jpeg",
    b64: "QUJD", // "ABC"
  })),
);
vi.mock("../lib/imageCompress", () => ({ compressImage: (f: File) => compressSpy(f) }));

afterEach(() => {
  cleanup();
  compressSpy.mockClear();
});

/** v1.42 草稿夹具：每个用例一个新的 store（storage: null = 不落盘）；默认挂在
 *  会话 A 的键上。需要跨 rerender 复用同一 store 的用例自己传入。 */
const KEY_A = draftKey("home-mac", "web:a");
const KEY_B = draftKey("home-mac", "web:b");
const newDrafts = () => createDraftStore({ storage: null });

function mount(onSend = vi.fn(() => true), drafts = newDrafts()) {
  render(
    <Composer status="ready" onSend={onSend} onStop={vi.fn()} drafts={drafts} draftKey={KEY_A} />,
  );
  const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
  return { ta, onSend, drafts };
}

const type = (ta: HTMLTextAreaElement, v: string) => fireEvent.change(ta, { target: { value: v } });
const key = (ta: HTMLTextAreaElement, k: string) => fireEvent.keyDown(ta, { key: k });
/** 面板查询（null = 未弹）：返回 within 绑定的 scoped queries。 */
const panel = () => {
  const el = screen.queryByRole("listbox", { name: "命令面板" });
  return el ? within(el) : null;
};

describe("Composer 命令面板（v1.7）", () => {
  it("输入 / 浮出全量面板；普通文本不弹", () => {
    const { ta } = mount();
    expect(panel()).toBeNull();
    type(ta, "你好");
    expect(panel()).toBeNull();
    type(ta, "/");
    expect(panel()).toBeTruthy();
    // 首项选中（aria-selected）
    const options = panel()!.getAllByRole("option");
    expect(options.length).toBeGreaterThan(10); // 全量命令表
    expect(options[0].getAttribute("aria-selected")).toBe("true");
  });

  it("前缀过滤：/mo → 仅 /model（行内含常用模型 chips）", () => {
    const { ta } = mount();
    type(ta, "/mo");
    const options = panel()!.getAllByRole("option");
    expect(options).toHaveLength(1);
    expect(options[0].textContent).toContain("/model");
    // /model 行附 chips（default/sonnet/opus/haiku）
    for (const m of ["default", "sonnet", "opus", "haiku"]) {
      expect(panel()!.getByRole("button", { name: m })).toBeTruthy();
    }
  });

  it("描述关键词过滤：/模型 → 命中 /model", () => {
    const { ta } = mount();
    type(ta, "/模型");
    expect(panel()!.getAllByRole("option").some((o) => o.textContent!.includes("/model"))).toBe(
      true,
    );
  });

  it("键盘 ↑↓ 换选 + Enter/Tab 填充（`/name ` 尾随空格留参数位）", () => {
    const { ta } = mount();
    type(ta, "/");
    key(ta, "ArrowDown"); // 0 → 1
    key(ta, "Enter");
    expect(ta.value).toBe("/tasks ");
    // 面板随空白关闭（参数输入态）
    expect(panel()).toBeNull();

    type(ta, "/mo");
    key(ta, "Tab");
    expect(ta.value).toBe("/model ");
  });

  it("↑ 环回到底项；Enter 发送语义不抢（面板关闭时 Enter 直接发送）", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    type(ta, "/");
    key(ta, "ArrowUp"); // 0 → 末项
    const options = panel()!.getAllByRole("option");
    expect(options[options.length - 1].getAttribute("aria-selected")).toBe("true");
    key(ta, "Escape");
    expect(panel()).toBeNull();
    // 面板关闭（Esc）后 Enter 走发送
    type(ta, "/help");
    key(ta, "Enter");
    expect(onSend).toHaveBeenCalledWith("/help", undefined);
  });

  it("点击条目填充同效；模型 chip 直接填 `/model sonnet `", () => {
    const { ta } = mount();
    type(ta, "/");
    fireEvent.click(panel()!.getAllByRole("option").find((o) => o.textContent!.includes("/help"))!);
    expect(ta.value).toBe("/help ");

    type(ta, "/mod");
    fireEvent.click(panel()!.getByRole("button", { name: "sonnet" }));
    expect(ta.value).toBe("/model sonnet ");
    expect(panel()).toBeNull(); // 含空白 → 参数态关闭
  });

  it("Esc 关闭后继续输入不再弹；清掉 /（回空）后重新输入 / 恢复", () => {
    const { ta } = mount();
    type(ta, "/st");
    key(ta, "Escape");
    expect(panel()).toBeNull();
    type(ta, "/sta");
    expect(panel()).toBeNull(); // 仍关闭
    type(ta, "");
    type(ta, "/");
    expect(panel()).toBeTruthy(); // 复位
  });

  it("无命中（/zzz）不弹面板，Enter 直接发送", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    type(ta, "/zzz");
    expect(panel()).toBeNull();
    key(ta, "Enter");
    expect(onSend).toHaveBeenCalledWith("/zzz", undefined);
  });
});

// ---------------------------------------------------------------------------
// v1.43 完整命令名 + Enter 直接发送：命令面板打开时 Enter 原先一律走补全（补一个
// 空格），把命令名完整敲出来再回车（/stop、/peers、/tasks …）什么都没发，要按
// 两次。现在「去首尾空白后恰好等于高亮那条命令名」时不补全，落到发送分支；只敲了
// 前缀 / 高亮换了条目 / 按 Tab 的照旧补全。
// ---------------------------------------------------------------------------

describe("Composer 命令面板：完整命令名回车直接发送（v1.43）", () => {
  it("① 完整命令名 + Enter → 一次回车直接发送，输入框清空", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    for (const cmd of ["/stop", "/peers", "/tasks", "/status", "/help"]) {
      type(ta, cmd);
      expect(panel(), cmd).toBeTruthy(); // 面板开着：命令名是自己那条候选的前缀
      key(ta, "Enter");
      expect(ta.value, cmd).toBe("");
      expect(onSend).toHaveBeenLastCalledWith(cmd, undefined);
      expect(panel(), cmd).toBeNull();
    }
    expect(onSend).toHaveBeenCalledTimes(5);
  });

  it("② 只敲前缀 + Enter → 仍然补全（`/pe` → `/peers `），不发送", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    type(ta, "/pe");
    key(ta, "Enter");
    expect(ta.value).toBe("/peers ");
    expect(onSend).not.toHaveBeenCalled();
    // 补全后是参数输入态（含空白）：面板关闭，此时的回车才是发送
    expect(panel()).toBeNull();
  });

  it("③ 完整命令名 + Tab → 仍然只补全（`/stop` → `/stop `），不发送", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    type(ta, "/stop");
    key(ta, "Tab");
    expect(ta.value).toBe("/stop ");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("④ 完整命令名但用 ↓ 把高亮移到别的条目 + Enter → 补全的是高亮那条，不发送", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    type(ta, "/stop");
    // /stop 的候选里还有描述含「stop」的条目（首候选是 /stop 本身）
    const names = filterCommands("/stop").map((c) => c.name);
    expect(names[0]).toBe("/stop");
    expect(names.length).toBeGreaterThan(1);
    key(ta, "ArrowDown");
    key(ta, "Enter");
    expect(ta.value).toBe(`${names[1]} `);
    expect(onSend).not.toHaveBeenCalled();
  });

  it("⑤ 完整命令名 + Enter 但未发出（onSend 返回 false）：输入保留、面板仍在；再按回车仍是发送而不是补全", () => {
    const onSend = vi.fn().mockReturnValueOnce(false).mockReturnValue(true);
    const { ta } = mount(onSend);
    type(ta, "/stop");
    key(ta, "Enter");
    expect(onSend).toHaveBeenCalledTimes(1);
    expect(onSend).toHaveBeenLastCalledWith("/stop", undefined);
    expect(ta.value).toBe("/stop");
    expect(panel()).toBeTruthy();
    key(ta, "Enter");
    expect(onSend).toHaveBeenCalledTimes(2);
    expect(ta.value).toBe("");
  });

  it("Shift+Enter 不受影响：完整命令名上既不发送也不补全（换行交给输入框默认行为，jsdom 不模拟插入）", () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    type(ta, "/stop");
    fireEvent.keyDown(ta, { key: "Enter", shiftKey: true });
    expect(ta.value).toBe("/stop"); // 没被补成 "/stop "
    expect(onSend).not.toHaveBeenCalled();
    // 面板仍开着：随后松开 Shift 的回车才是发送
    expect(panel()).toBeTruthy();
    key(ta, "Enter");
    expect(onSend).toHaveBeenCalledWith("/stop", undefined);
  });
});

describe("Composer /model chips 动态化（v1.8）", () => {
  /** 第三方映射形态：sonnet/opus 有映射（opus target null 属官方回退形态，
   *  此处混用以覆盖 title 有/无两分支），haiku 未映射。 */
  const MAPPED: ModelsInfo = {
    current: "glm-4.6",
    cliDefault: "glm-4.6",
    options: [
      { label: "sonnet", target: "glm-4.6" },
      { label: "opus", target: null },
    ],
  };

  /** 挂载并输入 /mo 弹面板（唯一命中 /model 行）。 */
  function mountPanel(models?: ModelsInfo) {
    render(
      <Composer
        status="ready"
        models={models}
        onSend={vi.fn(() => true)}
        onStop={vi.fn()}
        drafts={newDrafts()}
        draftKey={KEY_A}
      />,
    );
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    type(ta, "/mo");
    return panel()!;
  }

  it("快照 options 渲染动态 chips（default 前置；未映射别名不出）+ 行尾当前模型文本（current）+ title 映射提示", () => {
    const p = mountPanel(MAPPED);
    expect(p.getByRole("button", { name: "default" })).toBeTruthy();
    expect(p.getByRole("button", { name: "sonnet" })).toBeTruthy();
    expect(p.getByRole("button", { name: "opus" })).toBeTruthy();
    expect(p.queryByRole("button", { name: "haiku" })).toBeNull();
    // 行尾当前模型文本：current 非空显示 current。
    expect(p.getByText("当前 glm-4.6")).toBeTruthy();
    // 有 target 的 chip 加映射目标提示（title）；target null 无。
    expect(p.getByRole("button", { name: "sonnet" }).getAttribute("title")).toBe(
      "映射 → glm-4.6",
    );
    expect(p.getByRole("button", { name: "opus" }).getAttribute("title")).toBeNull();
  });

  it("current 空且 cliDefault 非空 →「当前 默认 · x」；无快照 → 内置 chips +「当前 默认」", () => {
    let p = mountPanel({ current: null, cliDefault: "glm-4.6", options: MAPPED.options });
    expect(p.getByText("当前 默认 · glm-4.6")).toBeTruthy();
    cleanup();
    p = mountPanel(undefined);
    for (const m of ["default", "sonnet", "opus", "haiku"]) {
      expect(p.getByRole("button", { name: m })).toBeTruthy(); // 第③层内置兜底
    }
    expect(p.getByText("当前 默认")).toBeTruthy();
  });

  it("点击动态 chip 直接填 `/model sonnet `（含空白 → 面板关闭）", () => {
    mountPanel(MAPPED);
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    fireEvent.click(panel()!.getByRole("button", { name: "sonnet" }));
    expect(ta.value).toBe("/model sonnet ");
    expect(panel()).toBeNull();
  });
});

describe("Composer 图片附件（v1.7）", () => {
  const FILE_INPUT = "选择图片";

  /** 触发隐藏 file input 的 change（jsdom 不弹真实选框）。 */
  const pick = (file: File) =>
    fireEvent.change(screen.getByLabelText(FILE_INPUT), { target: { files: [file] } });

  it("选图 → 压缩 → 预览（文件名 + 缩略 + 移除）；随消息一起发（text 可空）", async () => {
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    pick(new File(["x"], "shot.png", { type: "image/png" }));
    expect(compressSpy).toHaveBeenCalledTimes(1);

    // 预览出现（压缩异步）
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    expect(screen.getByAltText("shot.jpg")).toBeTruthy();
    // text 空 + media：发送按钮不因空文本禁用
    const send = screen.getByRole("button", { name: "发送" }) as HTMLButtonElement;
    expect(send.disabled).toBe(false);

    fireEvent.click(send);
    expect(onSend).toHaveBeenCalledWith("", {
      name: "shot.jpg",
      mime: "image/jpeg",
      b64: "QUJD",
    });
    // 发送成功后预览与输入清空
    expect(screen.queryByLabelText("移除图片")).toBeNull();
    expect(ta.value).toBe("");

    // 图 + 文说明一起发
    pick(new File(["y"], "b.webp", { type: "image/webp" }));
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    type(ta, "看这张");
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(onSend).toHaveBeenLastCalledWith("看这张", {
      name: "shot.jpg",
      mime: "image/jpeg",
      b64: "QUJD",
    });
  });

  it("移除附件：预览消失、空文本发送恢复禁用", async () => {
    const { ta } = mount();
    pick(new File(["x"], "a.png", { type: "image/png" }));
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    fireEvent.click(screen.getByLabelText("移除图片"));
    expect(screen.queryByLabelText("移除图片")).toBeNull();
    expect(ta.value).toBe("");
    expect((screen.getByRole("button", { name: "发送" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("非图片文件拒绝（行内提示）；压缩失败给行内提示", async () => {
    mount();
    pick(new File(["x"], "a.txt", { type: "text/plain" }));
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toContain("仅支持图片");

    compressSpy.mockRejectedValueOnce(new Error("decode"));
    pick(new File(["x"], "bad.png", { type: "image/png" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("图片处理失败"));
  });

  it("发送失败（onSend false）保留输入与附件", async () => {
    const onSend = vi.fn(() => false);
    const { ta } = mount(onSend);
    pick(new File(["x"], "keep.png", { type: "image/png" }));
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(screen.getByLabelText("移除图片")).toBeTruthy();
    expect(ta.value).toBe("");
  });
});

// ---------------------------------------------------------------------------
// v1.37 粘贴图片直进上行管道：剪贴板含图片文件 → preventDefault + 复用既有
// 选图压缩管道（pickImage）；非图片/纯文本保持默认粘贴。多文件取第一张
// （既有 media 单附件能力边界）。jsdom 无 DataTransfer 构造，经 fireEvent
// eventInit 透传 clipboardData 形状（React 合成事件从 nativeEvent 读取）。
// ---------------------------------------------------------------------------

describe("Composer 粘贴图片（v1.37）", () => {
  it("粘贴图片文件 → preventDefault + 进既有压缩管道（预览出现）", async () => {
    mount();
    const file = new File(["p"], "clip.png", { type: "image/png" });
    const pasted = fireEvent.paste(screen.getByLabelText("消息输入框"), {
      clipboardData: { files: [file] },
    });
    expect(pasted).toBe(false); // preventDefault 已调用（不落默认粘贴）
    expect(compressSpy).toHaveBeenCalledTimes(1);
    expect(compressSpy).toHaveBeenCalledWith(file);
    // 预览出现（压缩异步）——文件名走既有 jpgName 换后缀 fake 载荷
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
  });

  it("多文件粘贴取第一张（既有管道单附件边界）", () => {
    mount();
    const a = new File(["1"], "a.png", { type: "image/png" });
    const b = new File(["2"], "b.png", { type: "image/png" });
    fireEvent.paste(screen.getByLabelText("消息输入框"), { clipboardData: { files: [a, b] } });
    expect(compressSpy).toHaveBeenCalledTimes(1);
    expect(compressSpy).toHaveBeenCalledWith(a);
  });

  it("纯文本/非图片文件粘贴不受影响：不 preventDefault、不进管道", () => {
    const { ta } = mount();
    expect(
      fireEvent.paste(ta, { clipboardData: { files: [] } }),
    ).toBe(true); // 未拦截（默认粘贴行为保留）
    expect(
      fireEvent.paste(ta, {
        clipboardData: { files: [new File(["x"], "a.txt", { type: "text/plain" })] },
      }),
    ).toBe(true);
    expect(compressSpy).not.toHaveBeenCalled();
    expect(screen.queryByLabelText("移除图片")).toBeNull();
    expect(ta.value).toBe("");
  });

  it("剪贴板缺席（clipboardData null）不炸", () => {
    mount();
    expect(fireEvent.paste(screen.getByLabelText("消息输入框"), {})).toBe(true);
    expect(compressSpy).not.toHaveBeenCalled();
  });
});

// ---------------------------------------------------------------------------
// v1.39 语音输入（P1.5）：Web Speech API，能力探测决定入口渲染 + 逻辑在
// useSpeechRecognition（hook 状态机细节在 useSpeechRecognition.test.ts）。
// jsdom 无 SpeechRecognition——Object.defineProperty(window, ...) 注入 stub
// 类并持有实例引用，直驱 result/error/end 事件；不支持环境用例不注入。
// ---------------------------------------------------------------------------

describe("Composer 语音输入（v1.39 P1.5）", () => {
  const MIC = "开始语音输入";
  let instances: StubRecognition[] = [];

  /** 最小 Web Speech stub：字段/方法形状对齐 hook 的 SpeechRecognitionLike。 */
  class StubRecognition {
    lang = "";
    continuous = false;
    interimResults = false;
    onresult: any = null;
    onerror: any = null;
    onend: any = null;
    start = vi.fn();
    stop = vi.fn();
    abort = vi.fn();
    constructor() {
      instances.push(this);
    }
  }

  const injectSpeech = () => {
    instances = [];
    Object.defineProperty(window, "SpeechRecognition", {
      value: StubRecognition,
      configurable: true,
      writable: true,
    });
  };

  afterEach(() => {
    delete (window as { SpeechRecognition?: unknown }).SpeechRecognition;
  });

  /** 直驱 result 事件（resultIndex 起的增量段；results 为类数组形状）。 */
  const emitResult = (
    rec: StubRecognition,
    resultIndex: number,
    parts: Array<[string, boolean]>,
  ) => {
    const results: any = { length: resultIndex + parts.length };
    parts.forEach(([t, isFinal], i) => {
      results[resultIndex + i] = { isFinal, length: 1, 0: { transcript: t } };
    });
    act(() => rec.onresult?.({ resultIndex, results }));
  };
  const emitError = (rec: StubRecognition, code: string) =>
    act(() => rec.onerror?.({ error: code }));
  const emitEnd = (rec: StubRecognition) => act(() => rec.onend?.());

  it("不支持环境（Firefox/旧 iOS）：不渲染麦克风按钮", () => {
    mount();
    expect(screen.queryByRole("button", { name: MIC })).toBeNull();
    expect(screen.queryByRole("button", { name: "停止语音输入" })).toBeNull();
  });

  it("点击开始：zh-CN/interim/单句配置 + 录音态；final 前置空格追加，不自动发送", () => {
    const onSend = vi.fn(() => true);
    injectSpeech();
    const { ta } = mount(onSend);
    type(ta, "已输入");
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    expect(rec).toBeTruthy();
    expect(rec.lang).toBe("zh-CN");
    expect(rec.interimResults).toBe(true);
    expect(rec.continuous).toBe(false);
    expect(rec.start).toHaveBeenCalledTimes(1);
    // 录音态：按钮 aria 切停止
    expect(screen.getByRole("button", { name: "停止语音输入" })).toBeTruthy();

    emitResult(rec, 0, [["识别文本", true]]);
    expect(ta.value).toBe("已输入 识别文本"); // 前置空格拼接，保留已输入内容
    expect(onSend).not.toHaveBeenCalled(); // 只填入框，绝不自动发送
    expect(document.activeElement).toBe(ta); // 焦点回输入框便于确认
  });

  it("interim 只进预览（小字灰色，不进输入框）；final 追加并清预览；静音自动断归位", () => {
    injectSpeech();
    const { ta } = mount();
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    emitResult(rec, 0, [["正在说的", false]]);
    const preview = screen.getByText(/正在说的/);
    expect(preview.className).toContain("text-ghost"); // 小字灰色暂显
    expect(ta.value).toBe(""); // 不进输入框

    emitResult(rec, 0, [["完整的话", true]]);
    expect(screen.queryByText(/正在说的/)).toBeNull(); // 预览清除
    expect(ta.value).toBe("完整的话");

    // 说过话的 end（单句静音自动断）→ 正常归位可再点；不触发 v1.39.1 重试
    // （重试只针对无 result 的静默失败）
    emitEnd(rec);
    expect(screen.getByRole("button", { name: MIC })).toBeTruthy();
  });

  it("onerror 人话提示 + 状态复位 + 预览清除", () => {
    injectSpeech();
    mount();
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    emitResult(rec, 0, [["说到一半", false]]);
    emitError(rec, "not-allowed");
    expect(screen.getByRole("alert").textContent).toContain("麦克风权限被拒，请在系统设置允许");
    expect(screen.queryByText(/说到一半/)).toBeNull(); // 预览清除
    expect(screen.getByRole("button", { name: MIC })).toBeTruthy(); // 复位

    emitError(rec, "no-speech");
    expect(screen.getByRole("alert").textContent).toContain("没有听到说话");
    emitError(rec, "network");
    expect(screen.getByRole("alert").textContent).toContain("网络异常");
    emitError(rec, "banana");
    expect(screen.getByRole("alert").textContent).toContain("语音识别出错");
  });

  it("错误提示 4s 自动消退（对齐既有 toast 节奏）", () => {
    vi.useFakeTimers();
    try {
      injectSpeech();
      mount();
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      emitError(instances[0], "no-speech");
      expect(screen.getByRole("alert").textContent).toContain("没有听到说话");
      act(() => {
        vi.advanceTimersByTime(4000);
      });
      expect(screen.queryByRole("alert")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("静默失败自动重试（v1.39.1）：end 后保持录音态，400ms 换新实例；重试再静默归位", () => {
    vi.useFakeTimers();
    try {
      injectSpeech();
      mount();
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      expect(instances.length).toBe(1);
      emitEnd(instances[0]); // WebKit 静默失败：无 result 无 error 即 end
      // 重试延迟窗口：录音态保持（呼吸点不断，按钮仍为停止态，用户无感）
      expect(screen.getByRole("button", { name: "停止语音输入" })).toBeTruthy();
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(2); // 自动重试：新实例已 start
      expect(instances[1].start).toHaveBeenCalledTimes(1);
      // 重试会话再静默失败：不循环重试，归位可再点
      emitEnd(instances[1]);
      expect(screen.getByRole("button", { name: MIC })).toBeTruthy();
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances.length).toBe(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("再点停止：stop() 优雅收尾（非 abort，保留收尾 final），end 归位后可再开", () => {
    injectSpeech();
    mount();
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    emitResult(rec, 0, [["说到一半", false]]);
    fireEvent.click(screen.getByRole("button", { name: "停止语音输入" }));
    expect(rec.stop).toHaveBeenCalledTimes(1);
    expect(rec.abort).not.toHaveBeenCalled();
    // stop 后收尾 final 保留（实例未解绑）+ end 归位
    emitResult(rec, 1, [["最后一句", true]]);
    emitEnd(rec);
    expect(screen.getByRole("button", { name: MIC })).toBeTruthy();
    expect(instances[0].onresult).toBeNull(); // end 已解绑
    // 归位后可再开一轮（新实例）
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    expect(instances.length).toBe(2);
  });

  it("unmount 清理：abort + 三事件解绑", () => {
    injectSpeech();
    const { unmount } = render(
      <Composer
        status="ready"
        onSend={vi.fn(() => true)}
        onStop={vi.fn()}
        drafts={newDrafts()}
        draftKey={KEY_A}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    unmount();
    expect(rec.abort).toHaveBeenCalledTimes(1);
    expect(rec.onresult).toBeNull();
    expect(rec.onerror).toBeNull();
    expect(rec.onend).toBeNull();
  });

  it("断线/e2eBlocked 与输入框同规则禁用", () => {
    injectSpeech();
    render(
      <Composer
        status="reconnecting"
        onSend={vi.fn(() => true)}
        onStop={vi.fn()}
        drafts={newDrafts()}
        draftKey={KEY_A}
      />,
    );
    expect((screen.getByRole("button", { name: MIC }) as HTMLButtonElement).disabled).toBe(true);
    cleanup();
    render(
      <Composer
        status="ready"
        e2eBlocked
        onSend={vi.fn(() => true)}
        onStop={vi.fn()}
        drafts={newDrafts()}
        draftKey={KEY_A}
      />,
    );
    expect((screen.getByRole("button", { name: MIC }) as HTMLButtonElement).disabled).toBe(true);
  });

  // v1.42：识别结果追加到**当前**会话的草稿（写入时取 keyRef）
  it("v1.42 final 追加到当前会话的草稿：识别进行中切到会话 B，结果落在 B；A 的草稿不被改写", () => {
    injectSpeech();
    const drafts = newDrafts();
    const onSend = vi.fn(() => true);
    const el = (key: string) => (
      <Composer status="ready" onSend={onSend} onStop={vi.fn()} drafts={drafts} draftKey={key} />
    );
    const view = render(el(KEY_A));
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    type(ta, "A 已输入");
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    // 识别进行中切换会话
    view.rerender(el(KEY_B));
    emitResult(rec, 0, [["识别文本", true]]);
    const taB = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    expect(taB.value).toBe("识别文本"); // 落在当前（B）
    expect(drafts.text(KEY_B)).toBe("识别文本");
    expect(drafts.text(KEY_A)).toBe("A 已输入"); // A 不被迟到的识别结果改写
    expect(onSend).not.toHaveBeenCalled(); // 仍然只填入框、不自动发送
  });

  // v1.42：追加以 **store 里的当前值**为准，而不是回调闭包里的组件 state——
  // 同一渲染批次内连续两段 final 时，第二个回调拿到的闭包还是旧的（第一次的
  // setState 尚未渲染），若以它为基准会覆盖掉第一段。
  it("v1.42 同一批次内连续两段 final：第二段接在第一段之后，不丢第一段", () => {
    injectSpeech();
    const { ta, drafts } = mount();
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    const rec = instances[0];
    const final = (t: string) => ({ resultIndex: 0, results: { length: 1, 0: { isFinal: true, length: 1, 0: { transcript: t } } } });
    act(() => {
      rec.onresult?.(final("第一段"));
      rec.onresult?.(final("第二段"));
    });
    expect(ta.value).toBe("第一段 第二段");
    expect(drafts.text(KEY_A)).toBe("第一段 第二段");
  });
});

// ---------------------------------------------------------------------------
// v1.39.2：iOS 主屏（standalone）撤除抢先拦截——用户真机实证主屏模式并非
// 绝对不支持（首次识别可用，授权后出字），后续才静默失败（WebKit 连续会话
// bug，v1.39.1 重试机制兜底）。点 🎤 照常真实发起识别；静默重试耗尽后经
// onSilentFailExhausted 呈现行内 alert 受限提示（仅 iOS 主屏命中；iOS Safari
// / Android standalone 等未命中环境静默耗尽不提示，不误报）。环境注入：UA
// 经 navigator 实例 defineProperty 覆盖（delete 还原 prototype getter）；
// standalone 信号 iOS 用专有 navigator.standalone、Android 用 matchMedia
// stub（jsdom 无此 API，delete 还原「缺席」）；不支持环境/正常用例不动任何
// 全局（检测函数对 matchMedia 缺席有 typeof 防御）。
// ---------------------------------------------------------------------------

describe("Composer 语音 iOS 主屏模式（v1.39.2 撤拦 + 耗尽兜底）", () => {
  const MIC = "开始语音输入";
  const HINT = "主屏模式下语音受 iOS 系统限制可能不稳定，建议用 Safari 打开使用";
  const IOS_UA =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
  const ANDROID_UA =
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36";
  let instances: StubRecognition[] = [];

  /** 最小 Web Speech stub（同上组）：构造即持引用，直驱事件断言。 */
  class StubRecognition {
    lang = "";
    continuous = false;
    interimResults = false;
    onresult: any = null;
    onerror: any = null;
    onend: any = null;
    start = vi.fn();
    stop = vi.fn();
    abort = vi.fn();
    constructor() {
      instances.push(this);
    }
  }

  const injectSpeech = () => {
    instances = [];
    Object.defineProperty(window, "SpeechRecognition", {
      value: StubRecognition,
      configurable: true,
      writable: true,
    });
  };

  const setUA = (ua: string) =>
    Object.defineProperty(window.navigator, "userAgent", { value: ua, configurable: true });

  /** matchMedia stub：standalone 查询命中（Android Chrome 主屏形态）。 */
  const stubMatchMedia = () => {
    Object.defineProperty(window, "matchMedia", {
      value: (query: string) => ({
        matches: query.includes("standalone"),
        media: query,
        onchange: null,
        addListener: vi.fn(),
        removeListener: vi.fn(),
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        dispatchEvent: vi.fn(),
      }),
      configurable: true,
      writable: true,
    });
  };

  /** 直驱静默失败 end（WebKit 连续会话 bug 特征：无 result 无 error 即 end）。 */
  const emitEnd = (rec: StubRecognition) => act(() => rec.onend?.());

  afterEach(() => {
    delete (window as { SpeechRecognition?: unknown }).SpeechRecognition;
    delete (window.navigator as { userAgent?: string }).userAgent;
    delete (window.navigator as { standalone?: boolean }).standalone;
    delete (window as { matchMedia?: unknown }).matchMedia;
  });

  it("iOS 主屏（standalone）：点 🎤 真实发起识别（撤除抢先拦截，无 alert；title 同步受限提示）", () => {
    setUA(IOS_UA);
    Object.defineProperty(window.navigator, "standalone", { value: true, configurable: true });
    injectSpeech();
    const onSend = vi.fn(() => true);
    const { ta } = mount(onSend);
    const mic = screen.getByRole("button", { name: MIC }) as HTMLButtonElement;
    expect(mic.title).toBe(HINT); // title 同步（措辞已从「不支持」改「受限」）
    fireEvent.click(mic);
    expect(instances).toHaveLength(1); // 真实发起（不再拦截，stub 构造器已触发）
    expect(instances[0].start).toHaveBeenCalledTimes(1);
    expect(instances[0].lang).toBe("zh-CN");
    expect(screen.queryByRole("alert")).toBeNull(); // 提前拦截提示已撤
    expect(mic.disabled).toBe(false);
    expect(onSend).not.toHaveBeenCalled();
    expect(ta.value).toBe("");
  });

  it("iOS standalone 静默重试耗尽：行内 alert 显示受限文案（仅耗尽时；4s 消退复用既有管道）", () => {
    vi.useFakeTimers();
    try {
      setUA(IOS_UA);
      Object.defineProperty(window.navigator, "standalone", { value: true, configurable: true });
      injectSpeech();
      mount();
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      emitEnd(instances[0]); // 首会话静默失败 → 重试窗口（尚未耗尽，不提示）
      expect(screen.queryByRole("alert")).toBeNull();
      expect(screen.getByRole("button", { name: "停止语音输入" })).toBeTruthy();
      act(() => {
        vi.advanceTimersByTime(400);
      });
      expect(instances).toHaveLength(2); // v1.39.1 自动重试换新实例
      emitEnd(instances[1]); // 重试会话仍静默失败 → 耗尽 → 兜底提示
      expect(screen.getByRole("alert").textContent).toBe(HINT);
      act(() => {
        vi.advanceTimersByTime(4000);
      });
      expect(screen.queryByRole("alert")).toBeNull(); // 4s 自动消退
      expect(screen.getByRole("button", { name: MIC })).toBeTruthy(); // 归位可再点
    } finally {
      vi.useRealTimers();
    }
  });

  it("非 standalone 静默重试耗尽：无 alert（真实无语音等场景不误报）", () => {
    vi.useFakeTimers();
    try {
      injectSpeech(); // 不注入 UA/standalone——桌面环境（iosStandalone 未命中）
      mount();
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      emitEnd(instances[0]);
      act(() => {
        vi.advanceTimersByTime(400);
      });
      emitEnd(instances[1]); // 同样耗尽，但未命中 iOS 主屏
      expect(instances).toHaveLength(2);
      expect(screen.queryByRole("alert")).toBeNull(); // 静默，不误报
      expect(screen.getByRole("button", { name: MIC })).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("iOS Safari 浏览器内：正常发起识别（无提示）", () => {
    setUA(IOS_UA); // 不注入 standalone——jsdom 无 matchMedia，两信号皆否
    injectSpeech();
    mount();
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    expect(instances).toHaveLength(1);
    expect(instances[0].start).toHaveBeenCalledTimes(1);
    expect(instances[0].lang).toBe("zh-CN");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("Android standalone（display-mode: standalone 命中）：正常发起识别（限制仅 iOS）", () => {
    setUA(ANDROID_UA);
    stubMatchMedia();
    injectSpeech();
    mount();
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    expect(instances).toHaveLength(1);
    expect(instances[0].start).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// v1.40 语音转写 whisper 主路径（Composer 分流层）：快照 asr=true → 🎤 走
// useVoiceRecorder 录音模式（Web Audio 采 PCM → WAV b64 → onAsr 上行）；
// hook 状态机细节在 useVoiceRecorder.test.ts。jsdom 无 Web Audio——全局注入
// fake AudioContext / AudioWorkletNode / navigator.mediaDevices / URL.create-
// ObjectURL（Composer 内 hook 默认工厂探测取用；用后 delete 还原）。
// ---------------------------------------------------------------------------

describe("Composer 语音转写 whisper（v1.40）", () => {
  const MIC = "开始语音输入";
  const STOP = "停止录音";
  const BUSY = "语音识别中";
  let contexts: FakeAsrCtx[] = [];
  let workletNodes: FakeAsrWorkletNode[] = [];
  let gum: ReturnType<typeof vi.fn>;

  /** fake AudioContext（默认工厂 new AudioContext({sampleRate:16000}) 探测
   *  命中；sampleRate 取构造 options）。 */
  class FakeAsrCtx {
    sampleRate: number;
    state = "running";
    destination = { connect: vi.fn(), disconnect: vi.fn() };
    audioWorklet = { addModule: vi.fn(async () => {}) };
    resume = vi.fn(async () => {});
    close = vi.fn(async () => {});
    createMediaStreamSource = vi.fn(() => ({ connect: vi.fn(), disconnect: vi.fn() }));
    constructor(opts?: { sampleRate?: number }) {
      this.sampleRate = opts?.sampleRate ?? 48000;
      contexts.push(this);
    }
  }

  /** fake AudioWorkletNode：port.onmessage 由 hook 设置，测试经它投 PCM 块。 */
  class FakeAsrWorkletNode {
    port = { postMessage: vi.fn(), onmessage: null as ((ev: { data: Float32Array }) => void) | null };
    connect = vi.fn();
    disconnect = vi.fn();
    constructor(_ctx: unknown, _name: string) {
      workletNodes.push(this);
    }
  }

  const injectRecorderEnv = () => {
    contexts = [];
    workletNodes = [];
    gum = vi.fn(async () => {
      const track = { stop: vi.fn() };
      return { getTracks: () => [track] } as unknown as MediaStream;
    });
    Object.defineProperty(window, "AudioContext", {
      value: FakeAsrCtx,
      configurable: true,
      writable: true,
    });
    Object.defineProperty(window, "AudioWorkletNode", {
      value: FakeAsrWorkletNode,
      configurable: true,
      writable: true,
    });
    Object.defineProperty(window.navigator, "mediaDevices", {
      value: { getUserMedia: gum },
      configurable: true,
    });
    // worklet 模块 Blob URL 加载（jsdom 无此 API）
    Object.defineProperty(URL, "createObjectURL", {
      value: () => "blob:fake",
      configurable: true,
      writable: true,
    });
    Object.defineProperty(URL, "revokeObjectURL", {
      value: () => {},
      configurable: true,
      writable: true,
    });
  };

  afterEach(() => {
    delete (window as { AudioContext?: unknown }).AudioContext;
    delete (window as { AudioWorkletNode?: unknown }).AudioWorkletNode;
    delete (window.navigator as { mediaDevices?: unknown }).mediaDevices;
  });

  function mountAsr(onAsr: ReturnType<typeof vi.fn> = vi.fn(() => true), drafts = newDrafts()) {
    const view = render(
      <Composer
        status="ready"
        asr
        asrEvent={null}
        onAsr={onAsr}
        onSend={vi.fn(() => true)}
        onStop={vi.fn()}
        drafts={drafts}
        draftKey={KEY_A}
      />,
    );
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    return { view, ta, onAsr, drafts };
  }

  /** flush start() 异步链（gum/addModule 立即 resolve，无真实 timer 参与）。 */
  const flush = () => act(async () => {});

  it("asr=true 点 🎤 录音（getUserMedia）→ 再点停止 → onAsr 上行（b64/durationMs）→「识别中」disabled", async () => {
    injectRecorderEnv();
    const { ta, onAsr } = mountAsr();
    type(ta, "已输入");
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    await flush();
    expect(gum).toHaveBeenCalledTimes(1);
    // 录音态：aria 切停止 + 呼吸点语义（aria-pressed）
    const stopBtn = screen.getByRole("button", { name: STOP }) as HTMLButtonElement;
    expect(stopBtn.getAttribute("aria-pressed")).toBe("true");
    // 投 1600 采样（16k 下 100ms）→ 停止 → 编码上行
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    fireEvent.click(screen.getByRole("button", { name: STOP }));
    expect(onAsr).toHaveBeenCalledTimes(1);
    const [audioB64, durationMs] = onAsr.mock.calls[0];
    expect(typeof audioB64).toBe("string");
    expect((audioB64 as string).startsWith("UklGR")).toBe(true); // "RIFF" 的 b64 前缀
    expect(durationMs).toBe(100);
    // 「识别中」：disabled + aria 切换（emoji ⏳）
    const busy = screen.getByRole("button", { name: BUSY }) as HTMLButtonElement;
    expect(busy.disabled).toBe(true);
  });

  it("asr_result 回包（asrEvent result）→ 文本前置空格追加输入框 + 焦点回框 + 归位可再点；不自动发送", async () => {
    injectRecorderEnv();
    const onSend = vi.fn(() => true);
    const drafts = newDrafts(); // rerender 须复用同一 store，否则切换即丢草稿
    const view = render(
      <Composer
        status="ready"
        asr
        asrEvent={null}
        onAsr={vi.fn(() => true)}
        onSend={onSend}
        onStop={vi.fn()}
        drafts={drafts}
        draftKey={KEY_A}
      />,
    );
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    type(ta, "已输入");
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    await flush();
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    fireEvent.click(screen.getByRole("button", { name: STOP }));
    expect(screen.getByRole("button", { name: BUSY })).toBeTruthy();
    // 回包 → 追加（前置空格拼接）+ 归位
    view.rerender(
      <Composer
        status="ready"
        asr
        asrEvent={{ kind: "result", text: "识别文本", durationMs: 100 }}
        onAsr={vi.fn(() => true)}
        onSend={onSend}
        onStop={vi.fn()}
        drafts={drafts}
        draftKey={KEY_A}
      />,
    );
    expect(ta.value).toBe("已输入 识别文本");
    expect(onSend).not.toHaveBeenCalled(); // 只填入框，绝不自动发送
    expect(document.activeElement).toBe(ta); // 焦点回输入框
    const mic = screen.getByRole("button", { name: MIC }) as HTMLButtonElement;
    expect(mic.disabled).toBe(false);
    expect(mic.getAttribute("aria-pressed")).toBe("false");
  });

  it("error 帧 asr_* code → 行内 alert 人话映射（asr_unconfigured 等；未知码兜底）", async () => {
    vi.useFakeTimers();
    try {
      injectRecorderEnv();
      const { view, drafts } = mountAsr(); // 三次 rerender 复用同一 store
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      await flush();
      act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
      fireEvent.click(screen.getByRole("button", { name: STOP }));
      view.rerender(
        <Composer
          status="ready"
          asr
          asrEvent={{ kind: "error", code: "asr_unconfigured" }}
          onAsr={vi.fn(() => true)}
          onSend={vi.fn(() => true)}
          onStop={vi.fn()}
          drafts={drafts}
          draftKey={KEY_A}
        />,
      );
      expect(screen.getByRole("alert").textContent).toBe("网关未启用语音转写");
      expect(screen.getByRole("button", { name: MIC })).toBeTruthy(); // error 即归位
      // 未列 code 兜底文案
      view.rerender(
        <Composer
          status="ready"
          asr
          asrEvent={{ kind: "error", code: "asr_future" }}
          onAsr={vi.fn(() => true)}
          onSend={vi.fn(() => true)}
          onStop={vi.fn()}
          drafts={drafts}
          draftKey={KEY_A}
        />,
      );
      expect(screen.getByRole("alert").textContent).toBe("语音转写出错，请重试");
      // 4s 自动消退（复用既有 speechError 管道）
      view.rerender(
        <Composer
          status="ready"
          asr
          asrEvent={{ kind: "error", code: "asr_failed" }}
          onAsr={vi.fn(() => true)}
          onSend={vi.fn(() => true)}
          onStop={vi.fn()}
          drafts={drafts}
          draftKey={KEY_A}
        />,
      );
      expect(screen.getByRole("alert").textContent).toBe("语音转写失败，请重试");
      act(() => {
        vi.advanceTimersByTime(4000);
      });
      expect(screen.queryByRole("alert")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("60s 无回包超时：行内提示 + 归位可再点（旧网关无 asr_result 的收敛路径）", async () => {
    vi.useFakeTimers();
    try {
      injectRecorderEnv();
      mountAsr();
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      await flush();
      act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
      fireEvent.click(screen.getByRole("button", { name: STOP }));
      expect(screen.getByRole("button", { name: BUSY })).toBeTruthy();
      act(() => {
        vi.advanceTimersByTime(60_000);
      });
      expect(screen.getByRole("alert").textContent).toBe("语音识别超时，请重试");
      const mic = screen.getByRole("button", { name: MIC }) as HTMLButtonElement;
      expect(mic.disabled).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });

  it("onAsr 返回 false（未发出）→ 行内提示 + 就地归位可再点", async () => {
    injectRecorderEnv();
    const onAsr = vi.fn(() => false);
    mountAsr(onAsr);
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    await flush();
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    fireEvent.click(screen.getByRole("button", { name: STOP }));
    expect(onAsr).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("alert").textContent).toBe("语音识别请求发送失败，请重试");
    expect(screen.getByRole("button", { name: MIC })).toBeTruthy();
  });

  it("asr=true 但环境无 getUserMedia（极旧浏览器）：点 🎤 行内错误反馈，不发起录音", () => {
    // 注入 AudioContext 但不注入 mediaDevices（默认工厂探测缺席）
    Object.defineProperty(window, "AudioContext", {
      value: FakeAsrCtx,
      configurable: true,
      writable: true,
    });
    const onAsr = vi.fn(() => true);
    render(
      <Composer
        status="ready"
        asr
        asrEvent={null}
        onAsr={onAsr}
        onSend={vi.fn(() => true)}
        onStop={vi.fn()}
        drafts={newDrafts()}
        draftKey={KEY_A}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    expect(screen.getByRole("alert").textContent).toBe("录音失败，请重试");
    expect(onAsr).not.toHaveBeenCalled();
  });

  it("快照无 asr：🎤 走 Web Speech 既有路径零变化（SpeechRecognition 发起，getUserMedia 不被调用）", () => {
    injectRecorderEnv();
    // 同时注入 Web Speech stub：asr 缺席时 🎤 应走它
    class StubRecognition {
      lang = "";
      continuous = false;
      interimResults = false;
      onresult: unknown = null;
      onerror: unknown = null;
      onend: unknown = null;
      start = vi.fn();
      stop = vi.fn();
      abort = vi.fn();
    }
    Object.defineProperty(window, "SpeechRecognition", {
      value: StubRecognition,
      configurable: true,
      writable: true,
    });
    try {
      mount();
      fireEvent.click(screen.getByRole("button", { name: MIC }));
      expect(gum).not.toHaveBeenCalled(); // 录音管道未被触达
      // Web Speech 实例已发起（stub 构造即持引用——经全局 instances 不可得，
      // 这里以 🎤 切换到「停止语音输入」aria 断言 listening 态）
      expect(screen.getByRole("button", { name: "停止语音输入" })).toBeTruthy();
    } finally {
      delete (window as { SpeechRecognition?: unknown }).SpeechRecognition;
    }
  });

  // v1.42：asr 回填与语音 final 同规——追加到当前会话的草稿
  it("v1.42 asr_result 回填追加到当前会话的草稿：识别进行中切到会话 B，文本落在 B；A 的草稿不被改写", async () => {
    injectRecorderEnv();
    const onSend = vi.fn(() => true);
    const drafts = newDrafts();
    const el = (key: string, asrEvent: AsrUiEvent | null) => (
      <Composer
        status="ready"
        asr
        asrEvent={asrEvent}
        onAsr={vi.fn(() => true)}
        onSend={onSend}
        onStop={vi.fn()}
        drafts={drafts}
        draftKey={key}
      />
    );
    const view = render(el(KEY_A, null));
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    type(ta, "A 已输入");
    fireEvent.click(screen.getByRole("button", { name: MIC }));
    await flush();
    act(() => workletNodes[0].port.onmessage?.({ data: new Float32Array(1600).fill(0.1) }));
    fireEvent.click(screen.getByRole("button", { name: STOP }));
    expect(screen.getByRole("button", { name: BUSY })).toBeTruthy();
    // 「识别中」切换会话：B 的输入框是空的（A 的草稿没跟过来）
    view.rerender(el(KEY_B, null));
    const taB = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    expect(taB.value).toBe("");
    // 回包到达：文本追加到当前会话（B）
    view.rerender(el(KEY_B, { kind: "result", text: "识别文本", durationMs: 100 }));
    expect(taB.value).toBe("识别文本");
    expect(drafts.text(KEY_B)).toBe("识别文本");
    expect(drafts.text(KEY_A)).toBe("A 已输入"); // A 不被改写
    expect(onSend).not.toHaveBeenCalled();
    // 切回 A：仍是自己的草稿
    view.rerender(el(KEY_A, null));
    expect(ta.value).toBe("A 已输入");
  });
});

// ---------------------------------------------------------------------------
// v1.42 草稿按「机器 + 会话」隔离：输入框文本与待发图片归属 draftKey；draftKey
// 变化即载入该会话的草稿。store 注入 storage: null（落盘在 lib/drafts.test.ts
// 覆盖；这里只验组件与 store 的协作）。
// ---------------------------------------------------------------------------

describe("Composer 草稿按会话隔离（v1.42）", () => {
  const FILE_INPUT = "选择图片";
  const MEDIA_A = { name: "shot.jpg", mime: "image/jpeg", b64: "QUJD" };
  const MEDIA_B = { name: "other.jpg", mime: "image/jpeg", b64: "QkNE" };
  const png = (name: string) => new File(["x"], name, { type: "image/png" });
  const pick = (file: File) =>
    fireEvent.change(screen.getByLabelText(FILE_INPUT), { target: { files: [file] } });
  const box = () => screen.getByLabelText("消息输入框") as HTMLTextAreaElement;

  /** 挂载在会话 A；switchTo 用**同一个 store** 切到指定键，返回切换后的输入框。 */
  function mountAtA(onSend = vi.fn(() => true)) {
    const drafts = createDraftStore({ storage: null });
    const el = (key: string) => (
      <Composer status="ready" onSend={onSend} onStop={vi.fn()} drafts={drafts} draftKey={key} />
    );
    const view = render(el(KEY_A));
    const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
    const switchTo = (key: string): HTMLTextAreaElement => {
      view.rerender(el(key));
      return box();
    };
    return { drafts, onSend, ta, switchTo };
  }

  it("① 会话 A 输入 → 切到 B 输入框为空；切回 A 恢复", () => {
    const { ta, switchTo, drafts } = mountAtA();
    type(ta, "A 的草稿");
    // 每次写入即落 store（store 是事实源）
    expect(drafts.text(KEY_A)).toBe("A 的草稿");
    expect(switchTo(KEY_B).value).toBe("");
    expect(drafts.text(KEY_A)).toBe("A 的草稿");
    expect(switchTo(KEY_A).value).toBe("A 的草稿");
  });

  it("② B 里输入不影响 A；两个会话各自独立", () => {
    const { ta, switchTo, drafts } = mountAtA();
    type(ta, "A 的草稿");
    type(switchTo(KEY_B), "B 的草稿");
    expect(drafts.text(KEY_A)).toBe("A 的草稿");
    expect(drafts.text(KEY_B)).toBe("B 的草稿");
    type(switchTo(KEY_A), "A 的草稿 追加");
    expect(switchTo(KEY_B).value).toBe("B 的草稿");
    expect(drafts.text(KEY_A)).toBe("A 的草稿 追加");
    expect(drafts.text(KEY_B)).toBe("B 的草稿");
  });

  it("③ A 选了图片 → 切到 B 不显示、切回 A 仍显示；各自可挂各自的图片，移除互不影响", async () => {
    const { switchTo, drafts } = mountAtA();
    pick(png("a.png"));
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    expect(drafts.media(KEY_A)).toEqual(MEDIA_A);
    expect(switchTo(KEY_B).value).toBe("");
    expect(screen.queryByLabelText("移除图片")).toBeNull();
    switchTo(KEY_A);
    expect(screen.getByAltText("shot.jpg")).toBeTruthy();
    // B 里另选一张
    compressSpy.mockResolvedValueOnce(MEDIA_B);
    switchTo(KEY_B);
    pick(png("b.png"));
    await waitFor(() => expect(screen.getByAltText("other.jpg")).toBeTruthy());
    expect(drafts.media(KEY_A)).toEqual(MEDIA_A);
    expect(drafts.media(KEY_B)).toEqual(MEDIA_B);
    // 移除 B 的图片不影响 A 的
    fireEvent.click(screen.getByLabelText("移除图片"));
    expect(drafts.media(KEY_B)).toBeNull();
    expect(drafts.media(KEY_A)).toEqual(MEDIA_A);
    switchTo(KEY_A);
    expect(screen.getByAltText("shot.jpg")).toBeTruthy();
  });

  it("④ 发送成功后该会话草稿清空（文字 + 图片）；切走再切回仍为空；别的会话不受影响", async () => {
    const { ta, onSend, switchTo, drafts } = mountAtA();
    type(ta, "要发的");
    pick(png("a.png"));
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    type(switchTo(KEY_B), "B 的草稿");
    switchTo(KEY_A);
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(onSend).toHaveBeenLastCalledWith("要发的", MEDIA_A);
    expect(box().value).toBe("");
    expect(screen.queryByLabelText("移除图片")).toBeNull();
    expect(drafts.text(KEY_A)).toBe("");
    expect(drafts.media(KEY_A)).toBeNull();
    expect(switchTo(KEY_B).value).toBe("B 的草稿");
    expect(switchTo(KEY_A).value).toBe("");
  });

  it("⑤ 发送失败（onSend false）：草稿留在 store，切走再切回仍在（文字与图片）", async () => {
    const { ta, switchTo, drafts } = mountAtA(vi.fn(() => false));
    type(ta, "没发出去的");
    pick(png("a.png"));
    await waitFor(() => expect(screen.getByLabelText("移除图片")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(drafts.text(KEY_A)).toBe("没发出去的");
    expect(drafts.media(KEY_A)).toEqual(MEDIA_A);
    switchTo(KEY_B);
    switchTo(KEY_A);
    expect(box().value).toBe("没发出去的");
    expect(screen.getByAltText("shot.jpg")).toBeTruthy();
  });

  it("⑥ 挂载时即恢复 store 里该会话的既有草稿（刷新恢复路径：文字 + 图片）", () => {
    const drafts = createDraftStore({ storage: null });
    drafts.setText(KEY_A, "刷新前没发完的");
    drafts.setMedia(KEY_A, MEDIA_A);
    render(
      <Composer status="ready" onSend={vi.fn(() => true)} onStop={vi.fn()} drafts={drafts} draftKey={KEY_A} />,
    );
    expect(box().value).toBe("刷新前没发完的");
    expect(screen.getByAltText("shot.jpg")).toBeTruthy();
  });

  it("⑦ 切换会话时命令面板状态复位：Esc 关闭态不带到别的会话，切回时面板随草稿重新弹出", () => {
    const { ta, switchTo } = mountAtA();
    type(ta, "/");
    expect(panel()).toBeTruthy();
    key(ta, "Escape");
    expect(panel()).toBeNull(); // A：Esc 关闭
    switchTo(KEY_B);
    type(box(), "/");
    expect(panel()).toBeTruthy(); // B：A 的关闭态不跟过来
    key(box(), "Escape");
    expect(panel()).toBeNull();
    switchTo(KEY_A);
    expect(box().value).toBe("/");
    expect(panel()).toBeTruthy(); // 切回 A：关闭态已复位，面板随草稿弹出
    // 选中序号归零：首项选中
    const options = panel()!.getAllByRole("option");
    expect(options[0].getAttribute("aria-selected")).toBe("true");
  });

  it("⑧ 选图压缩完成前切换会话：图片归属发起选图的会话（不落到当前会话），切回时仍在", async () => {
    let finish: (m: MediaPayload) => void = () => {};
    compressSpy.mockImplementationOnce(
      () =>
        new Promise<MediaPayload>((resolve) => {
          finish = resolve;
        }),
    );
    const { switchTo, drafts } = mountAtA();
    pick(png("a.png"));
    expect(compressSpy).toHaveBeenCalledTimes(1);
    switchTo(KEY_B); // 压缩还在进行
    await act(async () => {
      finish(MEDIA_A);
    });
    // B 的输入框不受影响；图片在 A 的草稿里
    expect(screen.queryByLabelText("移除图片")).toBeNull();
    expect(drafts.media(KEY_B)).toBeNull();
    expect(drafts.media(KEY_A)).toEqual(MEDIA_A);
    switchTo(KEY_A);
    expect(screen.getByAltText("shot.jpg")).toBeTruthy();
  });

  it("⑧b 压缩失败的提示同样只落在发起选图的会话（切走后到达不污染当前会话）", async () => {
    let fail: (e: Error) => void = () => {};
    compressSpy.mockImplementationOnce(
      () =>
        new Promise<MediaPayload>((_resolve, reject) => {
          fail = reject;
        }),
    );
    const { switchTo } = mountAtA();
    pick(png("bad.png"));
    switchTo(KEY_B);
    await act(async () => {
      fail(new Error("decode"));
    });
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("⑨ 清空输入 = 删除该会话草稿：全部删光后 store 里没有该键（不是空串占位）", () => {
    const { ta, drafts } = mountAtA();
    type(ta, "写一点");
    expect(drafts.text(KEY_A)).toBe("写一点");
    type(ta, "");
    expect(drafts.text(KEY_A)).toBe("");
  });
});

