// CardView 终态收敛（真机反馈回归锁）：
// - Rust core finalize 的终态 card_patch 保留最后 chunk 的 phase（对飞书平台
//   有既有语义，协议不改），SPA 侧终态（state=done）不得再渲染头部 phase
//   徽章——终态语义由 footer 已完成/出错承载，否则「已完成 + 输出中」并存；
// - 完成工具的时间线终态整块折叠为一行摘要按钮（流式行为不变）。
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { CardView } from "./CardView";
import type { CardEntry, CardPayload } from "../lib/protocol";

function makeCard(overrides: Partial<CardPayload> = {}): CardPayload {
  return {
    text: "",
    tool_calls: [],
    thoughts: [],
    todos: [],
    phase: "outputting",
    run_secs: 3,
    terminal: "done",
    ...overrides,
  };
}

function makeEntry(card: CardPayload, state: "streaming" | "done"): CardEntry {
  return { kind: "card", key: "card:test:m1", messageId: "m1", card, state };
}

// vitest 未开 globals：RTL 不会自动注册 cleanup，须手动
afterEach(() => {
  cleanup();
});

describe("CardView 终态收敛", () => {
  it("流式态：渲染「输出中」徽章，时间线展开（运行项 aria-current），正文带流式光标", () => {
    const { container } = render(
      <CardView
        entry={makeEntry(
          makeCard({
            text: "流式正文",
            phase: "outputting",
            tool_calls: [
              { name: "read_file", summary: "读取配置", done: true },
              { name: "edit_file", summary: "修改配置", done: false },
            ],
          }),
          "streaming",
        )}
      />,
    );
    expect(screen.getByText("输出中")).toBeTruthy();
    // 时间线展开：运行中项高亮（aria-current=step），完成项单行可见
    expect(container.querySelector('li[aria-current="step"]')).toBeTruthy();
    expect(screen.getByText("read_file")).toBeTruthy();
    // 正文末尾流式光标 ▍
    expect(container.querySelector('[aria-label="生成中"]')).toBeTruthy();
  });

  it("终态 done：phase 残留 outputting 也不渲染徽章；时间线整块折叠，点击展开/收起", () => {
    const { container } = render(
      <CardView
        entry={makeEntry(
          makeCard({
            text: "最终回复",
            phase: "outputting", // 复现真机：finalize 终态帧 phase 不重置
            tool_calls: [
              { name: "read_file", summary: "读取配置", done: true },
              { name: "edit_file", summary: "修改配置", done: true },
            ],
          }),
          "done",
        )}
      />,
    );
    // phase 徽章不再出现，终态由 footer「已完成」承载；复制按钮终态保留
    expect(screen.queryByText("输出中")).toBeNull();
    expect(screen.getByText("已完成")).toBeTruthy();
    expect(screen.getByRole("button", { name: "复制整段" })).toBeTruthy();
    // 时间线默认折叠为一行摘要：无运行项、无流式光标、工具名不可见
    expect(screen.getByText("执行了 2 个工具")).toBeTruthy();
    expect(container.querySelector('li[aria-current="step"]')).toBeNull();
    expect(container.querySelector('[aria-label="生成中"]')).toBeNull();
    expect(screen.queryByText("read_file")).toBeNull();
    // 点击展开：完整时间线可见；再点收起
    fireEvent.click(screen.getByText("执行了 2 个工具"));
    expect(screen.getByText("read_file")).toBeTruthy();
    expect(screen.getByText("edit_file")).toBeTruthy();
    fireEvent.click(screen.getByText("执行了 2 个工具"));
    expect(screen.queryByText("read_file")).toBeNull();
  });

  it("终态 error 带未完成工具：折叠行用 ⚠️ 标注未完成，点击展开可见工具名", () => {
    render(
      <CardView
        entry={makeEntry(
          makeCard({
            text: "中途出错",
            terminal: "error",
            error: "工具执行失败",
            tool_calls: [
              { name: "bash", summary: "跑构建", done: true },
              { name: "write_file", summary: "写产物", done: false },
            ],
          }),
          "done",
        )}
      />,
    );
    const collapsed = screen.getByText("2 个工具调用 · 1 个未完成");
    // 存在未完成 → ⚠️ 前缀（不用 ✓）
    expect(collapsed.closest("button")?.textContent).toContain("⚠️");
    expect(collapsed.closest("button")?.textContent).not.toContain("执行了");
    fireEvent.click(collapsed);
    expect(screen.getByText("bash")).toBeTruthy();
    expect(screen.getByText("write_file")).toBeTruthy();
  });

  it("工具列表为空：流式与终态均不渲染时间线（无 ol、无摘要按钮）", () => {
    const { container, rerender } = render(
      <CardView entry={makeEntry(makeCard({ text: "纯文本回复" }), "streaming")} />,
    );
    expect(container.querySelector('[aria-label="执行链"]')).toBeNull();
    rerender(<CardView entry={makeEntry(makeCard({ text: "纯文本回复" }), "done")} />);
    expect(container.querySelector('[aria-label="执行链"]')).toBeNull();
    expect(screen.queryByText(/个工具/)).toBeNull();
  });
});
