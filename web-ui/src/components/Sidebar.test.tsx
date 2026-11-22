// Sidebar「外观」三段主题开关：三选项渲染（跟随系统/浅色/深色）、当前偏好
// 高亮（aria-pressed）、点击回调按选项回传 ThemePref。其余区块（会话列表/
// 机器切换器等）由 App.test 端到端覆盖，此处仅挂最小 props。
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Sidebar, type ConvItem } from "./Sidebar";
import type { ThemePref } from "../lib/theme";

afterEach(cleanup);

const NO_CONVS: ConvItem[] = [];

function mountSidebar(themePref: ThemePref = "system") {
  const onThemeChange = vi.fn();
  render(
    <Sidebar
      convs={NO_CONVS}
      current="web:default"
      machineOnline={true}
      machines={[]}
      machinesKnown={false}
      currentMachine="default"
      pushEnabled={false}
      pushAvailable={false}
      pushBusy={false}
      themePref={themePref}
      onThemeChange={onThemeChange}
      onSelect={() => {}}
      onSelectMachine={() => {}}
      onCreate={() => {}}
    />,
  );
  return { onThemeChange };
}

const themeButtons = () =>
  within(screen.getByRole("group", { name: "外观" })).getAllByRole("button") as HTMLButtonElement[];

describe("Sidebar 外观三段开关", () => {
  it("渲染三选项（跟随系统/浅色/深色）；当前偏好高亮（aria-pressed）", () => {
    mountSidebar("dark");
    const btns = themeButtons();
    expect(btns.map((b) => b.textContent)).toEqual(["跟随系统", "浅色", "深色"]);
    expect(btns.map((b) => b.getAttribute("aria-pressed"))).toEqual(["false", "false", "true"]);
  });

  it("点击按选项回传 ThemePref（跟随系统/浅色/深色）", () => {
    const { onThemeChange } = mountSidebar();
    fireEvent.click(themeButtons()[2]); // 深色
    fireEvent.click(themeButtons()[1]); // 浅色
    fireEvent.click(themeButtons()[0]); // 跟随系统
    expect(onThemeChange.mock.calls).toEqual([["dark"], ["light"], ["system"]]);
  });

  it("按可访问名定位（验证 label 即选项文案）", () => {
    mountSidebar("light");
    expect(
      (within(screen.getByRole("group", { name: "外观" })).getByRole("button", {
        name: "浅色",
      }) as HTMLButtonElement).getAttribute("aria-pressed"),
    ).toBe("true");
  });
});

// ---------------------------------------------------------------------------
// v1.37 会话项「运行中」状态：running 会话项标题行左侧渲染呼吸动画点
// （emerald + animate-pulse，形态随状态条在线点；title/aria 提示「运行中」）
// ---------------------------------------------------------------------------

