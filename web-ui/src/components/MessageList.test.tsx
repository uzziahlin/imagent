// MessageList 空态（v1.43 emptyHint）：窗口会话（peer:）首次打开还没有任何消息时，
// 用一段说明替代通用示例 prompt（示例是通用任务，发给一个本地窗口没有意义）。
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MessageList } from "./MessageList";
import type { StreamEntry } from "../lib/protocol";

afterEach(cleanup);

const HINT =
  "这是与本地 Claude Code 窗口「win-a」的对话。直接输入文字即可派活，窗口空闲时会自动开始处理；窗口的回信会出现在这里。";
const SAMPLES = ["看看这个项目结构", "帮我写个脚本", "解释一下最近的改动"];
const USER_ENTRY: StreamEntry = { kind: "user", key: "user:1", text: "你好" };

describe("MessageList 空态 emptyHint（v1.43）", () => {
  it("默认空态（无 emptyHint）：标题 + 键盘提示 + 三个示例 prompt（onSample 提供时），点击示例即发送", () => {
    const onSample = vi.fn();
    render(<MessageList entries={[]} onSample={onSample} />);
    expect(screen.getByText("向你的 Mac 发送第一条消息")).toBeTruthy();
    expect(screen.getByText("Enter 发送 · Shift+Enter 换行")).toBeTruthy();
    for (const s of SAMPLES) {
      expect(screen.getByRole("button", { name: s }), s).toBeTruthy();
    }
    fireEvent.click(screen.getByRole("button", { name: "帮我写个脚本" }));
    expect(onSample).toHaveBeenCalledWith("帮我写个脚本");
    expect(screen.queryByText(HINT)).toBeNull();
  });

  it("emptyHint 提供：显示说明文字，不渲染任何示例 prompt——即使 onSample 已提供；标题与键盘提示仍在", () => {
    const onSample = vi.fn();
    render(<MessageList entries={[]} onSample={onSample} emptyHint={HINT} />);
    expect(screen.getByText(HINT)).toBeTruthy();
    for (const s of SAMPLES) {
      expect(screen.queryByRole("button", { name: s }), s).toBeNull();
    }
    expect(screen.getByText("向你的 Mac 发送第一条消息")).toBeTruthy();
    expect(screen.getByText("Enter 发送 · Shift+Enter 换行")).toBeTruthy();
    expect(onSample).not.toHaveBeenCalled();
  });

  it("emptyHint 提供但会话已有消息：不显示说明（说明只属于空会话）", () => {
    render(<MessageList entries={[USER_ENTRY]} onSample={() => {}} emptyHint={HINT} />);
    expect(screen.queryByText(HINT)).toBeNull();
    expect(screen.getByText("你好")).toBeTruthy();
    expect(screen.queryByText("向你的 Mac 发送第一条消息")).toBeNull();
  });

  it("rerender：从空会话（显示说明）到有消息（说明消失）", () => {
    const { rerender } = render(<MessageList entries={[]} emptyHint={HINT} />);
    expect(screen.getByText(HINT)).toBeTruthy();
    rerender(<MessageList entries={[USER_ENTRY]} emptyHint={HINT} />);
    expect(screen.queryByText(HINT)).toBeNull();
    expect(screen.getByText("你好")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// v1.44 送达状态：用户气泡的四种形态。sent 与不带 ack（旧网关 / 镜像补回的条目）
// 必须与改动前逐字节一致——基线字面量是在改动 MessageList 之前，用当时的实现渲染
// { kind:"user", key, text:"你好" } 抓下来的 outerHTML。
// ---------------------------------------------------------------------------
describe("MessageList 用户气泡的送达状态（v1.44）", () => {
  type UserEntry = Extract<StreamEntry, { kind: "user" }>;
  /** 改动前（v1.43）用户气泡外层节点的 DOM 原样。 */
  const BASELINE_HTML =
    '<div class="flex flex-col items-end"><div class="max-w-[80%] rounded-2xl rounded-br-md bg-emerald-600 px-3 py-2 text-sm text-white shadow-sm"><p class="whitespace-pre-wrap break-words">你好</p></div></div>';
  /** 气泡本体的类名（四种状态共用；pending 多一个 opacity-70）。 */
  const BUBBLE_CLS =
    "max-w-[80%] rounded-2xl rounded-br-md bg-emerald-600 px-3 py-2 text-sm text-white shadow-sm";
  const echo = (extra: Partial<UserEntry> = {}): StreamEntry => ({
    kind: "user",
    key: "user:m_abc_00000000",
    text: "你好",
    ...extra,
  });
  /** 用户气泡的外层节点（flex 列：时间戳 / 气泡 / 状态行）。 */
  const wrapOf = (container: HTMLElement): HTMLElement =>
    container.querySelector(".bg-emerald-600")!.parentElement as HTMLElement;
  const bubbleOf = (container: HTMLElement): HTMLElement =>
    container.querySelector(".bg-emerald-600") as HTMLElement;

  it("sent 与不带 ack：DOM 与改动前逐字节一致（等于基线字面量）；网关镜像补回的条目（self）同样", () => {
    const plain = render(<MessageList entries={[echo()]} />);
    expect(wrapOf(plain.container).outerHTML).toBe(BASELINE_HTML);
    plain.unmount();
    const sent = render(<MessageList entries={[echo({ clientId: "m_abc_00000000", ack: "sent" })]} />);
    expect(wrapOf(sent.container).outerHTML).toBe(BASELINE_HTML);
    sent.unmount();
    const mirror = render(<MessageList entries={[echo({ key: "user:3", self: true })]} />);
    expect(wrapOf(mirror.container).outerHTML).toBe(BASELINE_HTML);
  });

  it("带时间戳的条目：sent 的 DOM 同样只是「时间戳 + 气泡」，没有多余节点", () => {
    const { container } = render(<MessageList entries={[echo({ ack: "sent", at: 1700000000000 })]} />);
    const wrap = wrapOf(container);
    expect(wrap.children).toHaveLength(2);
    expect(wrap.firstElementChild!.className).toBe("mb-0.5 pr-1 text-[11px] leading-4 text-ghost");
    expect(wrap.lastElementChild).toBe(bubbleOf(container));
    expect(wrap.textContent).not.toMatch(/发送中|未确认送达|未送达/);
  });

  it("pending：气泡多一个 opacity-70，右下一行小字「发送中」；没有重发按钮，气泡内部结构不变", () => {
    const onResend = vi.fn();
    const { container } = render(<MessageList entries={[echo({ ack: "pending" })]} onResend={onResend} />);
    const wrap = wrapOf(container);
    expect(wrap.children).toHaveLength(2);
    expect(bubbleOf(container).className).toBe(`${BUBBLE_CLS} opacity-70`);
    expect(bubbleOf(container).innerHTML).toBe('<p class="whitespace-pre-wrap break-words">你好</p>');
    const status = wrap.lastElementChild as HTMLElement;
    expect(status.textContent).toBe("发送中");
    expect(status.className).toContain("text-faint");
    expect(status.className).toContain("text-[11px]");
    expect(screen.queryByRole("button", { name: "重发这条消息" })).toBeNull();
    expect(onResend).not.toHaveBeenCalled();
  });

  it("unconfirmed：气泡不透明，下一行「未确认送达」（警示色）+「重发」按钮；点击把消息文字交给 onResend", () => {
    const onResend = vi.fn();
    const { container } = render(<MessageList entries={[echo({ ack: "unconfirmed" })]} onResend={onResend} />);
    const wrap = wrapOf(container);
    expect(bubbleOf(container).className).toBe(BUBBLE_CLS);
    const status = wrap.lastElementChild as HTMLElement;
    expect(status.className).toContain("text-warn");
    expect(within(status).getByText("未确认送达")).toBeTruthy();
    const btn = within(status).getByRole("button", { name: "重发这条消息" });
    expect(btn.textContent).toBe("重发");
    fireEvent.click(btn);
    expect(onResend).toHaveBeenCalledTimes(1);
    expect(onResend).toHaveBeenCalledWith("你好");
  });

  it("failed：下一行「未送达：{网关给的原因}」（错误色）+「重发」按钮", () => {
    const onResend = vi.fn();
    const { container } = render(
      <MessageList entries={[echo({ ack: "failed", ackReason: "排队消息过多，这条被丢弃" })]} onResend={onResend} />,
    );
    const wrap = wrapOf(container);
    expect(bubbleOf(container).className).toBe(BUBBLE_CLS);
    const status = wrap.lastElementChild as HTMLElement;
    expect(status.className).toContain("text-err");
    expect(status.className).not.toContain("text-warn");
    expect(within(status).getByText("未送达：排队消息过多，这条被丢弃")).toBeTruthy();
    fireEvent.click(within(status).getByRole("button", { name: "重发这条消息" }));
    expect(onResend).toHaveBeenCalledWith("你好");
  });

  it("没提供 onResend：状态文字照常显示，但不渲染重发按钮", () => {
    render(
      <MessageList entries={[echo({ ack: "unconfirmed" }), echo({ key: "user:m_def_00000000", text: "第二条", ack: "failed", ackReason: "消息格式无效" })]} />,
    );
    expect(screen.getByText("未确认送达")).toBeTruthy();
    expect(screen.getByText("未送达：消息格式无效")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "重发这条消息" })).toBeNull();
  });

  it("带图的消息（image）：回显里只有占位文本、图片本体已不在——显示状态但不给重发按钮", () => {
    const onResend = vi.fn();
    render(
      <MessageList entries={[echo({ text: "[图片]", ack: "failed", ackReason: "消息格式无效", image: true })]} onResend={onResend} />,
    );
    expect(screen.getByText("未送达：消息格式无效")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "重发这条消息" })).toBeNull();
    expect(onResend).not.toHaveBeenCalled();
  });

  it("rerender：pending → sent 状态行消失、DOM 回到基线；sent → failed 出现原因与按钮；failed → sent 再消失", () => {
    const base: UserEntry = { kind: "user", key: "user:m_abc_00000000", text: "你好", clientId: "m_abc_00000000" };
    const { rerender, container } = render(<MessageList entries={[{ ...base, ack: "pending" }]} onResend={() => {}} />);
    expect(screen.getByText("发送中")).toBeTruthy();
    rerender(<MessageList entries={[{ ...base, ack: "sent" }]} onResend={() => {}} />);
    expect(wrapOf(container).outerHTML).toBe(BASELINE_HTML);
    rerender(
      <MessageList entries={[{ ...base, ack: "failed", ackReason: "发送者未获授权，消息未被处理" }]} onResend={() => {}} />,
    );
    expect(screen.getByText("未送达：发送者未获授权，消息未被处理")).toBeTruthy();
    expect(screen.getByRole("button", { name: "重发这条消息" })).toBeTruthy();
    rerender(<MessageList entries={[{ ...base, ack: "sent" }]} onResend={() => {}} />);
    expect(wrapOf(container).outerHTML).toBe(BASELINE_HTML);
  });
});
