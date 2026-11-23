// lib/swStamp 单测（v1.41.7 构建戳纯函数）：
// - computeBuildStamp：定义以 sha256 标准向量钉死（前 16 位小写 hex）、同输入
//   同输出、index.html 字节变 → 戳变；
// - stampServiceWorkerSource：占位符全部替换且无残留、缺占位符抛错；
// - 提交产物一致性：仓库里已提交的 crates/relay/static/sw.js 的 BUILD 值必须
//   等于由同目录 index.html 算出的戳（防产物被手改 / 漏打戳）。
// 产物经 ?raw 读入——目录在 web-ui 之外，vite.config.ts 的 server.fs.allow 已
// 按目录放行；?raw 给的是 UTF-8 文本，TextEncoder 还原的字节与文件一致。
import { describe, expect, it } from "vitest";
import { SW_BUILD_PLACEHOLDER, computeBuildStamp, stampServiceWorkerSource } from "./swStamp";
import committedIndexHtml from "../../../crates/relay/static/index.html?raw";
import committedSwJs from "../../../crates/relay/static/sw.js?raw";

const bytes = (s: string) => new TextEncoder().encode(s);
const HTML = '<!doctype html>\n<html lang="zh-CN"><title>imagent · Web 驾驶舱</title>\n' +
  '<script type="module" crossorigin src="/imagent/assets/index-abc123.js"></script>\n</html>\n';

describe("computeBuildStamp", () => {
  it("定义：index.html 字节的 sha256 前 16 位小写 hex（标准向量钉死）", () => {
    // FIPS 180-4 向量：sha256("") = e3b0c44298fc1c14…、sha256("abc") = ba7816bf8f01cfea…
    expect(computeBuildStamp(bytes(""))).toBe("e3b0c44298fc1c14");
    expect(computeBuildStamp(bytes("abc"))).toBe("ba7816bf8f01cfea");
  });

  it("同输入同输出；形态为 16 位小写 hex", () => {
    const stamp = computeBuildStamp(bytes(HTML));
    expect(computeBuildStamp(bytes(HTML))).toBe(stamp);
    expect(stamp).toMatch(/^[0-9a-f]{16}$/);
  });

  it("index.html 变一个字节 → 戳变（含只改资产 hash 引用 / 只多一个换行）", () => {
    const base = computeBuildStamp(bytes(HTML));
    expect(computeBuildStamp(bytes(HTML.replace("abc123", "xyz789")))).not.toBe(base);
    expect(computeBuildStamp(bytes(HTML.replace("zh-CN", "en-US")))).not.toBe(base);
    expect(computeBuildStamp(bytes(`${HTML}\n`))).not.toBe(base);
  });

  it("按字节而非字符计：多字节字符取其 UTF-8 字节", () => {
    // "é" 一个字符两个字节（UTF-8：c3 a9）；戳与显式字节序列一致，且不同于
    // Latin-1 单字节形态
    const stamp = computeBuildStamp(bytes("é"));
    expect(stamp).toBe(computeBuildStamp(new Uint8Array([0xc3, 0xa9])));
    expect(stamp).not.toBe(computeBuildStamp(new Uint8Array([0xe9])));
  });
});

describe("stampServiceWorkerSource", () => {
  it("占位符全部替换且无残留（多处出现一并替换）", () => {
    const src = [
      `const BUILD = "${SW_BUILD_PLACEHOLDER}";`,
      `// 注释里的 ${SW_BUILD_PLACEHOLDER} 也一并替换`,
      `log("${SW_BUILD_PLACEHOLDER} ${SW_BUILD_PLACEHOLDER}");`,
    ].join("\n");
    const out = stampServiceWorkerSource(src, "0123456789abcdef");
    expect(out).not.toContain(SW_BUILD_PLACEHOLDER);
    expect(out.match(/0123456789abcdef/g)).toHaveLength(4);
    expect(out.split("\n")[0]).toBe('const BUILD = "0123456789abcdef";');
  });

  it("除占位符外逐字节原样（同输入同输出）", () => {
    const src = `/* 注释 */\nconst BUILD = "${SW_BUILD_PLACEHOLDER}";\nconst SHELL_CACHE = "imagent-shell-v1";\n`;
    const once = stampServiceWorkerSource(src, "deadbeefdeadbeef");
    expect(once).toBe(src.replace(SW_BUILD_PLACEHOLDER, "deadbeefdeadbeef"));
    expect(stampServiceWorkerSource(src, "deadbeefdeadbeef")).toBe(once);
    // 戳只进占位符：cache 名行不受影响
    expect(once).toContain('const SHELL_CACHE = "imagent-shell-v1";');
  });

  it("源文件缺占位符 → 抛错（信息指明占位符与后果）", () => {
    expect(() => stampServiceWorkerSource('const BUILD = "";\n', "0123456789abcdef")).toThrow(
      /sw\.js 源文件缺少 __SW_BUILD__ 占位符，客户端将永远无法感知新版本/,
    );
    // 已被打过戳的产物再打一次（占位符已不在）同样抛错——插件只认源形态
    const stamped = stampServiceWorkerSource(`const BUILD = "${SW_BUILD_PLACEHOLDER}";`, "0123456789abcdef");
    expect(() => stampServiceWorkerSource(stamped, "fedcba9876543210")).toThrow(/缺少 __SW_BUILD__ 占位符/);
  });
});

// ---------------------------------------------------------------------------
// 提交产物一致性：crates/relay/static 是随发版提交的构建产物（relay rust-embed
// 内嵌）。已提交的 sw.js 必须确实打上了「由同目录 index.html 算出的戳」——
// 产物被手改 / 漏打戳（例如绕过 vite build 直接拷文件）在此暴露。
// 注意本用例只约束两个产物之间的一致性：源码改了但尚未重新构建时，两个
// 产物仍是旧的一致对，本用例为绿（重新构建的提醒不属于本用例职责）。
// ---------------------------------------------------------------------------

describe("提交产物一致性（crates/relay/static）", () => {
  it("已提交 sw.js 的 BUILD 值 == 由已提交 index.html 算出的戳，且无占位符残留", () => {
    const build = /^const BUILD = "([^"]*)";$/m.exec(committedSwJs)?.[1];
    // 已打上实值（不是占位符、不是空串；整行缺失 = 产物早于打戳机制，未重新构建）
    expect(build, "已提交的 sw.js 缺少 BUILD 常量行（产物未重新构建？）").toBeDefined();
    expect(build).toMatch(/^[0-9a-f]{16}$/);
    expect(committedSwJs).not.toContain(SW_BUILD_PLACEHOLDER);
    expect(build).toBe(computeBuildStamp(bytes(committedIndexHtml)));
  });
});
