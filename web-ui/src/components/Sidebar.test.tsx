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
