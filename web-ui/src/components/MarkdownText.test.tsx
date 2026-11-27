// MarkdownText 消毒与代码块（输出层 rehype-sanitize 方案的安全回归）：
// - 威胁模型：卡片/文本内容来自模型输出，XSS 载荷目标是 localStorage 里的凭证；
// - 旧方案（源文本层 DOMPurify 剥全部标签）会毁掉代码块里的 HTML 示例——
//   本文件锁死该回归：代码块内 HTML 标签文本必须原样保留；
// - 双层防线：react-markdown 无 raw HTML（源里的 <script> 直接丢弃）+
//   rehype-sanitize 输出层兜底（未知标签/属性/on* 事件/危险协议剥离）。
// GFM（表格 / 任务列表 / 删除线 / 自动链接 / 脚注）的功能、不误伤，以及安全红线
// 不因接入 GFM 而放松，见下方 GFM 各组用例。
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MarkdownText } from "./MarkdownText";

// vitest 未开 globals：RTL 不会自动注册 cleanup，须手动
afterEach(() => {
  cleanup();
  // 还原单测注入的实例级 clipboard shadow
  delete (navigator as { clipboard?: unknown }).clipboard;
  // 还原 console 侦测（React 运行时告警断言用）
  vi.restoreAllMocks();
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

describe("MarkdownText GFM 表格", () => {
  it("表格：表头与单元格结构、文字正确，外层包横滚容器", () => {
    const { container } = render(
      <MarkdownText source={"| 名称 | 数量 |\n| --- | --- |\n| 苹果 | 3 |\n| 香蕉 | 5 |"} />,
    );
    const wrap = container.querySelector(".md-table-wrap");
    expect(wrap).toBeTruthy();
    // 容器是 .md 的直接子元素：首末子元素外边距归零规则靠它命中
    expect(wrap?.parentElement).toBe(container.querySelector(".md"));
    const table = wrap?.querySelector("table");
    expect(
      Array.from(table?.querySelectorAll("thead > tr > th") ?? []).map((th) => th.textContent),
    ).toEqual(["名称", "数量"]);
    const rows = Array.from(table?.querySelectorAll("tbody > tr") ?? []).map((tr) =>
      Array.from(tr.querySelectorAll("td")).map((td) => td.textContent),
    );
    expect(rows).toEqual([
      ["苹果", "3"],
      ["香蕉", "5"],
    ]);
    // 不再是竖线原文
    expect(container.textContent).not.toContain("|");
  });

  it("表格对齐标记：GFM 的对齐保留到表头与单元格（渲染层转成内联 text-align）", () => {
    const { container } = render(
      <MarkdownText source={"| 左 | 中 | 右 |\n| --- | :-: | --: |\n| 1 | 2 | 3 |"} />,
    );
    const aligns = (selector: string) =>
      Array.from(container.querySelectorAll<HTMLElement>(selector)).map(
        (el) => el.style.textAlign,
      );
    expect(aligns("thead th")).toEqual(["", "center", "right"]);
    expect(aligns("tbody td")).toEqual(["", "center", "right"]);
  });

  it("无分隔行 / 列数不匹配的竖线文本仍是普通段落（不误判为表格）", () => {
    const { container } = render(
      <MarkdownText source={"| 仅竖线 | 无分隔行 |\n下一行\n\n| a | b |\n| --- |\n| 1 |"} />,
    );
    expect(container.querySelector("table")).toBeNull();
    expect(container.textContent).toContain("| 仅竖线 | 无分隔行 |");
    expect(container.textContent).toContain("| a | b |");
  });
});

describe("MarkdownText GFM 任务列表", () => {
  const TASKS = "- [x] 已完成\n- [ ] 未完成\n- 普通项";

  it("每个任务项一个 disabled 复选框，勾选状态与源文一致；普通项不受影响", () => {
    const { container } = render(<MarkdownText source={TASKS} />);
    const boxes = Array.from(
      container.querySelectorAll<HTMLInputElement>('input[type="checkbox"]'),
    );
    expect(boxes).toHaveLength(2);
    // sanitize 兜底：恒为 disabled，用户无法切换（纯展示）
    expect(boxes.every((box) => box.disabled)).toBe(true);
    expect(boxes.map((box) => box.checked)).toEqual([true, false]);
    // 复选框归属的任务项与源文对应（GFM 在复选框后补一个空格文本节点）
    expect(boxes[0].closest("li")?.textContent).toBe(" 已完成");
    expect(boxes[1].closest("li")?.textContent).toBe(" 未完成");
    const items = container.querySelectorAll("li");
    expect(items).toHaveLength(3);
    expect(items[2].textContent).toBe("普通项");
  });

  it("复选框只读展示：渲染过程无 React 受控输入告警", () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    render(<MarkdownText source={TASKS} />);
    // sanitize 强制的 disabled 已抑制 React 的受控告警（其条件含 disabled），无需
    // onChange / readOnly；一旦 disabled 不再被强制，这里会第一时间转红
    expect(error).not.toHaveBeenCalled();
  });
});

describe("MarkdownText GFM 删除线与自动链接", () => {
  it("删除线 ~~旧~~ 渲染为 del", () => {
    const { container } = render(<MarkdownText source={"新版替代~~旧版~~"} />);
    expect(container.querySelector("del")?.textContent).toBe("旧版");
    expect(container.textContent).toBe("新版替代旧版");
  });

  it("自动链接 www.example.com：可点、地址以 http 开头、新开页 + noopener", () => {
    const { container } = render(<MarkdownText source={"见 www.example.com 了解"} />);
    const link = container.querySelector("a");
    expect(link?.textContent).toBe("www.example.com");
    expect(link?.getAttribute("href")).toMatch(/^http/i);
    expect(link?.getAttribute("target")).toBe("_blank");
    expect(link?.getAttribute("rel")?.split(" ")).toEqual(
      expect.arrayContaining(["noopener", "noreferrer", "nofollow"]),
    );
  });

  it("自动链接产出的链接同样走链接策略：mailto / 邮箱 / 页内锚点降级为纯文本", () => {
    const { container } = render(
      <MarkdownText
        source={
          "<foo@example.com>、bar@baz.com、https://example.org/x\n\n正文[^1]\n\n[^1]: 脚注"
        }
      />,
    );
    const links = container.querySelectorAll("a");
    // 仅 http(s) 存活：尖括号邮箱、裸邮箱、脚注引用与回链（页内锚点）全部降级
    expect(links).toHaveLength(1);
    expect(links[0].getAttribute("href")).toBe("https://example.org/x");
    expect(container.textContent).toContain("foo@example.com、bar@baz.com、https://example.org/x");
    expect(container.textContent).toContain("脚注");
  });

  it("自动链接后紧跟中文标点：链接在标点前截止，后续文字不被吞进链接", () => {
    const { container } = render(
      <MarkdownText source={"见 https://example.com/docs，另外 www.example.org。完"} />,
    );
    const links = Array.from(container.querySelectorAll("a"));
    expect(links.map((a) => a.getAttribute("href"))).toEqual([
      "https://example.com/docs",
      "http://www.example.org",
    ]);
    expect(links.map((a) => a.textContent)).toEqual([
      "https://example.com/docs",
      "www.example.org",
    ]);
    // 文字零丢失，且被截出的部分落在链接之外
    expect(container.querySelector("p")?.textContent).toBe(
      "见 https://example.com/docs，另外 www.example.org。完",
    );
    expect(links[0].nextSibling?.textContent).toBe("，另外 ");
    expect(links[1].nextSibling?.textContent).toBe("。完");
  });

  it("自动链接修正不误伤：中文路径与显式链接保持完整", () => {
    const { container } = render(
      <MarkdownText
        source={"[文档](https://example.com/a，b) 与 https://zh.wikipedia.org/wiki/中文"}
      />,
    );
    const links = Array.from(container.querySelectorAll("a"));
    expect(links).toHaveLength(2);
    // 显式链接：地址原样（哪怕含全角标点——那是作者的本意）
    expect(links[0].textContent).toBe("文档");
    expect(links[0].getAttribute("href")).toBe(
      `https://example.com/a${encodeURIComponent("，")}b`,
    );
    // 非 ASCII 字母数字属于地址，不截
    expect(links[1].getAttribute("href")).toBe(
      `https://zh.wikipedia.org/wiki/${encodeURIComponent("中文")}`,
    );
    expect(links[1].textContent).toBe("https://zh.wikipedia.org/wiki/中文");
  });
});

describe("MarkdownText 删除线只认双波浪线（GFM 关闭单波浪线）", () => {
  // 中文里「10~20 秒，最多 30~40 秒」「3~5 个」这类范围写法很常见：两个 ~ 内侧
  // 都紧贴内容就会配成一对删除线，两个范围之间的正文被整段划掉。故关闭
  // singleTilde，只保留 ~~文字~~；用单个 ~ 表达删除线的意图很少见，牺牲得起。
  it("范围写法「10~20 秒，最多 30~40 秒」「3~5 个」不出现 del，原文完整", () => {
    const lines = ["10~20 秒，最多 30~40 秒", "3~5 个"];
    const { container } = render(<MarkdownText source={lines.join("\n\n")} />);
    expect(container.querySelector("del")).toBeNull();
    expect(Array.from(container.querySelectorAll("p")).map((p) => p.textContent)).toEqual(lines);
  });

  it("~~旧方案~~ 仍渲染为 del", () => {
    const { container } = render(<MarkdownText source={"~~旧方案~~ 已被新方案取代"} />);
    expect(container.querySelector("del")?.textContent).toBe("旧方案");
    expect(container.textContent).toBe("旧方案 已被新方案取代");
  });

  it("单独一个 ~（耗时 ~3s）保持原文", () => {
    const { container } = render(<MarkdownText source={"耗时 ~3s"} />);
    expect(container.querySelector("del")).toBeNull();
    expect(container.textContent).toBe("耗时 ~3s");
  });
});

describe("MarkdownText GFM 不误伤", () => {
  it("围栏代码块里的表格 / 任务列表 / 删除线 / 自动链接语法保持原样", () => {
    const { container } = render(
      <MarkdownText source={"```\n| a | b |\n- [ ] x\n~~y~~\nwww.example.com\n```"} />,
    );
    expect(container.querySelector("table, del, input, a")).toBeNull();
    expect(container.querySelector("pre")?.textContent).toBe(
      "| a | b |\n- [ ] x\n~~y~~\nwww.example.com\n",
    );
  });

  it("行内代码里的 ~x~ 与 [^1] 保持原样", () => {
    const { container } = render(<MarkdownText source={"`~x~` 与 `[^1]`"} />);
    expect(container.querySelector("del, sup, section")).toBeNull();
    expect(Array.from(container.querySelectorAll("code")).map((c) => c.textContent)).toEqual([
      "~x~",
      "[^1]",
    ]);
  });

  it("「耗时 ~3s，误差 ~5%」不被当成删除线", () => {
    const { container } = render(<MarkdownText source={"耗时 ~3s，误差 ~5%"} />);
    expect(container.querySelector("del")).toBeNull();
    expect(container.textContent).toBe("耗时 ~3s，误差 ~5%");
  });

  it("无定义的 [^x] 不被当成脚注", () => {
    const { container } = render(<MarkdownText source={"正则 [^x] 表示排除"} />);
    expect(container.querySelector("sup, section")).toBeNull();
    expect(container.textContent).toBe("正则 [^x] 表示排除");
  });
});

describe("MarkdownText GFM 消毒（安全红线不因 GFM 放松）", () => {
  it("表格单元格里的原始 HTML 不渲染，表格结构不受影响", () => {
    const { container } = render(
      <MarkdownText
        source={
          '| a | b |\n| --- | --- |\n| <img src=x onerror="alert(1)"> | <script>alert(2)</script> |'
        }
      />,
    );
    expect(container.querySelector("img, script, iframe")).toBeNull();
    for (const el of Array.from(container.querySelectorAll("*"))) {
      expect(el.getAttribute("onerror")).toBeNull();
    }
    expect(container.querySelectorAll("tbody td")).toHaveLength(2);
  });

  it("任务列表项里的原始 HTML 不渲染，勾选状态不受影响", () => {
    const { container } = render(
      <MarkdownText
        source={'- [x] ok <script>alert(1)</script> <iframe src="https://evil.example"></iframe>'}
      />,
    );
    expect(container.querySelector("script, iframe")).toBeNull();
    const box = container.querySelector<HTMLInputElement>('input[type="checkbox"]');
    expect(box?.disabled).toBe(true);
    expect(box?.checked).toBe(true);
    const item = box?.closest("li");
    expect(item?.textContent).toContain("ok");
    // script 标签已丢弃，内容按既有语义落为文本
    expect(item?.textContent).toContain("alert(1)");
  });

  it("任务列表项里塞其它表单元素：只有 checkbox 存活，其余不渲染", () => {
    const { container } = render(
      <MarkdownText
        source={
          '- [x] <select><option>a</option></select><button onclick="alert(1)">b</button><textarea>c</textarea><form action="https://evil.example"></form>'
        }
      />,
    );
    expect(container.querySelector("select, button, textarea, form, option")).toBeNull();
    const boxes = container.querySelectorAll("input");
    // 唯一放行的表单元素是任务列表语法自己产生的那个 checkbox
    expect(boxes).toHaveLength(1);
    expect(boxes[0].getAttribute("type")).toBe("checkbox");
    // 表单元素的文字内容不丢（标签本身被剥）
    expect(container.querySelector("li")?.textContent).toContain("abc");
  });

  it("脚注里的 javascript: 链接与原始 HTML 同样被消毒", () => {
    const { container } = render(
      <MarkdownText
        source={"正文[^1]\n\n[^1]: [点我](javascript:alert(1)) <img src=x onerror=alert(2)>"}
      />,
    );
    expect(container.querySelector("a, img")).toBeNull();
    expect(container.querySelector("section.footnotes")?.textContent).toContain("点我");
  });
});
