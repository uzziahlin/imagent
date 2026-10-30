// v1.7 Composer：命令面板（过滤/键盘/点选/填充/Esc/模型 chips）+ 图片附件
// （选图→压缩→预览→随发）。canvas 主路径（compressImage）经 vi.mock 替换——
// jsdom 无 canvas（纯逻辑阈值/文件名在 lib/imageCompress.test.ts 覆盖）。
// v1.8 模型菜单动态化：/model chips 取快照 models 段（modelChips 三层回退）
// + 行尾当前模型文本（currentModelText）+ 有映射 target 的 chip title 提示。
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import type { MediaPayload, ModelsInfo } from "../lib/protocol";

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

function mount(onSend = vi.fn(() => true)) {
  render(<Composer status="ready" onSend={onSend} onStop={vi.fn()} />);
  const ta = screen.getByLabelText("消息输入框") as HTMLTextAreaElement;
  return { ta, onSend };
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
      <Composer status="ready" models={models} onSend={vi.fn(() => true)} onStop={vi.fn()} />,
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
