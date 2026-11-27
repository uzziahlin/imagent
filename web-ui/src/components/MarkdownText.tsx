import type { ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import rehypeSanitize, { defaultSchema, type Options } from "rehype-sanitize";
import remarkGfm from "remark-gfm";
import type { Element, ElementContent } from "hast";
import type { Link, Nodes, PhrasingContent, Root } from "mdast";
import { CopyButton } from "./CopyButton";

/**
 * 消毒方案（安全红线：卡片/文本内容来自模型输出，可能内嵌 XSS 载荷，目标是
 * 从 localStorage 偷 USER_TOKEN）——输出层 rehype-sanitize，双层防线：
 *
 * 1. react-markdown 不解析原始 HTML（未接 rehype-raw）：markdown 源里的
 *    `<script>…</script>` 等原始 HTML 在 mdast→hast 阶段只被包成不生成元素的
 *    raw 节点；本管线里它们在输出层被 rehype-sanitize 丢弃，到不了渲染层——
 *    即使没有 sanitize，react-markdown 也只会把 raw 输出成纯文本，不会执行；
 * 2. rehype-sanitize（GitHub 风格 defaultSchema）在 hast 输出层兜底：即使
 *    未来接入 rehype-raw 或插件次序调整，未知标签/属性/on* 事件/危险协议
 *    （javascript: 等）也会被剥离。
 *
 * 与旧方案（源文本层 DOMPurify ALLOWED_TAGS:[] 剥全部标签）的安全等价性：
 * 旧方案本质是「最终 DOM 无危险节点」，新方案两道防线达成同一不变量，且修复
 * 了旧方案的已知缺陷——源文本剥标签会把代码块里的 HTML 示例（```html
 * <div>…</div>）一并毁掉。新方案下代码块内容是 code 的 text 子节点，不经
 * 标签消毒，HTML 示例完整保留。
 *
 * schema 扩展：rehype-highlight 在 code/span 上添加 hljs/hljs-* token class
 * 与 language-* class；defaultSchema 默认只放行 code 的 language-*（span 的
 * className 未放行），且属性定义取首个匹配条目——故 code 处需「替换」默认
 * className 条目而非追加。仍只放行 hljs/language 前缀，不放开任意 class。
 */
/** className 白名单条目：language-* / hljs / hljs-*（rehype-highlight 产物）。 */
const HLJS_CLASS_NAMES: ["className", RegExp, RegExp, RegExp] = [
  "className",
  /^language-./,
  /^hljs$/,
  /^hljs-./,
];

// rehype-sanitize 的 Options 即 hast-util-sanitize 的 Schema（别名导出）
const sanitizeSchema: Options = {
  ...defaultSchema,
  attributes: {
    ...defaultSchema.attributes,
    code: [
      ...(defaultSchema.attributes?.code ?? []).filter(
        (d) => !(Array.isArray(d) && d[0] === "className"),
      ),
      HLJS_CLASS_NAMES,
    ],
    span: [...(defaultSchema.attributes?.span ?? []), HLJS_CLASS_NAMES],
  },
};

// GFM 产物无需扩展上面的 schema，defaultSchema 原生覆盖：删除线 / 表格系列标签
// 都在 tagNames 内；任务列表的 <input type="checkbox" checked disabled> 中，
// disabled 仅 true、type 仅 checkbox 由 input 条目限定并经 required 兜底补齐，
// checked 则由 '*' 通配条目放行——hast-util-sanitize 在标签专属定义未命中时
// 回退 '*'，而 required 只补缺不校验（不会补出 checked）。input 是唯一放行的
// 表单元素，且只能由任务列表语法产生（原始 HTML 不进 hast，用户无法控制其
// 属性）；disabled 已足够让它只读展示——React 的受控告警条件含 disabled，无需
// 再加 onChange / readOnly。

/** URL 之外的字符：非 ASCII 的标点 / 符号 / 空白（中文句读、全角括号等）。
 *  ASCII 可见字符与非 ASCII 字母数字（中文路径、国际化域名）都属于 URL。 */
const NON_URL_CHARS = /[^\x21-\x7e\p{L}\p{N}\p{M}]/u;

/**
 * 修正 GFM 自动链接字面量对中文的吞并。GFM 规定自动链接在域名之后可接任意
 * 「非空白」字符，只在末尾剥掉少数 ASCII 标点——URL 后紧跟全角标点时（如
 * 「见 https://a.com/x，另外…」），标点及其后整句都被吞进链接直到下一个空白，
 * 地址成了带中文标点的错误链接。接入 GFM 前这类文本是纯文本，属于回归。
 *
 * 做法：在 mdast 层（地址尚未做百分号编码）把自动链接在第一个 URL 之外的
 * 字符处截断，其后内容还原为普通文本——链接仍可点、文字零丢失。只认「链接
 * 文字即地址原文」的自动链接形态，显式链接（[文字](地址)）不动。截断点只取决于
 * 链接自身，流式输出下不会随补丁抖动。
 */
function remarkAutolinkBoundary() {
  return (tree: Root) => {
    cutAutolinks(tree);
  };
}

/** 就地截断一个自动链接，返回移出链接的文字（无需处理时返回 undefined）。 */
function cutAutolink(link: Link): string | undefined {
  const [text] = link.children;
  // www. 形态的地址带 http:// 前缀而链接文字不带，故用「地址以文字结尾」判定
  if (
    link.children.length !== 1 ||
    text.type !== "text" ||
    !text.value ||
    !link.url.endsWith(text.value)
  ) {
    return undefined;
  }
  const cut = link.url.search(NON_URL_CHARS);
  if (cut < 1) {
    return undefined;
  }
  const tail = link.url.slice(cut);
  link.url = link.url.slice(0, cut);
  text.value = text.value.slice(0, text.value.length - tail.length);
  return tail;
}

function cutAutolinks(node: Nodes): void {
  if (!("children" in node)) {
    return;
  }
  // 先子树后本层：截断会往 children 插入文本节点，不能边遍历边改
  for (const child of node.children) {
    cutAutolinks(child);
  }
  // link 只出现在行内容器里；块级容器的 children 不含行内节点，扫描自然落空
  const siblings = node.children as PhrasingContent[];
  for (let i = 0; i < siblings.length; i++) {
    const kid = siblings[i];
    if (kid.type !== "link") {
      continue;
    }
    const tail = cutAutolink(kid);
    if (tail === undefined) {
      continue;
    }
    const next = siblings[i + 1];
    if (next?.type === "text") {
      next.value = tail + next.value;
    } else {
      siblings.splice(i + 1, 0, { type: "text", value: tail });
    }
  }
}

/** 链接安全覆盖：仅放行 http(s)，强制新开页 + noopener（防 window.opener 劫持）。
 *  与 rehype-sanitize 的协议白名单（http/https/mailto/…）分层——此处更严：
 *  mailto 等也降级为纯文本。 */
const components: Components = {
  a: ({ href, children }) => {
    const safe = typeof href === "string" && /^https?:\/\//i.test(href) ? href : undefined;
    return safe ? (
      <a href={safe} target="_blank" rel="noopener noreferrer nofollow">
        {children}
      </a>
    ) : (
      <span>{children}</span>
    );
  },
  pre: ({ node, children }) => <CodeBlock node={node}>{children}</CodeBlock>,
  // 表格外包一层横滚容器：窄屏下宽表格在容器内横向滚动，不撑宽页面。GFM 的
  // 对齐标记落在 th / td 上（渲染层转成内联 text-align），表格自身无属性，故只
  // 取 children。
  // 已知且有意保留：下面这个键是 react-markdown 约定的 HTML 标签名，而 Tailwind
  // 按原始文本扫描 src，会把同名的显示类也生成进产物样式表（一条无人使用的
  // 死规则）。键名由库约定、无法改名；核对产物样式时它会稳定出现，不是回归。
  table: ({ children }) => (
    <div className="md-table-wrap">
      <table>{children}</table>
    </div>
  ),
};

/** hast 子树纯文本（代码块「复制」取原始代码：高亮 span 摊平拼接）。 */
function textOf(node: ElementContent | undefined): string {
  if (!node) {
    return "";
  }
  if (node.type === "text") {
    return node.value;
  }
  if (node.type === "element") {
    return node.children.map(textOf).join("");
  }
  return ""; // comment / raw 不构成代码文本
}

/** pre 直下 code 元素的 language-* class → 语言名（rehype-highlight 保序追加
 *  hljs class；sanitize 白名单内 language-* 存活）。 */
function langOf(code: Element | undefined): string | undefined {
  const cls = code?.properties?.className;
  if (!Array.isArray(cls)) {
    return undefined;
  }
  for (const c of cls) {
    if (typeof c === "string" && c.startsWith("language-")) {
      return c.slice("language-".length);
    }
  }
  return undefined;
}

/**
 * 代码块：头部（语言标签 + 复制按钮）+ 横滚 pre。复制内容为原始代码文本
 * （剥掉围栏语法产生的尾换行）；无语言标注显示 "text"。横滚样式见 index.css。
 */
function CodeBlock({ node, children }: { node?: Element; children?: ReactNode }) {
  const code = node?.children.find(
    (c): c is Element => c.type === "element" && c.tagName === "code",
  );
  const lang = langOf(code);
  const raw = textOf(code).replace(/\n$/, "");

  return (
    <div className="code-block my-2 overflow-hidden rounded-xl border border-line bg-code">
      <div className="flex items-center justify-between gap-2 border-b border-line px-3 py-1">
        <span className="font-mono text-[11px] uppercase tracking-wider text-ghost">
          {lang || "text"}
        </span>
        <CopyButton getText={() => raw} label="复制代码" />
      </div>
      <pre className="p-3">{children}</pre>
    </div>
  );
}

/**
 * 所有 markdown 渲染的唯一入口（App 内不要绕过它直接用 react-markdown）。
 * remark 层：remark-gfm 启用表格 / 任务列表 / 删除线 / 自动链接 / 脚注（同时
 * 启用，无法单独关闭），自动链接的中文修正须排在其后。rehype 层次序敏感：
 * rehype-highlight（打 token class）在前，rehype-sanitize（按白名单保 class）
 * 在后——注意 rehype-sanitize 的插件选项就是 schema 本身（非 {schema} 包装）。
 * 排版与高亮 token 主题见 index.css 的 `.md` 命名空间。
 */
export function MarkdownText({ source }: { source: string }) {
  return (
    <div className="md">
      {/* GFM 关闭单波浪线删除线（singleTilde: false），只保留 ~~文字~~：中文里
          「10~20 秒，最多 30~40 秒」「3~5 个」这类范围写法很常见——两个 ~ 内侧
          都紧贴内容就会配成一对，两个范围之间的正文被整段划掉；而用单个 ~ 表达
          删除线的意图很少见，牺牲得起。 */}
      <ReactMarkdown
        remarkPlugins={[[remarkGfm, { singleTilde: false }], remarkAutolinkBoundary]}
        rehypePlugins={[rehypeHighlight, [rehypeSanitize, sanitizeSchema]]}
        components={components}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}
