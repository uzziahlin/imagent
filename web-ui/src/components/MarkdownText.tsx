import type { ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import rehypeSanitize, { defaultSchema, type Options } from "rehype-sanitize";
import type { Element, ElementContent } from "hast";
import { CopyButton } from "./CopyButton";

/**
 * 消毒方案（安全红线：卡片/文本内容来自模型输出，可能内嵌 XSS 载荷，目标是
 * 从 localStorage 偷 USER_TOKEN）——输出层 rehype-sanitize，双层防线：
 *
 * 1. react-markdown 默认不渲染原始 HTML（未接 rehype-raw）：markdown 源里的
 *    `<script>…</script>` 等原始 HTML 节点在 mdast→hast 阶段被直接丢弃，
 *    到不了渲染层；
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
 * 插件次序敏感：rehype-highlight（打 token class）在前，rehype-sanitize
 * （按白名单保 class）在后——注意 rehype-sanitize 的插件选项就是 schema
 * 本身（非 {schema} 包装）。排版与高亮 token 主题见 index.css 的 `.md` 命名空间。
 */
export function MarkdownText({ source }: { source: string }) {
  return (
    <div className="md">
      <ReactMarkdown
        rehypePlugins={[rehypeHighlight, [rehypeSanitize, sanitizeSchema]]}
        components={components}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}
