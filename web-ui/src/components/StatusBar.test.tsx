// StatusBar「窗口在等你」提示（v1.43）：有本地窗口卡在 waiting 时，状态条在会话名
// pill 之前出现一个 amber 按钮，点击打开会话抽屉。只在 windowsWaiting > 0 且两个
// props 都提供时出现——「运行中」的数量放侧栏区块头，不占状态条。
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { StatusBar } from "./StatusBar";

afterEach(cleanup);

type Props = Parameters<typeof StatusBar>[0];

const BASE: Omit<Props, "windowsWaiting" | "onOpenWindows"> = {
  status: "ready",
  machineOnline: true,
  convLabel: "web:default",
  onLogout: () => {},
  onReconnect: () => {},
  onOpenDrawer: () => {},
};

/** 会话名 pill（状态条里显示当前会话名的元素）。 */
const pill = (): HTMLElement => screen.getByText("web:default") as HTMLElement;
/** 「窗口在等你」提示按钮（未渲染 → null）。 */
const chip = (): HTMLElement | null =>
  screen.queryByRole("button", { name: /个本地窗口在等你确认，打开会话列表$/ });

describe("StatusBar 窗口待确认提示（v1.43）", () => {
  it("未提供 windowsWaiting / onOpenWindows：不出现提示（升级前的状态条不变）", () => {
    render(<StatusBar {...BASE} />);
    expect(chip()).toBeNull();
    expect(pill()).toBeTruthy();
  });

  it("windowsWaiting 为 0：不出现（没有窗口在等人就不占状态条）", () => {
    render(<StatusBar {...BASE} windowsWaiting={0} onOpenWindows={() => {}} />);
    expect(chip()).toBeNull();
  });

  it("两个 props 缺一不可：只给数量（无处可点）/ 只给回调（没有数量）都不渲染", () => {
    const first = render(<StatusBar {...BASE} windowsWaiting={2} />);
    expect(chip()).toBeNull();
    first.unmount();
    render(<StatusBar {...BASE} onOpenWindows={() => {}} />);
    expect(chip()).toBeNull();
  });

  it("有 1 个 waiting：出现「⏸ 1 个窗口待确认」，可访问名「有 1 个本地窗口在等你确认，打开会话列表」；点击只调 onOpenWindows", () => {
    const onOpenDrawer = vi.fn();
    const onOpenWindows = vi.fn();
    render(
      <StatusBar {...BASE} onOpenDrawer={onOpenDrawer} windowsWaiting={1} onOpenWindows={onOpenWindows} />,
    );
    const c = chip();
    expect(c).toBeTruthy();
    expect(c!.textContent).toBe("⏸ 1 个窗口待确认");
    expect(c!.getAttribute("aria-label")).toBe("有 1 个本地窗口在等你确认，打开会话列表");
    fireEvent.click(c!);
    expect(onOpenWindows).toHaveBeenCalledTimes(1);
    expect(onOpenDrawer).not.toHaveBeenCalled();
  });

  it("数量进入文案与可访问名（3 个）；与连接态无关（重连中同样出现）", () => {
    render(
      <StatusBar
        {...BASE}
        status="reconnecting"
        machineOnline={false}
        windowsWaiting={3}
        onOpenWindows={() => {}}
      />,
    );
    expect(chip()!.textContent).toBe("⏸ 3 个窗口待确认");
    expect(chip()!.getAttribute("aria-label")).toBe("有 3 个本地窗口在等你确认，打开会话列表");
    expect(screen.getByText("重连中")).toBeTruthy();
  });

  it("提示位于会话名 pill 之前（DOM 序），且排在 ☰ / 连接态之后", () => {
    render(<StatusBar {...BASE} windowsWaiting={1} onOpenWindows={() => {}} />);
    const header = screen.getByRole("banner");
    const menu = header.querySelector("button") as HTMLElement;
    const c = chip()!;
    expect(menu.getAttribute("aria-label")).toBe("打开会话列表");
    // compareDocumentPosition 的返回位掩码含 FOLLOWING = 后一个参数在文档序上位于前一个之后
    const follows = (a: Node, b: Node): boolean =>
      (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
    expect(follows(menu, c)).toBe(true);
    expect(follows(c, pill())).toBe(true);
  });
});