describe("Sidebar 会话项运行态点（v1.37）", () => {
  const CONVS: ConvItem[] = [
    { convId: "web:default", preview: "", at: 0, unread: 0 },
    { convId: "web:busy", title: "跑任务", preview: "生成中…", at: 1, unread: 0, running: true },
  ];

  it("running 会话渲染运行点（落在该条目内）；非 running 条目不渲染", () => {
    render(
      <Sidebar
        convs={CONVS}
        current="web:default"
        machineOnline={true}
        machines={[]}
        machinesKnown={false}
        currentMachine="default"
        pushEnabled={false}
        pushAvailable={false}
        pushBusy={false}
        themePref="system"
        onSelect={() => {}}
        onSelectMachine={() => {}}
        onCreate={() => {}}
      />,
    );
    // 仅 busy（running: true）条目渲染运行点，且点落在该条目内部；
    // 非 running 条目（default）不渲染——全树仅一处
    const dot = screen.getByTitle("运行中");
    const busyItem = screen.getByRole("button", { name: /跑任务/ });
    expect(busyItem.contains(dot)).toBe(true);
    expect(dot.getAttribute("aria-label")).toBe("运行中");
    const defaultItem = screen.getByRole("button", { name: /default/ });
    expect(defaultItem.contains(dot)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// v1.41.3 长按删除：会话项长按 500ms 触发 onConvDelete（滚动手势/提前松手
// 取消；触发后吞掉紧随的 click 不切会话；未传 onConvDelete 不启用）
// ---------------------------------------------------------------------------

describe("Sidebar 会话项长按删除（v1.41.3）", () => {
  const CONVS: ConvItem[] = [
    { convId: "web:default", preview: "", at: 0, unread: 0 },
    { convId: "web:ghost", title: "幽灵会话", preview: "误触发的会话", at: 1, unread: 0 },
  ];

  /** jsdom 的 PointerEvent 不带 pointerType/clientX——fireEvent.pointerDown
   *  走 init 对象补齐（handler 读 pointerType/clientX/clientY）。 */
  function press(item: HTMLElement, opts: { x?: number; y?: number } = {}) {
    fireEvent.pointerDown(item, {
      pointerType: "touch",
      button: 0,
      clientX: opts.x ?? 10,
      clientY: opts.y ?? 10,
    });
  }

  function mount(onConvDelete: (conv: string) => void, onSelect: (conv: string) => void) {
    render(
      <Sidebar
        convs={CONVS}
        current="web:default"
        machineOnline={true}
        machines={[]}
        machinesKnown={false}
        currentMachine="default"
        pushEnabled={false}
        pushAvailable={false}
        pushBusy={false}
        themePref="system"
        onSelect={onSelect}
        onSelectMachine={() => {}}
        onCreate={() => {}}
        onConvDelete={onConvDelete}
      />,
    );
    return screen.getByRole("button", { name: /幽灵会话/ });
  }

  it("长按 500ms 触发 onConvDelete（带 convId）；触发后吞 click（不切会话）", () => {
    vi.useFakeTimers();
    try {
      const onDelete = vi.fn();
      const onSelect = vi.fn();
      const item = mount(onDelete, onSelect);
      press(item);
      fireEvent.click(item); // 500ms 内的 click 不受影响（正常点选）
      expect(onSelect).toHaveBeenCalledTimes(1);
      vi.advanceTimersByTime(500);
      expect(onDelete).toHaveBeenCalledTimes(1);
      expect(onDelete).toHaveBeenCalledWith("web:ghost");
      // 长按触发后浏览器仍派发 click——必须被吞（不切会话）
      fireEvent.click(item);
      expect(onSelect).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("提前松手 / 位移超容差 / pointer 离项：不触发（滚动手势与点选保护）", () => {
    vi.useFakeTimers();
    try {
      const onDelete = vi.fn();
      const onSelect = vi.fn();
      const item = mount(onDelete, onSelect);

      // 提前松手（<500ms）
      press(item);
      vi.advanceTimersByTime(200);
      fireEvent.pointerUp(item);
      vi.advanceTimersByTime(500);
      expect(onDelete).not.toHaveBeenCalled();

      // 滚动手势（按下后移动 >8px）
      press(item, { x: 10, y: 10 });
      fireEvent.pointerMove(item, { clientX: 30, clientY: 40 });
      vi.advanceTimersByTime(500);
      expect(onDelete).not.toHaveBeenCalled();
      fireEvent.pointerUp(item);
      fireEvent.click(item);
      expect(onSelect).toHaveBeenCalledTimes(1); // 滚动后的 click 照常点选

      // 指针离项（滑出条目）
      press(item);
      fireEvent.pointerLeave(item);
      vi.advanceTimersByTime(500);
      expect(onDelete).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("未传 onConvDelete：长按不触发（旧入口行为不变）", () => {
    vi.useFakeTimers();
    try {
      const onSelect = vi.fn();
      render(
        <Sidebar
          convs={CONVS}
          current="web:default"
          machineOnline={true}
          machines={[]}
          machinesKnown={false}
          currentMachine="default"
          pushEnabled={false}
          pushAvailable={false}
          pushBusy={false}
          themePref="system"
          onSelect={onSelect}
          onSelectMachine={() => {}}
          onCreate={() => {}}
        />,
      );
      const item = screen.getByRole("button", { name: /幽灵会话/ });
      press(item);
      vi.advanceTimersByTime(500);
      fireEvent.click(item);
      expect(onSelect).toHaveBeenCalledTimes(1); // 长按无效果，click 照常
    } finally {
      vi.useRealTimers();
    }
  });
});
