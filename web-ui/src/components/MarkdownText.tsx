import DOMPurify from "dompurify";
import ReactMarkdown, { type Components } from "react-markdown";

/**
 * markdown 源文本清洗（安全红线：所有 markdown 渲染必经 DOMPurify）。
 *
 * 威胁模型：卡片/文本内容来自模型输出，可能内嵌 HTML/XSS 载荷——目标是从
 * localStorage 偷 USER_TOKEN。双层防线：
 * 1. 本函数在**源文本层**剥离一切 HTML 标签（ALLOWED_TAGS 空 + KEEP_CONTENT
 *    保留标签内文字，script/style 等危险内容整段删除）；
 * 2. react-markdown 默认不渲染原始 HTML（未接 rehype-raw），标签只会被转义
 *    显示。
 */
export function sanitizeMarkdown(src: string): string {
  return DOMPurify.sanitize(src, { ALLOWED_TAGS: [], ALLOWED_ATTR: [], KEEP_CONTENT: true });
}

/** 链接安全覆盖：仅放行 http(s)，强制新开页 + noopener（防 window.opener 劫持）。 */
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
};

/**
 * 所有 markdown 渲染的唯一入口（App 内不要绕过它直接用 react-markdown）。
 * 排版样式见 index.css 的 `.md` 命名空间；M3b 富渲染（代码高亮等）在此组件
 * 内扩展 components 映射即可。
 */
export function MarkdownText({ source }: { source: string }) {
  return (
    <div className="md">
      <ReactMarkdown components={components}>{sanitizeMarkdown(source)}</ReactMarkdown>
    </div>
  );
}
