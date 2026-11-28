// MessageList 空态（v1.43 emptyHint）：窗口会话（peer:）首次打开还没有任何消息时，
// 用一段说明替代通用示例 prompt（示例是通用任务，发给一个本地窗口没有意义）。
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
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
