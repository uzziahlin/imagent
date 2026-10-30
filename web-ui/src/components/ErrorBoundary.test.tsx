// ErrorBoundary（顶层渲染错误边界）：子树渲染抛错 → 可读错误页（错误摘要 +
// 重试按钮），不再静默白屏；正常子树原样透传。
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ErrorBoundary } from "./ErrorBoundary";

function Boom(): never {
  throw new Error("渲染崩溃模拟：undefined.convOrder");
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ErrorBoundary", () => {
  it("子树正常时原样渲染 children", () => {
    render(
      <ErrorBoundary>
        <p>正常内容</p>
      </ErrorBoundary>,
    );
    expect(screen.getByText("正常内容")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("子树渲染抛错时显示错误页（摘要 + 重试），children 不再渲染", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <ErrorBoundary>
        <Boom />
      </ErrorBoundary>,
    );
    expect(screen.getByRole("alert")).toBeTruthy();
    expect(screen.getByText("页面渲染出错")).toBeTruthy();
    expect(screen.getByText(/渲染崩溃模拟/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "重试" })).toBeTruthy();
    expect(screen.queryByText("正常内容")).toBeNull();
  });
});
