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

// 执行链 diff 渲染（v1.39）：文件修改类工具（claude Edit/MultiEdit/Write、
// codex fileChange）完成条目带 unified 纯文本 diff——徽标 +n/−m、单条目默认
// 收起、展开着色渲染、超 400 行截断；无 diff 条目渲染零变化。
describe("CardView 执行链 diff 渲染", () => {
  const DIFF = [" context", "+added one", "-removed one", "+added two"].join("\n");

  it("终态：徽标统计正确（+2 −1）、默认收起；展开后 + 行 emerald 底、- 行 rose 底、其余素色，再点收起", () => {
    const { container } = render(
      <CardView
        entry={makeEntry(
          makeCard({ tool_calls: [{ name: "Edit", summary: "src/a.ts", done: true, diff: DIFF }] }),
          "done",
        )}
      />,
    );
    // 整块折叠语义不变：先展开块再见条目
    fireEvent.click(screen.getByText("执行了 1 个工具"));
    // 徽标数字正确；默认收起 diff 体不可见
    expect(screen.getByText("+2")).toBeTruthy();
    expect(screen.getByText("−1")).toBeTruthy();
    expect(screen.queryByText("+added one")).toBeNull();
    // 展开：+ 行 emerald 底、- 行 rose 底、context 行素色
    fireEvent.click(screen.getByText("Edit"));
    expect(screen.getByText("+added one")).toBeTruthy();
    expect(screen.getByText("-removed one")).toBeTruthy();
    const body = container.querySelector('[aria-label="变更内容"]') as HTMLElement;
    const lines = Array.from(body.children) as HTMLElement[];
    expect(lines).toHaveLength(4);
    expect(lines[0].className).not.toContain("bg-emerald-500/15");
    expect(lines[0].className).not.toContain("bg-rose-500/15");
    expect(lines[1].className).toContain("bg-emerald-500/15");
    expect(lines[2].className).toContain("bg-rose-500/15");
    expect(lines[3].className).toContain("bg-emerald-500/15");
    // 再点收起
    fireEvent.click(screen.getByText("Edit"));
    expect(screen.queryByText("+added one")).toBeNull();
    expect(container.querySelector('[aria-label="变更内容"]')).toBeNull();
  });

  it("混合条目：无 diff 条目零变化（原单行、无徽标、非按钮），带 diff 条目可展开", () => {
    const { container } = render(
      <CardView
        entry={makeEntry(
          makeCard({
            tool_calls: [
              { name: "read_file", summary: "读取配置", done: true },
              { name: "Edit", summary: "src/a.ts", done: true, diff: "+one\n-one" },
            ],
          }),
          "done",
        )}
      />,
    );
    fireEvent.click(screen.getByText("执行了 2 个工具"));
    expect(screen.getByText("read_file")).toBeTruthy();
    expect(screen.getByText("Edit")).toBeTruthy();
    expect(screen.getByText("+1")).toBeTruthy();
    expect(screen.getByText("−1")).toBeTruthy();
    // 默认收起：diff 行不可见
    expect(screen.queryByText("+one")).toBeNull();
    // 仅带 diff 条目有单条目折叠按钮（无 diff 条目仍是原单行 li）
    expect(container.querySelectorAll('[aria-label="执行链"] button[aria-expanded]')).toHaveLength(1);
    fireEvent.click(screen.getByText("Edit"));
    expect(screen.getByText("+one")).toBeTruthy();
    expect(screen.getByText("-one")).toBeTruthy();
  });

  it("超 400 行：徽标按全量统计（+405），展开仅渲染 400 行并显示尾部截断提示", () => {
    const diff = Array.from({ length: 405 }, (_, i) => `+line ${i}`).join("\n");
    const { container } = render(
      <CardView
        entry={makeEntry(
          makeCard({ tool_calls: [{ name: "Write", summary: "big.txt", done: true, diff }] }),
          "done",
        )}
      />,
    );
    fireEvent.click(screen.getByText("执行了 1 个工具"));
    // 徽标统计全量行数；无删除行不渲染 −m 侧
    expect(screen.getByText("+405")).toBeTruthy();
    expect(screen.queryByText(/−\d+/)).toBeNull();
    fireEvent.click(screen.getByText("Write"));
    expect(screen.getByText("…（已截断，共 405 行）")).toBeTruthy();
    const body = container.querySelector('[aria-label="变更内容"]') as HTMLElement;
    const lines = Array.from(body.children) as HTMLElement[];
    expect(lines).toHaveLength(401); // 400 行 + 1 截断提示
    expect(lines.filter((d) => d.className.includes("bg-emerald-500/15"))).toHaveLength(400);
  });

  it("流式：带 diff 完成条目徽标直接可见（时间线未整块折叠），点击展开 diff 体", () => {
    render(
      <CardView
        entry={makeEntry(
          makeCard({
            phase: "tool_running",
            tool_calls: [{ name: "Edit", summary: "a.ts", done: true, diff: "+新行\n-旧行" }],
          }),
          "streaming",
        )}
      />,
    );
    expect(screen.getByText("+1")).toBeTruthy();
    expect(screen.getByText("−1")).toBeTruthy();
    fireEvent.click(screen.getByText("Edit"));
    expect(screen.getByText("+新行")).toBeTruthy();
    expect(screen.getByText("-旧行")).toBeTruthy();
  });
});
