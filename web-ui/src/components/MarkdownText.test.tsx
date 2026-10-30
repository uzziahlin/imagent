// MarkdownText 消毒与代码块（输出层 rehype-sanitize 方案的安全回归）：
// - 威胁模型：卡片/文本内容来自模型输出，XSS 载荷目标是 localStorage 里的凭证；
// - 旧方案（源文本层 DOMPurify 剥全部标签）会毁掉代码块里的 HTML 示例——
//   本文件锁死该回归：代码块内 HTML 标签文本必须原样保留；
// - 双层防线：react-markdown 无 raw HTML（源里的 <script> 直接丢弃）+
//   rehype-sanitize 输出层兜底（未知标签/属性/on* 事件/危险协议剥离）。
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MarkdownText } from "./MarkdownText";

// vitest 未开 globals：RTL 不会自动注册 cleanup，须手动
afterEach(() => {
  cleanup();
  // 还原单测注入的实例级 clipboard shadow
  delete (navigator as { clipboard?: unknown }).clipboard;
});

describe("MarkdownText 消毒（安全红线）", () => {
  it("内联 <script> 载荷：渲染结果无 script 元素，正文文字保留", () => {
    const { container } = render(
      <MarkdownText source={"你好 <script>alert(1)</script> 世界"} />,
    );
    expect(container.querySelector("script")).toBeNull();
    expect(container.textContent).toContain("你好");
    expect(container.textContent).toContain("世界");
  });

  it("多形态 XSS 载荷（img onerror / javascript: 链接 / iframe）均不产生可执行节点", () => {
    const { container } = render(
      <MarkdownText
        source={
          '<img src=x onerror="alert(1)">\n[点我](javascript:alert(2))\n<iframe src="https://evil.example"></iframe>'
        }
      />,
    );
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("iframe")).toBeNull();
    // onerror 属性不得出现在任何元素上
    for (const el of Array.from(container.querySelectorAll("*"))) {
      expect(el.getAttribute("onerror")).toBeNull();
    }
    // javascript: 链接被 a 组件降级为纯文本 span
    const links = container.querySelectorAll("a");
    for (const a of Array.from(links)) {
      expect(a.getAttribute("href")?.toLowerCase()).not.toContain("javascript:");
    }
  });

  it("代码块内 HTML 示例：标签文本原样保留（源文本剥标签方案的回归锁）", () => {
    const { container } = render(
      <MarkdownText source={"```html\n<div id=\"demo\">hi</div>\n<span>x</span>\n```"} />,
    );
    expect(container.querySelector("script")).toBeNull();
    // code 块内是 text 子节点，不经标签消毒——HTML 示例完整
    expect(container.textContent).toContain('<div id="demo">hi</div>');
    expect(container.textContent).toContain("<span>x</span>");
  });

  it("普通 markdown（行内代码/加粗/列表）正常渲染", () => {
    const { container } = render(<MarkdownText source={"`code` **粗体**\n\n- 项目一"} />);
    expect(container.querySelector("code")).toBeTruthy();
    expect(container.querySelector("strong")).toBeTruthy();
    expect(container.querySelector("li")?.textContent).toBe("项目一");
  });
});

describe("MarkdownText 代码块升级", () => {
  it("代码块：语言标签 + 复制按钮 + 内容渲染", () => {
    const { container } = render(<MarkdownText source={"```ts\nconst a: number = 1;\n```"} />);
    // 语言标签（CSS uppercase 不改文本内容）
    expect(screen.getByText("ts")).toBeTruthy();
    expect(screen.getByRole("button", { name: "复制代码" })).toBeTruthy();
    // 高亮会把代码拆进多个 token span，整段内容对 pre 的 textContent 断言
    expect(container.querySelector("pre")?.textContent).toBe("const a: number = 1;\n");
  });

  it("无语言标注的代码块显示 text 标签", () => {
    render(<MarkdownText source={"```\nplain code\n```"} />);
    expect(screen.getByText("text")).toBeTruthy();
  });

  it("复制按钮：点击写入原始代码（剥尾换行）并短暂反馈「已复制」", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    render(<MarkdownText source={"```js\nlet x = 1;\n```"} />);
    fireEvent.click(screen.getByRole("button", { name: "复制代码" }));
    await waitFor(() => expect(writeText).toHaveBeenCalledTimes(1));
    // 尾换行是围栏语法产物，剥掉
    expect(writeText).toHaveBeenCalledWith("let x = 1;");
    await waitFor(() => expect(screen.getByText("已复制")).toBeTruthy());
  });

  it("语法高亮：token span 带 hljs-* class 且经 sanitize 白名单保留", () => {
    const { container } = render(
      <MarkdownText source={"```ts\nconst x = \"s\";\n```"} />,
    );
    const code = container.querySelector("pre code");
    expect(code).toBeTruthy();
    // code 自身带 hljs / language-ts class
    expect(code?.className).toContain("hljs");
    expect(code?.className).toContain("language-ts");
    // 关键字被 rehype-highlight 包成 hljs-keyword span（未被 sanitize 剥 class）
    expect(code?.querySelector("span.hljs-keyword")).toBeTruthy();
  });
});
