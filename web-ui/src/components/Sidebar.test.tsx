// Sidebar「外观」三段主题开关：三选项渲染（跟随系统/浅色/深色）、当前偏好
// 高亮（aria-pressed）、点击回调按选项回传 ThemePref。其余区块（会话列表/
// 机器切换器等）由 App.test 端到端覆盖，此处仅挂最小 props。
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Sidebar, type ConvItem } from "./Sidebar";
import type { ThemePref } from "../lib/theme";
import type { MachineWindows, WindowInfo, WindowStatus } from "../lib/protocol";

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
// v1.41.4 左滑删除：会话项左滑（横向位移占优且超方向判定阈值）跟手露出删除
// 按钮，点击按钮触发 onConvDelete（App 级确认弹层入口）；松手过吸附阈值吸附
// 打开、否则回弹关闭并吞 click；纵向占优放行滚动（click 照常点选）；打开态
// 点本体仅收起；一次只开一项；桌面 hover 小图标兜底；未传 onConvDelete 不启用
// ---------------------------------------------------------------------------

describe("Sidebar 会话项左滑删除（v1.41.4）", () => {
  const CONVS: ConvItem[] = [
    { convId: "web:default", preview: "", at: 0, unread: 0 },
    { convId: "web:ghost", title: "幽灵会话", preview: "误触发的会话", at: 1, unread: 0 },
  ];

  /** jsdom 无 PointerEvent 构造器（fireEvent 的 init 对象不会落到事件上）——
   *  手搓 Event 并 Object.assign 补齐 pointerType/clientX/clientY，对齐真实
   *  浏览器事件形态。y 缺省 20（纵向不动）。 */
  function pointer(
    type: "pointerdown" | "pointermove" | "pointerup",
    x: number,
    y: number,
  ) {
    const ev = new Event(type, { bubbles: true, cancelable: true, composed: true });
    Object.assign(ev, { pointerType: "touch", button: 0, clientX: x, clientY: y });
    return ev;
  }
  const down = (el: HTMLElement, x: number, y = 20) =>
    fireEvent(el, pointer("pointerdown", x, y));
  const move = (el: HTMLElement, x: number, y = 20) =>
    fireEvent(el, pointer("pointermove", x, y));
  const up = (el: HTMLElement, x: number, y = 20) =>
    fireEvent(el, pointer("pointerup", x, y));

  /** 左滑序列：按下 → 半程（触发方向判定）→ 终点 → 松手（对齐真机事件流）。 */
  function swipe(item: HTMLElement, fromX: number, toX: number, y = 20) {
    down(item, fromX, y);
    move(item, (fromX + toX) / 2, y);
    move(item, toX, y);
    up(item, toX, y);
  }

  function mount(onDelete?: (conv: string) => void, onSelect: (conv: string) => void = () => {}) {
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
        onConvDelete={onDelete}
      />,
    );
    return screen.getByRole("button", { name: /幽灵会话/ });
  }

  it("左滑超吸附阈值露出删除按钮；点击删除按钮触发 onConvDelete（带 convId）并收起", () => {
    const onDelete = vi.fn();
    const onSelect = vi.fn();
    const item = mount(onDelete, onSelect);
    expect(screen.queryByRole("button", { name: "删除" })).toBeNull();
    swipe(item, 200, 140); // dx = -60 < -40 → 吸附打开
    fireEvent.click(screen.getByRole("button", { name: "删除" }));
    expect(onDelete).toHaveBeenCalledTimes(1);
    expect(onDelete).toHaveBeenCalledWith("web:ghost");
    // 删除入口触发后收起（露出按钮卸载），且不切会话
    expect(screen.queryByRole("button", { name: "删除" })).toBeNull();
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("位移不足吸附阈值：回弹关闭且吞 click（不切会话）；微小抖动后点选照常", () => {
    const onDelete = vi.fn();
    const onSelect = vi.fn();
    const item = mount(onDelete, onSelect);
    // 左滑但不过半（dx = -15 ≥ -40）→ 回弹关闭，滑动语义吞 click
    swipe(item, 200, 185);
    expect(screen.queryByRole("button", { name: "删除" })).toBeNull();
    fireEvent.click(item);
    expect(onSelect).not.toHaveBeenCalled();
    // 按下 + 微小抖动（未超方向判定阈值）+ 抬起 → 正常点选
    down(item, 200);
    move(item, 196); // |dx| = 4 < 10
    up(item, 196);
    fireEvent.click(item);
    expect(onSelect).toHaveBeenCalledTimes(1);
  });

  it("纵向占优位移放行垂直滚动：不认领手势，click 照常点选", () => {
    const onDelete = vi.fn();
    const onSelect = vi.fn();
    const item = mount(onDelete, onSelect);
    down(item, 200);
    move(item, 202, 40); // |dx| = 2 < |dy| = 20 → 纵向占优，放弃手势
    up(item, 202, 40);
    fireEvent.click(item);
    expect(onDelete).not.toHaveBeenCalled();
    expect(onSelect).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("button", { name: "删除" })).toBeNull();
  });

  it("打开态点项本体仅收起（不切换）；一次只开一项（开新项自动收旧项）", () => {
    const onDelete = vi.fn();
    const onSelect = vi.fn();
    const ghost = mount(onDelete, onSelect);
    swipe(ghost, 200, 140);
    expect(screen.getAllByRole("button", { name: "删除" })).toHaveLength(1);
    // 真实事件流：点本体前先 down/up（down 重置吞 click 标记）→ click 收起
    down(ghost, 200);
    up(ghost, 200);
    fireEvent.click(ghost);
    expect(screen.queryByRole("button", { name: "删除" })).toBeNull();
    expect(onSelect).not.toHaveBeenCalled();
    // ghost 再开 → 左滑 default：ghost 自动收起，同时仅一项开着
    swipe(ghost, 200, 140);
    const def = screen.getByRole("button", { name: /default/ });
    swipe(def, 200, 140);
    expect(screen.getAllByRole("button", { name: "删除" })).toHaveLength(1);
  });

  it("桌面 hover 删除图标兜底：点击触发 onConvDelete；打开态不渲染（与露出按钮互斥）", () => {
    const onDelete = vi.fn();
    const ghost = mount(onDelete);
    // 图标挂在所属条目的手势容器内，按容器定位（jsdom 无法断言 md:/opacity
    // CSS 的显隐，渲染存在性与互斥可测）
    const ghostBox = within(ghost.parentElement as HTMLElement);
    fireEvent.click(ghostBox.getByRole("button", { name: "删除会话" }));
    expect(onDelete).toHaveBeenCalledTimes(1);
    expect(onDelete).toHaveBeenCalledWith("web:ghost");
    // 打开态不渲染本条目图标（与露出的删除按钮互斥）
    swipe(ghost, 200, 140);
    expect(ghostBox.queryByRole("button", { name: "删除会话" })).toBeNull();
    expect(screen.getByRole("button", { name: "删除" })).toBeTruthy();
  });

  it("未传 onConvDelete：左滑无效（无删除入口），click 照常点选", () => {
    const onSelect = vi.fn();
    const item = mount(undefined, onSelect);
    swipe(item, 200, 100);
    expect(screen.queryByRole("button", { name: "删除" })).toBeNull();
    expect(screen.queryByRole("button", { name: "删除会话" })).toBeNull();
    fireEvent.click(item);
    expect(onSelect).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// v1.41.7 侧栏底部运行版本号：`v{__APP_VERSION__}`（构建期 vite define 注入，
// vitest 经 test.define 同样注入——注入缺失时文本为 "vundefined"，本用例即红）。
// 纯展示：非按钮、无交互。
// ---------------------------------------------------------------------------

describe("Sidebar 运行版本号（v1.41.7）", () => {
  it("底部显示 v{__APP_VERSION__}（纯展示，全树仅一处）", () => {
    mountSidebar();
    const version = screen.getByText(`v${__APP_VERSION__}`);
    expect(version.tagName).toBe("DIV");
    expect(screen.getAllByText(/^v\d+\.\d+\.\d+$/)).toEqual([version]);
  });
});

// ---------------------------------------------------------------------------
// v1.43「本地窗口」区块：机器切换器之下、会话列表之上。windows 为 undefined
// （旧网关 / 未启用 peer 通道）整块不渲染；items 为空显示灰字；每行 = 状态点 +
// 窗口名 + 第二行状态文字；星标是独立的按钮（aria-pressed），点星标不触发行点击。
// ---------------------------------------------------------------------------

describe("Sidebar 本地窗口区块（v1.43）", () => {
  const NOW = 1_795_000_000_000;
  const MIN = 60_000;
  const mk = (name: string, status: WindowStatus, extra: Partial<WindowInfo> = {}): WindowInfo => ({
    name,
    status,
    cwd: `/Users/demo/work/${name}`,
    watched: false,
    statusUpdatedAt: NOW - 3 * MIN,
    ...extra,
  });
  const IDLE = mk("jormungandr-f8", "idle");
  const WAITING = mk("imagent-a7", "waiting", {
    waitingFor: "dialog open",
    watched: true,
    statusUpdatedAt: NOW - 2 * MIN,
  });
  const BUSY = mk("runner", "busy", { statusUpdatedAt: undefined });

  /** 挂载侧栏（多机模式，便于断言区块位置）。时钟取假时钟 NOW：相对时间文案
   *  不随真实时间漂移。 */
  function mount(windows?: MachineWindows) {
    const handlers = { onSelectWindow: vi.fn(), onToggleWindowWatch: vi.fn() };
    render(
      <Sidebar
        convs={NO_CONVS}
        current="web:default"
        machineOnline={true}
        machines={["home-mac"]}
        machinesKnown={true}
        currentMachine="home-mac"
        pushEnabled={false}
        pushAvailable={false}
        pushBusy={false}
        themePref="system"
        onThemeChange={() => {}}
        onSelect={() => {}}
        onSelectMachine={() => {}}
        onCreate={() => {}}
        windows={windows}
        onSelectWindow={handlers.onSelectWindow}
        onToggleWindowWatch={handlers.onToggleWindowWatch}
      />,
    );
    return handlers;
  }
  const section = (): HTMLElement | null => screen.queryByRole("region", { name: "本地窗口" });
  const rowOf = (name: string): HTMLElement =>
    within(section()!).getByRole("button", { name: new RegExp(`^窗口 ${name}：`) }) as HTMLElement;
  const starOf = (name: string): HTMLElement =>
    within(section()!).getByRole("button", { name: new RegExp(`^(?:取消)?关注窗口 ${name}$`) }) as HTMLElement;
  /** 行内状态点（圆点，aria-hidden）。 */
  const dotOf = (row: HTMLElement): string => (row.querySelector(".rounded-full") as HTMLElement).className;
  /** 行内第二行状态文字：状态文字（waiting 带括号里的原因）+「 · 」+ 相对时间。 */
  const lineOf = (row: HTMLElement): string => (row.querySelector(".text-faint") as HTMLElement).textContent ?? "";
  /** 区块头的汇总（「N 待确认」/「N 运行中」，都没有 → null）。按文案定位：
   *  行内状态文字是「运行中」或「运行中 · N 分钟前」，不以数字开头，不会撞上。 */
  const summary = (): { text: string; className: string } | null => {
    const sec = section();
    const el = sec
      ? Array.from(sec.querySelectorAll("span")).find((s) => /^\d+ (?:待确认|运行中)$/.test(s.textContent ?? ""))
      : undefined;
    return el ? { text: el.textContent ?? "", className: el.className } : null;
  };
  const follows = (a: Node, b: Node): boolean =>
    (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("windows 缺省（旧网关 / 未启用 peer 通道）→ 整块不渲染：没有名为「本地窗口」的 region，也没有钩子状态行", () => {
    mount();
    expect(section()).toBeNull();
    expect(screen.queryByText(/结论自动上报/)).toBeNull();
  });

  it("位置：机器列表之后、会话列表之前；空快照 → 一行灰字「没有在线的本地窗口」，区块头无汇总", () => {
    mount({ hookInstalled: true, items: [] });
    const sec = section();
    expect(sec).toBeTruthy();
    expect(follows(screen.getByRole("listbox", { name: "机器列表" }), sec!)).toBe(true);
    expect(follows(sec!, screen.getByRole("navigation", { name: "会话列表" }))).toBe(true);
    expect(within(sec!).getByText("没有在线的本地窗口")).toBeTruthy();
    expect(summary()).toBeNull();
  });

  it("状态点与状态文字：busy 运行中（唯一的呼吸点）/ waiting 等待确认（括号里是原因）/ idle 空闲 / unknown 未知；相对时间跟在状态后", () => {
    mount({ hookInstalled: true, items: [IDLE, WAITING, BUSY, mk("ghost", "unknown", { statusUpdatedAt: undefined })] });
    const idle = rowOf("jormungandr-f8");
    expect(lineOf(idle)).toBe("空闲 · 3 分钟前");
    expect(dotOf(idle)).toContain("bg-slate-500");
    expect(dotOf(idle)).not.toContain("animate-pulse");
    const waiting = rowOf("imagent-a7");
    // 分隔符恒为「 · 」（括号已自带留白也不例外——任务书正文公式的写法）
    expect(lineOf(waiting)).toBe("等待确认（等你确认） · 2 分钟前");
    expect(dotOf(waiting)).toContain("bg-amber-400");
    const busy = rowOf("runner");
    // 没有 statusUpdatedAt → 不出相对时间，也不留悬空的「 · 」
    expect(lineOf(busy)).toBe("运行中");
    expect(dotOf(busy)).toContain("bg-emerald-400");
    expect(dotOf(busy)).toContain("animate-pulse");
    const unknown = rowOf("ghost");
    expect(lineOf(unknown)).toBe("未知");
    expect(dotOf(unknown)).toContain("bg-slate-600");
    expect(section()!.querySelectorAll(".animate-pulse")).toHaveLength(1);
    // 行的可访问名 =「窗口 <名>：<状态文字>」，waiting 追加「，<原因>」
    expect(idle.getAttribute("aria-label")).toBe("窗口 jormungandr-f8：空闲");
    expect(waiting.getAttribute("aria-label")).toBe("窗口 imagent-a7：等待确认，等你确认");
    expect(busy.getAttribute("aria-label")).toBe("窗口 runner：运行中");
  });

  it("waiting 的原因文案：四种已知原因 → 人话；没见过的原因原样显示；没有原因时不出现括号", () => {
    mount({
      hookInstalled: true,
      items: [
        mk("w1", "waiting", { waitingFor: "dialog open" }),
        mk("w2", "waiting", { waitingFor: "input needed" }),
        mk("w3", "waiting", { waitingFor: "sandbox request" }),
        mk("w4", "waiting", { waitingFor: "goal proposal" }),
        mk("w5", "waiting", { waitingFor: "mcp reauth" }),
        mk("w6", "waiting"),
        mk("w7", "waiting", { waitingFor: "" }),
      ],
    });
    expect(lineOf(rowOf("w1"))).toContain("等待确认（等你确认）");
    expect(lineOf(rowOf("w2"))).toContain("等待确认（等你输入）");
    expect(lineOf(rowOf("w3"))).toContain("等待确认（等沙箱授权）");
    expect(lineOf(rowOf("w4"))).toContain("等待确认（等你定目标）");
    expect(lineOf(rowOf("w5"))).toContain("等待确认（mcp reauth）");
    expect(lineOf(rowOf("w6"))).toBe("等待确认 · 3 分钟前");
    expect(lineOf(rowOf("w7"))).toBe("等待确认 · 3 分钟前");
    expect(rowOf("w1").getAttribute("aria-label")).toBe("窗口 w1：等待确认，等你确认");
    expect(rowOf("w6").getAttribute("aria-label")).toBe("窗口 w6：等待确认");
  });

  it("星标：aria-label「关注窗口 X」/「取消关注窗口 X」与 aria-pressed 跟随 watched；点击星标只触发 onToggleWindowWatch(name)，不触发行点击", () => {
    const h = mount({ hookInstalled: true, items: [IDLE, WAITING] });
    expect(starOf("jormungandr-f8").getAttribute("aria-label")).toBe("关注窗口 jormungandr-f8");
    expect(starOf("jormungandr-f8").getAttribute("aria-pressed")).toBe("false");
    expect(starOf("imagent-a7").getAttribute("aria-label")).toBe("取消关注窗口 imagent-a7");
    expect(starOf("imagent-a7").getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(starOf("jormungandr-f8"));
    fireEvent.click(starOf("imagent-a7"));
    expect(h.onToggleWindowWatch.mock.calls).toEqual([["jormungandr-f8"], ["imagent-a7"]]);
    expect(h.onSelectWindow).not.toHaveBeenCalled();
  });

  it("点击行本体 → onSelectWindow(窗口名)；不触发关注切换；行的 title 是工作目录（名字不含路径，靠它区分窗口）", () => {
    const h = mount({ hookInstalled: true, items: [IDLE] });
    const row = rowOf("jormungandr-f8");
    expect(row.getAttribute("title")).toBe("/Users/demo/work/jormungandr-f8");
    fireEvent.click(row);
    expect(h.onSelectWindow).toHaveBeenCalledWith("jormungandr-f8");
    expect(h.onToggleWindowWatch).not.toHaveBeenCalled();
  });

  it("区块头汇总：有 waiting 显示「N 待确认」（优先级最高，同时有 busy 也不显示运行中）；只有 busy 显示「N 运行中」；都没有不显示", () => {
    const b2 = mk("runner-2", "busy");
    mount({ hookInstalled: true, items: [WAITING, BUSY, b2] });
    expect(summary()!.text).toBe("1 待确认");
    expect(summary()!.className).toContain("text-amber");
    cleanup();
    mount({ hookInstalled: true, items: [BUSY, b2, IDLE] });
    expect(summary()!.text).toBe("2 运行中");
    expect(summary()!.className).toContain("text-emerald");
    cleanup();
    mount({ hookInstalled: true, items: [IDLE] });
    expect(summary()).toBeNull();
  });

  it("钩子状态行：hookInstalled true →「结论自动上报：已开启」/ false →「结论自动上报：未安装」；title 给出安装命令的提示", () => {
    mount({ hookInstalled: true, items: [] });
    expect(within(section()!).getByText("结论自动上报：已开启")).toBeTruthy();
    cleanup();
    mount({ hookInstalled: false, items: [] });
    const line = within(section()!).getByText("结论自动上报：未安装");
    expect(line.getAttribute("title")).toBe(
      "在电脑上运行 imagent peer hook install 后，每个窗口每一轮的结论会自动同步到对话里",
    );
  });

  it("相对时间按分钟刷新：1 分钟后（useNow 的 tick）文案从「刚刚」变为「1 分钟前」", () => {
    mount({ hookInstalled: true, items: [mk("fresh", "idle", { statusUpdatedAt: NOW })] });
    expect(lineOf(rowOf("fresh"))).toBe("空闲 · 刚刚");
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(lineOf(rowOf("fresh"))).toBe("空闲 · 1 分钟前");
  });
});
